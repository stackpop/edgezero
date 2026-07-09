# EdgeZero Deploy GitHub Actions Implementation Plan

**Status:** Revised plan (layered, adapter-independent)

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

## Scope

Implement the layered deploy actions in the EdgeZero monorepo:

```text
.github/actions/build-cli
.github/actions/deploy-core
.github/actions/deploy-fastly
```

`build-cli` compiles the caller-selected EdgeZero CLI once and publishes it as an
artifact. `deploy-core` is the adapter-independent deploy engine that consumes
the prebuilt CLI. `deploy-fastly` is a minimal wrapper that types Fastly
credentials and calls the engine. Provider orchestration (build, deploy,
config push, provision) stays in the CLI.

The actions deploy already checked-out application source. They do not perform
checkout, staging, runtime config mutation as a deploy side effect, health
checks, rollback, or provider deployment-metadata parsing.

## Implementation phases

1. **`build-cli` action**
   - Required `cli-package` input: the Cargo package name of the CLI defined in
     the application's own workspace. Fail if missing or not found in the app
     workspace under `working-directory`.
   - `working-directory`, `rust-toolchain`, `artifact-name` inputs (no `adapters`
     input — the app's `Cargo.toml` pins adapters).
   - Optional `cli-bin` input (default = `cli-package`; the generated CLI names
     its bin after the package).
   - Require a `Cargo.lock` at the app's Cargo workspace root; run all Cargo
     commands with `--locked` (never mutate the lockfile). Validate via
     `cargo metadata --locked` that `cli-package` exists and declares a
     `<cli-bin>` binary target.
   - Install the host toolchain (no WASM target — the CLI is a native tool);
     build into an **action-owned `CARGO_TARGET_DIR` under `RUNNER_TEMP`** (never
     the checkout) so the CLI build leaves the app tree clean for the later
     dirty-source guard, via
     `cargo build --locked --release -p <cli-package> --bin <cli-bin>`. No
     `--features` injection: the app's own `Cargo.toml` pins its adapters.
   - Read `cli-version` from `cargo metadata`; smoke-check with `<cli-bin> --help`
     (today's CLI has no `--version`); write `cli-meta.json` (`cli-bin`,
     `cli-version`, `cli-package`) next to the binary and upload both as one
     **tar** so the executable bit survives `actions/upload-artifact` and the
     artifact is self-describing.
   - Outputs: `cli-version`, `cli-package`, `cli-bin`, `artifact-name`.
   - No provider credentials in scope. Never builds the EdgeZero monorepo CLI.

2. **`deploy-core` shared engine scripts (provider-neutral)**
   - A directory of scripts under `.github/actions/deploy-core/`, **not** a
     standalone composite action. Wrappers source them via
     `$GITHUB_ACTION_PATH/../deploy-core/…`.
   - Parameters (via env from the wrapper): `adapter`, `cli-artifact`, `cli-bin`,
     `working-directory`, `manifest`, `rust-toolchain`, `target`, `build-mode`,
     `build-args`, `deploy-args`, `deploy-arg-allow`, `provider-env`,
     `provider-env-clear`, `deploy-flags`, `cache`.
   - Download the CLI artifact (tar) under `RUNNER_TEMP`, extract preserving the
     executable bit (or `chmod +x <cli-bin>`), read `cli-meta.json` for
     `cli-bin`/`cli-version` (wrapper `cli-bin` overrides), and PATH-scope it.
   - Validate `adapter` well-formedness (no compiled-adapter enumeration — the
     CLI rejects unknown adapters itself), booleans, JSON arrays/object, NUL
     bytes.
   - Confine `working-directory` and `manifest` inside `github.workspace`
     (canonical paths, symlink resolution).
   - Resolve **Git root** for `source-revision` and the dirty-source guard
     (`build-cli`'s isolated target dir keeps this clean).
   - Resolve the **Cargo workspace root** (`cargo locate-project --workspace`)
     for lockfile hashing and `target/` caching — in a monorepo this may be under
     `working-directory`, not the Git root (spec §11.1).
   - Resolve Rust toolchain (explicit → Rustup files → `.tool-versions` → repo
     fallback) and application `target` (from `adapter` when `auto`).
   - Optional exact-key cache of the **Cargo workspace root** `target/`
     restore/save.
   - Resolve `build-mode`; optional credential-free build.
   - Non-deploy steps: unset the `provider-env-clear` names.
   - Deploy step: clear the `provider-env-clear` aliases, export only
     `provider-env`, then run
     `<cli-bin> deploy --adapter <adapter> -- <deploy-flags…> <deploy-args…>` via
     Bash arrays. Note the build-in-deploy caveat: Fastly's default `never`
     compiles the app during deploy with the token in scope, so require trusted
     immutable refs (spec §10.1).
   - Surface results to the wrapper: `adapter`, `source-revision`, `cli-version`,
     `effective-build-mode`.
   - Contains no provider-specific credential names, service concepts, endpoints,
     or CLI flags; invokes `<cli-bin>`, never a hard-coded `edgezero`.

3. **`deploy-fastly` wrapper (minimal composite action)**
   - Typed inputs: `cli-artifact`, `cli-bin`, `fastly-api-token`,
     `fastly-service-id`, plus forwarded `working-directory`, `manifest`,
     `build-mode`, `build-args`, `deploy-args`, `cache`.
   - Map `fastly-api-token` → `provider-env: {FASTLY_API_TOKEN: …}` and
     `fastly-service-id` → action-owned
     `deploy-flags: ["--service-id", …, "--non-interactive"]`.
   - Set `adapter: fastly`, `target: wasm32-wasip1`, `deploy-arg-allow` =
     `--comment` only, `provider-env-clear` = Fastly auth/endpoint aliases
     (`FASTLY_API_TOKEN`, `FASTLY_SERVICE_ID`, `FASTLY_ENDPOINT`, …), Fastly
     `auto` build-mode → `never`.
   - Source the shared `deploy-core` scripts; no build, toolchain, or path logic
     of its own.

4. **Scripts layout**
   - Provider-neutral scripts under `deploy-core/`; the Fastly install + checksum
     step lives with `deploy-fastly/` (or a shared script keyed by adapter).
   - No CLI-build script here — CLI build lives entirely in `build-cli`.

5. **CI workflow (`.github/workflows/deploy-action.yml`) — no Python**
   - Pin third-party actions to readable released tags (`actions/checkout@v4`,
     `actions/cache@v4`, artifact upload/download at released tags).
   - Run `actionlint` from a pinned release binary (no `go run @<commit>`).
   - Run `zizmor` from a pinned release binary or `cargo install zizmor --locked`
     (no `pip`).
   - Port the metadata-validation heredocs into `tests/run.sh`.
   - Composite smoke test: `build-cli` → `deploy-fastly` with fake provider
     binaries; assert CLI-artifact reuse and credential scoping.

6. **Bash contract tests (`tests/run.sh`)**
   - Cover engine + wrapper: adapter/boolean/JSON validation, path confinement,
     symlink escape, dirty source, toolchain precedence, cache keys, credential
     scoping, deploy-arg allowlist, build/deploy argv, cleanup, log redaction,
     and metadata contract checks.
   - No Python; no live provider credentials.

7. **Companion CLI change**
   - Add `#[command(version)]` to the downstream CLI template
     (`crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`) so generated app
     CLIs expose `--version`. Until adopted, `build-cli` reads the version from
     `cargo metadata` and smoke-checks with `--help`.

8. **Docs**
   - Rewrite `docs/guide/deploy-github-actions.md` around the three-layer model
     and general EdgeZero-app-repo adoption (not Trusted-Server-specific).
   - Document the app-provided CLI package build, artifact reuse, credential
     scoping, adapter layering, and non-goals.

9. **Validation**
   - Bash contract tests, `actionlint`, `shellcheck`, `zizmor`, checksum
     metadata, docs validation, composite smoke test.
   - Workspace Rust tests, format, clippy, and feature checks.

## Known follow-up candidates

- Add `deploy-cloudflare` / `deploy-spin` wrappers via the same engine.
- Add provider-specific staging/health-check/rollback as separate actions.
- Optionally consume a prebuilt/attested CLI binary matching the app's pinned
  version instead of compiling from source.
