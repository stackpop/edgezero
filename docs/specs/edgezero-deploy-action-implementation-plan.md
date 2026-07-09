# EdgeZero Deploy GitHub Actions Implementation Plan

**Status:** Revised plan (layered, adapter-independent)

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

## Scope

Implement the layered deploy actions in the EdgeZero monorepo:

```text
.github/actions/build-cli
.github/actions/deploy-core
.github/actions/deploy-fastly
.github/actions/healthcheck-fastly
.github/actions/rollback-fastly
```

`build-cli` compiles the app-provided CLI package once and publishes it as an
artifact. `deploy-core` is the adapter-independent deploy engine that consumes
the prebuilt CLI. `deploy-fastly` is a minimal wrapper that types Fastly
credentials and calls the engine, with an optional `stage` mode. `healthcheck-fastly`
and `rollback-fastly` are thin Fastly-specific wrappers for the staging lifecycle
(§5.4). Provider orchestration (build, deploy, config push, provision, stage,
healthcheck, rollback) stays in the CLI.

The actions deploy already checked-out application source. They do not perform
checkout or runtime config mutation as a deploy side effect. Fastly staging,
health checks, and rollback are provided as provider-specific actions (§5.4)
driven by the app CLI; the generic engine stays neutral.

## Porting from the existing #303 action (reference)

This design **supersedes** the monolithic Fastly action from #303
(`.github/actions/deploy/`). That branch is not the base; its scripts are a
reference to port from. Most transfer with light changes:

| Existing `.github/actions/deploy/`           | New home                        | Disposition                                                     |
| -------------------------------------------- | ------------------------------- | --------------------------------------------------------------- |
| `scripts/common.sh`                          | `deploy-core/scripts/`          | Reuse ~as-is (annotation escaping, helpers).                    |
| `scripts/cleanup.sh`                         | `deploy-core/scripts/`          | Reuse.                                                          |
| `scripts/write-summary.sh`                   | `deploy-core/scripts/`          | Reuse; update summary field names.                              |
| `scripts/validate-inputs.sh`                 | `deploy-core/scripts/`          | Reuse; move Fastly-specific allowlist to the wrapper.           |
| `scripts/resolve-project.sh`                 | `deploy-core/scripts/`          | Reuse + split Git root vs Cargo workspace root.                 |
| `scripts/install-rust.sh`                    | shared                          | Reuse; parameterize (build-cli host-only; engine adds target).  |
| `scripts/run-edgezero.sh`                    | `deploy-core/scripts/`          | Adapt to invoke `<cli-bin>` from the artifact + provider-env.   |
| `tests/run.sh`                               | `deploy-core/tests/`            | Reuse the harness; add new cases.                               |
| `scripts/install-fastly.sh`, `versions.json` | `deploy-fastly/`                | Move (provider-specific install + checksum).                    |
| `scripts/install-edgezero.sh`                | → `build-cli`                   | Rewrite: build the **app's** CLI package, not the monorepo CLI. |
| `action.yml` (one composite)                 | `build-cli/` + `deploy-fastly/` | Split into build + wrapper; engine is sourced scripts.          |
| `.github/workflows/deploy-action.yml`        | same path                       | Rewrite: de-Python, repin actions to tags.                      |
| cache `uses: actions/cache@<sha>`            | `actions/cache@v4`              | Repin to readable tag.                                          |

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
   - Non-secret parameters (available to all steps): `adapter`, `cli-artifact`,
     `cli-bin`, `working-directory`, `manifest`, `rust-toolchain`, `target`,
     `build-mode`, `build-args`, `deploy-args`, `deploy-arg-allow`,
     `provider-env-clear`, `deploy-flags`, `cache`.
   - **`provider-env` is NOT one of these.** It is bound only to the deploy
     step's own `env:` (step-scoped) and parsed only there — never present in the
     setup/build step environments, so the secret-bearing blob cannot leak (spec
     §5.2, §10). Setup/build see only the non-secret parameters plus
     `provider-env-clear`.
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
     fallback) and install the **wrapper-provided** concrete `target` (the engine
     never maps `adapter` → target).
   - Optional exact-key cache of the **Cargo workspace root** `target/`
     restore/save.
   - Resolve `build-mode`; optional credential-free build.
   - Non-deploy steps: unset the `provider-env-clear` names (defense-in-depth;
     `provider-env` itself is absent here).
   - Deploy step only (its `env:` carries `provider-env`): clear the
     `provider-env-clear` aliases, parse `provider-env` and export only its
     values, then run
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
     `build-mode`, `build-args`, `deploy-args`, `cache`, and `stage` (§5.4).
   - Map `fastly-api-token` → `provider-env: {FASTLY_API_TOKEN: …}` and
     `fastly-service-id` → action-owned
     `deploy-flags: ["--service-id", …, "--non-interactive"]`; when
     `stage: true`, add `--stage`.
   - Set `adapter: fastly`, `target: wasm32-wasip1`, `deploy-arg-allow` =
     `--comment` only, `provider-env-clear` = Fastly auth/endpoint aliases
     (`FASTLY_API_TOKEN`, `FASTLY_SERVICE_ID`, `FASTLY_ENDPOINT`, …), Fastly
     `auto` build-mode → `never`.
   - **Install the pinned Fastly CLI** (official release + checksum, action-owned
     dir on `PATH`) before sourcing the engine, so `<cli> deploy --adapter fastly`
     finds `fastly`. This is the wrapper's provider-tool responsibility; the
     engine assumes `fastly` is already on `PATH`.
   - Output `fastly-version` (parsed from the app CLI). Source the shared
     `deploy-core` scripts; no build, toolchain, or path logic of its own.

4. **Fastly staging lifecycle actions (§5.4)**
   - `healthcheck-fastly`: thin wrapper — inputs `cli-artifact`, `cli-bin`,
     `fastly-api-token`, `fastly-service-id`, `fastly-version`, `domain`,
     `deploy-to` (`production`/`staging`), retry/timeout; runs
     `<cli> healthcheck --adapter fastly --service-id <id> --version <v> …` with
     `FASTLY_API_TOKEN` in the step env; outputs `healthy`, `status-code`.
   - `rollback-fastly`: thin wrapper — inputs `cli-artifact`, `cli-bin`,
     `fastly-api-token`, `fastly-service-id`, `fastly-version`, `deploy-to`;
     runs `<cli> rollback --adapter fastly --service-id <id> --version <v> …` with
     `FASTLY_API_TOKEN` in the step env; outputs `rolled-back-to`.
   - Both map `fastly-service-id` → `--service-id` and `fastly-api-token` →
     step-scoped `FASTLY_API_TOKEN`. They reuse only the CLI-artifact download +
     credential-scoping helpers from `deploy-core`; no source resolution,
     toolchain, build, cache, or Fastly CLI install (they call the Fastly API).
     Carry no orchestration policy — the caller wires stage → healthcheck →
     rollback.

5. **Scripts layout**
   - Provider-neutral scripts under `deploy-core/`; the Fastly install + checksum
     step lives with `deploy-fastly/` (or a shared script keyed by adapter).
   - No CLI-build script here — CLI build lives entirely in `build-cli`.

6. **CI workflow (`.github/workflows/deploy-action.yml`) — no Python**
   - Pin third-party actions to readable released tags (`actions/checkout@v4`,
     `actions/cache@v4`, artifact upload/download at released tags).
   - Run `actionlint` from a pinned release binary (no `go run @<commit>`).
   - Run `zizmor` from a pinned release binary or `cargo install zizmor --locked`
     (no `pip`).
   - Port the metadata-validation heredocs into `tests/run.sh`.
   - Composite smoke test: `build-cli` → `deploy-fastly` (both production and
     `stage: true`) → `healthcheck-fastly` → `rollback-fastly`. Fake each action's
     real dependency: a fake `fastly` binary (marker files + printed version) for
     `deploy-fastly`; a fake app CLI or stubbed Fastly API/`curl` responses for
     `healthcheck-fastly`/`rollback-fastly` (they call the API, not `fastly`).
     Assert CLI-artifact reuse, credential scoping, and `fastly-version`
     threading.

7. **Bash contract tests (`tests/run.sh`)**
   - Cover engine + wrappers: adapter/boolean/JSON validation, path confinement,
     symlink escape, dirty source, toolchain precedence, cache keys, credential
     scoping, deploy-arg allowlist, build/deploy argv, cleanup, log redaction,
     metadata contract checks, and the staging lifecycle (stage flag, version
     output parsing, healthcheck/rollback argv, staging vs production paths).
   - No Python; no live provider credentials.

8. **Companion CLI scaffolding (`crates/edgezero-cli`, `edgezero-adapter-fastly`)**
   - Add `#[command(version)]` to the downstream CLI template
     (`crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`) so generated app
     CLIs expose `--version`. Until adopted, `build-cli` reads the version from
     `cargo metadata` and smoke-checks with `--help`.
   - Fastly staging deploy: extend the Fastly adapter `deploy` path with
     `--stage` → `fastly compute update --autoclone --version=active` +
     `fastly service-version stage`; emit the service version in a parseable form.
   - Add `healthcheck` and `rollback` CLI subcommands with a Fastly adapter
     implementation (staging-IP resolution via the Fastly API + `curl`; activate
     previous / deactivate staged), and wire `Healthcheck` / `Rollback` arms into
     the downstream CLI template so app CLIs expose them.

9. **Docs**
   - Rewrite `docs/guide/deploy-github-actions.md` around the three-layer model,
     general EdgeZero-app-repo adoption, and the Fastly staging lifecycle.
   - Document the app-provided CLI package build, artifact reuse, credential
     scoping, adapter layering, staging trio, and non-goals.

10. **Validation**
    - Bash contract tests, `actionlint`, `shellcheck`, `zizmor`, checksum
      metadata, docs validation, composite smoke test.
    - Workspace Rust tests, format, clippy, and feature checks.

## Known follow-up candidates

- Add `deploy-cloudflare` / `deploy-spin` wrappers via the same engine.
- Add staging/health-check/rollback lifecycle actions for adapters **beyond
  Fastly** (Fastly's trio is in current scope, phases 3–4 / 8).
- Optionally consume a prebuilt/attested CLI binary matching the app's pinned
  version instead of compiling from source.
