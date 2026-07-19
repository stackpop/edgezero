# EdgeZero Deploy GitHub Actions Implementation Plan

**Status:** Revised plan (layered, adapter-independent)

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

## Scope

Implement the layered deploy actions in the EdgeZero monorepo:

```text
.github/actions/build-app-cli
.github/actions/deploy-core
.github/actions/deploy-fastly
.github/actions/healthcheck-fastly
.github/actions/rollback-fastly
```

`build-app-cli` compiles the app-provided CLI package once and publishes it as an
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

| Existing `.github/actions/deploy/`           | New home                            | Disposition                                                                                                                                                                                      |
| -------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `scripts/common.sh`                          | `deploy-core/scripts/`              | Reuse ~as-is (annotation escaping, helpers).                                                                                                                                                     |
| `scripts/cleanup.sh`                         | `deploy-core/scripts/`              | Reuse.                                                                                                                                                                                           |
| `scripts/write-summary.sh`                   | `deploy-core/scripts/`              | Reuse; update summary field names.                                                                                                                                                               |
| `scripts/validate-inputs.sh`                 | `deploy-core/scripts/`              | Reuse; move Fastly-specific allowlist to the wrapper.                                                                                                                                            |
| `scripts/resolve-project.sh`                 | `deploy-core/scripts/`              | Reuse + split Git root vs Cargo workspace root.                                                                                                                                                  |
| `scripts/install-rust.sh`                    | dropped                             | Replaced by `actions-rust-lang/setup-rust-toolchain@v1` in deploy-fastly (toolchain from resolve output + wasm target). build-app-cli keeps `rustup` for dynamic app-resolved toolchain install. |
| `scripts/run-edgezero.sh`                    | `deploy-core/scripts/`              | Adapt to invoke `<app-cli-bin>` from the artifact + provider-env.                                                                                                                                |
| `tests/run.sh`                               | `deploy-core/tests/`                | Reuse the harness; add new cases.                                                                                                                                                                |
| `scripts/install-fastly.sh`, `versions.json` | `deploy-fastly/`                    | Move (provider-specific install + checksum).                                                                                                                                                     |
| `scripts/install-edgezero.sh`                | → `build-app-cli`                   | Rewrite: build the **app's** CLI package, not the monorepo CLI.                                                                                                                                  |
| `action.yml` (one composite)                 | `build-app-cli/` + `deploy-fastly/` | Split into build + wrapper; engine is sourced scripts.                                                                                                                                           |
| `.github/workflows/deploy-action.yml`        | same path                           | Rewrite: de-Python, repin actions to tags.                                                                                                                                                       |
| cache `uses: actions/cache@<sha>`            | `actions/cache@v4`                  | Repin to readable tag.                                                                                                                                                                           |

## Implementation phases

1. **`build-app-cli` action**
   - Required `app-cli-package` input: the Cargo package name of the CLI defined in
     the application's own workspace. Fail if missing or not found in the app
     workspace under `working-directory`.
   - `working-directory`, `rust-toolchain`, `app-cli-artifact` inputs (no `adapters`
     input — the app's `Cargo.toml` pins adapters).
   - Optional `app-cli-bin` input (default = `app-cli-package`; the generated CLI names
     its bin after the package).
   - Require a `Cargo.lock` at the app's Cargo workspace root; run all Cargo
     commands with `--locked` (never mutate the lockfile). Validate via
     `cargo metadata --locked` that `app-cli-package` exists and declares a
     `<app-cli-bin>` binary target.
   - Install the host toolchain (no WASM target — the CLI is a native tool);
     build into an **action-owned `CARGO_TARGET_DIR` under `RUNNER_TEMP`** (never
     the checkout) so the CLI build leaves the app tree clean for the later
     dirty-source guard, via
     `cargo build --locked --release -p <app-cli-package> --bin <app-cli-bin>`. No
     `--features` injection: the app's own `Cargo.toml` pins its adapters.
   - Read `app-cli-version` from `cargo metadata`; smoke-check with `<app-cli-bin> --help`
     (today's CLI has no `--version`); write `app-cli-meta.json` (`app-cli-bin`,
     `app-cli-version`, `app-cli-package`) next to the binary and upload both as one
     **tar** so the executable bit survives `actions/upload-artifact` and the
     artifact is self-describing.
   - Outputs: `app-cli-version`, `app-cli-package`, `app-cli-bin`, `app-cli-artifact`.
   - No provider credentials in scope. Never builds the EdgeZero monorepo CLI.

2. **`deploy-core` shared engine scripts (provider-neutral)**
   - A directory of scripts under `.github/actions/deploy-core/`, **not** a
     standalone composite action. Wrappers source them via
     `$GITHUB_ACTION_PATH/../deploy-core/…`.
   - Non-secret parameters (available to all steps): `adapter`, `app-cli-artifact`,
     `app-cli-bin`, `working-directory`, `manifest`, `rust-toolchain`, `target`,
     `build-mode`, `build-args`, `deploy-args`, `deploy-arg-allow`,
     `provider-env-clear`, `deploy-flags`, `cache`.
   - **`provider-env` is NOT one of these.** It is bound only to the deploy
     step's own `env:` (step-scoped) and parsed only there — never present in the
     setup/build step environments, so the secret-bearing blob cannot leak (spec
     §5.2, §10). Setup/build see only the non-secret parameters plus
     `provider-env-clear`.
   - Download the CLI artifact (tar) under `RUNNER_TEMP`, extract preserving the
     executable bit (or `chmod +x <app-cli-bin>`), read `app-cli-meta.json` for
     `app-cli-bin`/`app-cli-version` (wrapper `app-cli-bin` overrides), and PATH-scope it.
   - Validate `adapter` well-formedness (no compiled-adapter enumeration — the
     CLI rejects unknown adapters itself), booleans, JSON arrays/object, NUL
     bytes.
   - Confine `working-directory` and `manifest` inside `github.workspace`
     (canonical paths, symlink resolution).
   - Resolve **Git root** for `source-revision` and the dirty-source guard
     (`build-app-cli`'s isolated target dir keeps this clean).
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
     `<app-cli-bin> deploy --adapter <adapter> -- <deploy-flags…> <deploy-args…>` via
     Bash arrays. Note the build-in-deploy caveat: Fastly's default `never`
     compiles the app during deploy with the token in scope, so require trusted
     immutable refs (spec §10.1).
   - Surface results to the wrapper: `source-revision`, `app-cli-version` (the
     Fastly wrapper adds `fastly-version` and, for production, `previous-version`).
   - Contains no provider-specific credential names, service concepts, endpoints,
     or CLI flags; invokes `<app-cli-bin>`, never a hard-coded `edgezero`.

3. **`deploy-fastly` wrapper (minimal composite action)**
   - Typed inputs: `app-cli-artifact`, `app-cli-bin`, `fastly-api-token`,
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
   - `healthcheck-fastly`: thin wrapper — inputs `app-cli-artifact`, `app-cli-bin`,
     `fastly-api-token`, `fastly-service-id`, `fastly-version`, `domain`,
     `deploy-to` (`production`/`staging`), retry/timeout; runs
     `<cli> healthcheck --adapter fastly --service-id <id> --version <v> …` with
     `FASTLY_API_TOKEN` in the step env; outputs `healthy`, `status-code`.
   - `rollback-fastly`: thin wrapper — inputs `app-cli-artifact`, `app-cli-bin`,
     `fastly-api-token`, `fastly-service-id`, `fastly-version`, `rollback-to`
     (production only — the version to re-activate, captured from
     `deploy-fastly`'s `previous-version`; Fastly cannot infer it), `deploy-to`;
     runs `<cli> rollback --adapter fastly --service-id <id> --version <v> …` with
     `FASTLY_API_TOKEN` in the step env; outputs `rolled-back-to`.
   - Both map `fastly-service-id` → `--service-id` and `fastly-api-token` →
     step-scoped `FASTLY_API_TOKEN`. They reuse only the CLI-artifact download +
     credential-scoping helpers from `deploy-core`; no source resolution,
     toolchain, build, cache, or Fastly CLI install (they call the Fastly API).
     Carry no orchestration policy — the caller wires stage → healthcheck →
     rollback.

5. **`config-push-fastly` action + CLI staging support (§5.5)**
   - CLI (`edgezero-adapter-fastly` + `edgezero-cli`): add `--staging` to
     `config push`. `config.rs` already resolves one push entry
     `(key, body)` where `key = args.key.unwrap_or(logical_store_id)`; when
     `--staging` is set, derive the staging variant `<logical>_staging` (same
     resolved store, different key). `--key` is mutually exclusive with
     `--staging` (a derived staging key an explicit key would never match).
     Mirror the derivation in `config diff` so a staged diff compares against the
     staged key. Colocated tests: production key unchanged, `--staging` derives
     `<logical>_staging`, explicit `--key` + `--staging` is refused.
   - CLI canonical line: after a successful non-dry-run write, `config.rs` emits
     `pushed-key=<key>` (via `log::info!`, which the CLI routes to stdout) so the
     wrapper can thread the written key out as an output.
   - `config-push-fastly` wrapper: a **thin** composite mirroring
     `healthcheck-fastly` / `rollback-fastly` (not the heavy `deploy` path).
     Inputs `app-cli-artifact`, `app-cli-bin`, `fastly-api-token`,
     `working-directory`, `manifest`, `app-config`, `store`, `key`, `deploy-to`
     (`production`/`staging`, validated + fail-closed). Downloads the CLI artifact,
     installs the pinned Fastly CLI (the push shells out to
     `fastly config-store-entry update`), and calls the CLI directly with
     `FASTLY_API_TOKEN` in the step env — the adapter's own convention — with
     every other `FASTLY_*` alias blanked. Outputs `pushed-key`, `store`.
   - Contract + smoke coverage: a `config-push.sh` argv test (staging appends
     `--staging`; production does not); the smoke fixture's fake `fastly` gains
     `config-store list` / `config-store-entry` handlers, and a staged push
     asserts the `<logical>_staging` key reached the store.

6. **Scripts layout**
   - Provider-neutral scripts under `deploy-core/`; the Fastly install + checksum
     step lives with `deploy-fastly/` (or a shared script keyed by adapter).
   - No CLI-build script here — CLI build lives entirely in `build-app-cli`.

7. **CI workflow (`.github/workflows/deploy-action.yml`) — no Python**
   - Pin third-party actions to readable released tags (`actions/checkout@v4`,
     `actions/cache@v4`, artifact upload/download at released tags).
   - Run `actionlint` from a pinned release binary (no `go run @<commit>`).
   - Run `zizmor` from a pinned release binary or `cargo install zizmor --locked`
     (no `pip`).
   - Port the metadata-validation heredocs into `tests/run.sh`.
   - Composite smoke test: `build-app-cli` → `deploy-fastly` (both production and
     `stage: true`) → `healthcheck-fastly` → `rollback-fastly`. Fake each action's
     real dependency: a fake `fastly` binary (marker files + printed version) for
     `deploy-fastly`; a fake app CLI or stubbed Fastly API/`curl` responses for
     `healthcheck-fastly`/`rollback-fastly` (they call the API, not `fastly`).
     Assert CLI-artifact reuse, credential scoping, and `fastly-version`
     threading.

8. **Bash contract tests (`tests/run.sh`)**
   - Cover engine + wrappers: adapter/boolean/JSON validation, path confinement,
     symlink escape, dirty source, toolchain precedence, cache keys, credential
     scoping, deploy-arg allowlist, build/deploy argv, cleanup, log redaction,
     metadata contract checks, and the staging lifecycle (stage flag, version
     output parsing, healthcheck/rollback argv, staging vs production paths).
   - No Python; no live provider credentials.

9. **Companion CLI scaffolding (`crates/edgezero-cli`, `edgezero-adapter-fastly`)**
   - Add `#[command(version)]` to the downstream CLI template
     (`crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`) so generated app
     CLIs expose `--version`. Until adopted, `build-app-cli` reads the version from
     `cargo metadata` and smoke-checks with `--help`.
   - Fastly staging deploy: extend the Fastly adapter `deploy` path with
     `--stage` → `fastly compute update --autoclone --version=active` +
     `fastly service-version stage`; emit the service version in a parseable form.
   - Add `healthcheck` and `rollback` CLI subcommands with a Fastly adapter
     implementation (staging-IP resolution via the Fastly API + `curl`; activate
     previous / deactivate staged), and wire `Healthcheck` / `Rollback` arms into
     the downstream CLI template so app CLIs expose them.

10. **Docs**

- Write `docs/guide/deploy-github-actions.md` around the three-layer model,
  general EdgeZero-app-repo adoption, and the Fastly staging lifecycle.
- Document the app-provided CLI package build, artifact reuse, credential
  scoping, adapter layering, staging trio, and non-goals.

11. **Validation**
    - Bash contract tests, `actionlint`, `shellcheck`, `zizmor`, checksum
      metadata, docs validation, composite smoke test.
    - Workspace Rust tests, format, clippy, and feature checks.

## Known follow-up candidates

- Add `deploy-cloudflare` / `deploy-spin` wrappers via the same engine.
- Add staging/health-check/rollback lifecycle actions for adapters **beyond
  Fastly** (Fastly's trio is in current scope, phases 3–4 / 8).
- Optionally consume a prebuilt/attested CLI binary matching the app's pinned
  version instead of compiling from source.
