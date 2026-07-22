# EdgeZero Deploy GitHub Actions — Layered, Adapter-Independent Spec

**Status:** Revised design (supersedes the Fastly-only v0 spec)

**Date:** 2026-07-08

**Delivery target:** implementation in the `stackpop/edgezero` monorepo

**Action paths:**

```text
.github/actions/build-app-cli
.github/actions/deploy-core
.github/actions/deploy-fastly
.github/actions/healthcheck-fastly
.github/actions/rollback-fastly
.github/actions/config-push-fastly
```

## 1. Executive summary

Ship reusable GitHub composite actions that deploy a checked-out EdgeZero
application to a supported provider by driving the EdgeZero CLI.

The design is **layered** so a new adapter is added without rewriting the deploy
engine, and the **CLI is the boundary** — the actions never reproduce provider
build, deploy, config-push, or provision logic in YAML or shell. Everything an
adapter needs (`edgezero build`, `edgezero deploy`, `edgezero config push`,
`edgezero provision`) already lives in the CLI; the actions only compile the
CLI, scope credentials, and invoke the right subcommand.

The CLI is **the app's own CLI package.** The application tells `build-app-cli` which
CLI package to compile (a crate in the application's own workspace), and
`build-app-cli` builds it from the application checkout. It is **not** the EdgeZero
monorepo CLI and **not** the action's own repository revision. Because the CLI is
the app's own package, built from the app's source and lockfile, the CLI and the
application always agree on the manifest, adapter, and config schema — and an app
may ship a CLI extended with its own commands.

Three layers:

| Layer           | Action              | Responsibility                                                                        |
| --------------- | ------------------- | ------------------------------------------------------------------------------------- |
| Build           | `build-app-cli`     | Compile the app-provided CLI package **once** and publish it.                         |
| Engine (shared) | `deploy-core`       | Adapter-independent engine **scripts** sourced by wrappers; consume the prebuilt CLI. |
| Adapter wrapper | `deploy-fastly` (…) | Minimal per-adapter shim: type provider credentials, call the engine.                 |

The actions do not check out application source. The caller owns checkout,
repository permissions, ref selection, GitHub Environment policy, concurrency,
timeouts, and **orchestrating** the health-check / rollback process (the Fastly
mechanisms themselves are provided as actions — §5.4).

The core boundary is EdgeZero itself:

```text
<cli> build  --adapter <adapter>
<cli> deploy --adapter <adapter>
```

where `<cli>` is the application's own CLI binary built by `build-app-cli`.

The generic engine stays provider-neutral. Provider-specific staging, health
checks, and rollback are supported for Fastly as a separate lifecycle (§5.4) —
scaffolded into the EdgeZero CLI's Fastly adapter and exposed through the app's
own CLI, with thin action wrappers — so the engine never grows provider logic.

## 2. Design principles

1. **EdgeZero CLI is the deployment boundary.** The actions invoke CLI
   subcommands instead of reproducing provider logic. Provider orchestration
   belongs in the CLI, not in action shell scripts.
2. **Layered and adapter-independent.** A generic `deploy-core` engine holds all
   provider-neutral behavior. Adapter wrappers are minimal and only supply typed
   credentials and the adapter name. Adding an adapter adds a wrapper; it does
   not fork the engine.
3. **The caller owns source.** The actions never call `actions/checkout`.
4. **The application provides the CLI package.** The app tells `build-app-cli` which
   CLI package to compile via a required `app-cli-package` input, and `build-app-cli`
   builds that package from the application's own checkout. It never builds the
   EdgeZero monorepo CLI or the action's own repository revision. The
   application owns which CLI deploys it.
5. **Compile once, reuse everywhere.** `build-app-cli` compiles the CLI a single
   time per workflow and publishes it as an artifact. Deploy actions consume the
   prebuilt binary and never recompile it.
6. **Typed provider credentials.** Credentials are passed through wrapper action
   inputs, not caller `env:`, so setup and build steps never inherit provider
   tokens. Only the provider steps receive them — the deploy (as `provider-env`
   data) and the Fastly lifecycle steps that call the provider directly
   (rollback-target capture, healthcheck, rollback, config-push, under the
   adapter's own `FASTLY_API_TOKEN` convention); every other step blanks them.
7. **No shell string APIs.** Passthrough arguments are JSON arrays invoked
   through Bash arrays without `eval`.
8. **No Python in tooling or CI.** Validation, metadata checks, and security
   scans run through Bash, `jq`, and pinned release binaries (`actionlint`,
   `zizmor`). No `python3` heredocs and no `pip install`.
9. **Pin third-party actions to readable released tags.** Reusable third-party
   actions are referenced by their published major/minor tag (for example
   `actions/checkout@v4`), not opaque commit SHAs chosen ad hoc.
10. **Safe by default.** Caching is opt-in, deploys require committed source, and
    provider credentials never reach outputs, summaries, caches, or
    action-global environment files.

## 3. Goals

1. Deploy any checked-out EdgeZero application from GitHub Actions through the
   EdgeZero CLI.
2. Keep the deploy engine adapter-independent so Cloudflare, Spin, and future
   adapters reuse it.
3. Compile the CLI package the application provides, from the application's own
   source, so the CLI and the deployed application never disagree on schema.
4. Compile the CLI once and share the artifact across deploy steps and jobs.
5. Support same-repository, separate-repository, private-repository, and
   monorepo checkout layouts.
6. Respect the application's `edgezero.toml` when present and support explicit
   `working-directory` and `manifest` selection.
7. Accept typed provider credentials and expose them only to the steps that call
   the provider (the deploy and the Fastly lifecycle steps).
8. Support JSON-array build and deploy passthrough arguments.
9. Support opt-in, exact-key application `target/` caching.
10. Produce actionable validation failures before deployment begins.
11. Keep all tooling and CI free of Python; use pinned release binaries.

## 4. Non-goals

The **generic** engine (`deploy-core`) will not:

1. check out application source;
2. choose an application ref;
3. deploy more than one adapter per `deploy-*` invocation;
4. provision provider resources or push runtime config as a side effect of
   deploy — config push is its own action (`config-push-fastly`, §5.5), and
   provision remains an explicit CLI subcommand the caller may run separately;
5. implement provider staging, health checks, rollback, or deployment-version
   parsing **in the provider-neutral engine** — these are provider-specific and
   live in the Fastly staging lifecycle actions (§5.4), driven by the app CLI;
6. configure GitHub job permissions, environments, approvals, concurrency, or
   timeouts;
7. support Windows or macOS runners;
8. publish a stable version alias; or
9. provide a general `setup` action for running arbitrary EdgeZero commands
   (the CLI is available via the `build-app-cli` artifact for callers who need it).

Staging deploy, health checks, and rollback **are** supported for Fastly, as a
provider-specific lifecycle (§5.4). The engine stays neutral; the capability is
scaffolded into the EdgeZero CLI's Fastly adapter and exposed through the app's
own CLI, with thin action wrappers.

## 5. Architecture

### 5.1 Layer 1 — `build-app-cli`

Compiles the **CLI package the application provides** — a crate in the
application's own workspace, named by the required `app-cli-package` input — once,
and publishes it as a workflow artifact so every downstream deploy step consumes
the same binary. The CLI is built from the application checkout and its lockfile,
so it matches the application and may include app-specific commands. `build-app-cli`
never builds the EdgeZero monorepo CLI.

**Inputs**

| Input               | Required | Default             | Contract                                                                                                                                                         |
| ------------------- | -------- | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-cli-package`   | Yes      | none                | Cargo package name of the CLI to build, defined in the application's own workspace. `build-app-cli` builds this package from the application checkout.           |
| `app-cli-bin`       | No       | `<app-cli-package>` | Binary name produced by `app-cli-package`. Defaults to the package name (the generated downstream CLI names its bin after the package).                          |
| `working-directory` | No       | `.`                 | Application directory (relative to `github.workspace`) containing the workspace/lockfile that defines `app-cli-package`. Must resolve inside `github.workspace`. |
| `rust-toolchain`    | No       | `auto`              | Explicit toolchain, or `auto` to follow the application toolchain resolution precedence (§7).                                                                    |
| `app-cli-artifact`  | No       | `edgezero-cli`      | Name of the uploaded artifact.                                                                                                                                   |

There is intentionally no `adapters` / features input. The application's own
`Cargo.toml` already pins which adapters compile into its CLI (through the
`edgezero-cli` dependency it declares); `build-app-cli` builds the package exactly as
the application declares it, so the app owns adapter selection.

**Outputs**

| Output             | Meaning                                                        |
| ------------------ | -------------------------------------------------------------- |
| `app-cli-version`  | CLI package version, read from `cargo metadata` at build time. |
| `app-cli-package`  | The application CLI package that was built.                    |
| `app-cli-bin`      | The binary name inside the artifact.                           |
| `app-cli-artifact` | Name of the uploaded CLI artifact for downstream `download`.   |

**Behavior**

1. Require a `Cargo.lock` at the app's Cargo workspace root (see §11.1); fail
   with a remediation message if it is missing. All Cargo commands run with
   `--locked` so the build never creates or updates the lockfile.
2. Confirm via `cargo metadata --locked` that `app-cli-package` exists in the
   application workspace under `working-directory` and that it declares a binary
   target named `<app-cli-bin>` (default `<app-cli-package>`). Fail if either is absent.
3. Install the resolved host Rust toolchain (§7). The CLI is a native host tool;
   the WASM target needed to build the _application_ is installed later by the
   deploy engine, not here.
4. Build **into an action-owned `CARGO_TARGET_DIR` below `RUNNER_TEMP`**, never
   the app checkout, so the CLI build does not write `target/` into the
   application working tree (which the deploy engine later dirty-checks):

   ```text
   CARGO_TARGET_DIR=<RUNNER_TEMP>/edgezero-cli-build \
     cargo build --locked --release -p <app-cli-package> --bin <app-cli-bin>
   ```

5. Read `app-cli-version` from `cargo metadata` for `app-cli-package`, and smoke-check
   the binary with `<app-cli-bin> --help`. The version comes from `cargo metadata`
   (authoritative, and works even for a hand-written CLI that omits the flag), and
   `--help` is the runnability check every clap CLI supports.
6. Write a small metadata file (`app-cli-meta.json`) next to the binary containing
   `app-cli-bin`, `app-cli-version`, and `app-cli-package`.
7. Upload the binary **and `app-cli-meta.json`** as a single **tar archive** so the
   executable bit survives the round trip (`actions/upload-artifact` zips and
   drops POSIX permissions).

The artifact is self-describing: the engine reads `app-cli-meta.json` to learn the
binary name and version, so callers do not have to re-pass `app-cli-bin`/`app-cli-version`
(a wrapper `app-cli-bin` input, if given, overrides the metadata).

`build-app-cli` never receives provider credentials and leaves the app checkout
clean (no `target/`, no lockfile mutation), so a later dirty-source guard passes.

> **CLI version surface:** the generated downstream CLI template sets clap
> `version` (`#[command(name = "…", version, …)]`), so `<cli> --version` works.
> `build-app-cli` nonetheless takes `app-cli-version` from `cargo metadata` (it is
> authoritative and works even for a hand-written CLI that omits the flag) and
> uses `--help` for the runnability check.

### 5.2 Layer 2 — `deploy-core` (shared engine scripts)

Adapter-independent. Holds every provider-neutral concern so wrappers stay
minimal. `deploy-core` is **not a standalone composite action**; it is a
directory of shared scripts under `.github/actions/deploy-core/`. Each adapter
wrapper is the real composite action and sources these scripts through
`$GITHUB_ACTION_PATH/../deploy-core/…`, which resolves inside the same fetched
EdgeZero action repository. This avoids referencing a "private" sibling action by
ref and keeps one engine for every adapter.

The engine is parameterized by the values the wrapper passes to those scripts
(as environment variables), conceptually:

| Parameter            | Meaning                                                                                                                                                                                                                                            |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `adapter`            | Passed to `<cli> deploy --adapter <adapter>`. The engine does **not** enumerate compiled adapters; the CLI rejects an unknown adapter with its own error.                                                                                          |
| `app-cli-artifact`   | Name of the `build-app-cli` artifact to download. The engine reads `app-cli-bin` and `app-cli-version` from the artifact's `app-cli-meta.json`.                                                                                                    |
| `app-cli-bin`        | Optional override for the binary name; if empty, taken from `app-cli-meta.json`.                                                                                                                                                                   |
| `working-directory`  | Application directory relative to `github.workspace`. Must resolve inside `github.workspace`.                                                                                                                                                      |
| `manifest`           | Optional `edgezero.toml` path relative to `working-directory`. If set, must exist; exported as `EDGEZERO_MANIFEST`.                                                                                                                                |
| `rust-toolchain`     | Application Rust toolchain for the deploy build. `auto` follows §7.                                                                                                                                                                                |
| `target`             | Concrete application build target the **wrapper** supplies (Fastly → `wasm32-wasip1`). The engine installs exactly this target and never maps `adapter` → target, so adding an adapter does not touch the engine.                                  |
| `build-mode`         | One of `auto`, `always`, `never` (§8).                                                                                                                                                                                                             |
| `build-args`         | JSON array of strings passed after `<cli> build --adapter <adapter> --`. Must not contain secrets.                                                                                                                                                 |
| `deploy-args`        | JSON array of caller-supplied deploy args appended after action-owned deploy flags. Must not contain secrets.                                                                                                                                      |
| `deploy-arg-allow`   | Adapter allowlist pattern for caller `deploy-args` (wrapper-provided; §9).                                                                                                                                                                         |
| `provider-env`       | JSON object of provider credential names → values. Present **only in the deploy step's own environment** (step-scoped `env:`); the variable never reaches setup/build steps, and the engine parses it only inside the deploy step (§10).           |
| `provider-env-clear` | JSON array of env var names (wrapper-provided) the engine unsets in non-deploy steps and clears + re-exports from `provider-env` inside the deploy step (§10). Defense-in-depth against inherited caller `env:`; keeps clearing provider-agnostic. |
| `deploy-flags`       | JSON array of action-owned deploy flags the wrapper injects before caller `deploy-args` (`--service-id …`, `--non-interactive`).                                                                                                                   |
| `cache`              | Enable exact-key application `target/` caching (`true`/`false`).                                                                                                                                                                                   |

The wrapper surfaces its outputs: `fastly-version`, `previous-version`
(production only), `source-revision`, and `app-cli-version`.

The engine contains no provider-specific credential names, service concepts,
endpoints, or CLI flags — those, and the list of aliases to clear
(`provider-env-clear`), all arrive from the wrapper. It invokes the application's
CLI binary (`<app-cli-bin>`), not a hard-coded `edgezero`.

The engine runs `build` or `deploy`. Config push (§5.5) does not go through this
engine — like the other thin lifecycle wrappers it drives the app CLI directly.

### 5.3 Layer 3 — adapter wrappers (`deploy-fastly`, …)

Minimal composite actions. A wrapper only:

1. declares the provider's typed credential inputs;
2. maps them into `provider-env` and action-owned `deploy-flags`;
3. sets `adapter`, a **concrete `target`** (for Fastly, `wasm32-wasip1`), the
   adapter `deploy-arg` allowlist, and the `provider-env-clear` alias list;
4. **installs the pinned provider CLI it needs** — for Fastly, the Fastly CLI
   (official release, checksum-verified, into an action-owned dir on `PATH`), so
   the app CLI's `<cli> deploy --adapter fastly` (which shells out to `fastly`)
   finds it. This is the one provider-specific install, and it lives in the
   wrapper precisely so the provider-neutral engine never learns provider tools;
   and
5. sources the shared `deploy-core` scripts via `$GITHUB_ACTION_PATH/../deploy-core`.

A wrapper contains no build logic, no toolchain resolution, no path
confinement — those are engine concerns. Provider **tooling** (the Fastly CLI)
is a wrapper concern; the engine assumes the provider CLI is already on `PATH`.

**`deploy-fastly` inputs**

| Input               | Required | Default       | Contract                                                                                                                                                                     |
| ------------------- | -------- | ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | none          | `build-app-cli` artifact name. Forwarded to the engine.                                                                                                                      |
| `app-cli-bin`       | No       | from artifact | Binary name inside the artifact. Forwarded to the engine.                                                                                                                    |
| `fastly-api-token`  | Yes      | none          | Mapped into `provider-env` as `FASTLY_API_TOKEN` for the deploy step, and passed directly (adapter convention) to the rollback-target capture step; blanked everywhere else. |
| `fastly-service-id` | Yes      | none          | Mapped into action-owned `deploy-flags` as `--service-id <id>` to prevent accidental creation.                                                                               |
| `working-directory` | No       | `.`           | Forwarded to the engine.                                                                                                                                                     |
| `manifest`          | No       | empty         | Forwarded to the engine.                                                                                                                                                     |
| `build-mode`        | No       | `auto`        | Forwarded. Fastly `auto` resolves to `never`.                                                                                                                                |
| `build-args`        | No       | `[]`          | Forwarded to the engine.                                                                                                                                                     |
| `deploy-args`       | No       | `[]`          | Forwarded. Allowlisted to `--comment` for Fastly (§9).                                                                                                                       |
| `stage`             | No       | `false`       | When `true`, deploy to a **staged** draft version instead of activating production (§5.4).                                                                                   |
| `cache`             | No       | `false`       | Forwarded to the engine.                                                                                                                                                     |

**`deploy-fastly` outputs**

| Output             | Meaning                                                                                                                                      |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `fastly-version`   | The Fastly service version deployed (production) or staged. Emitted by the app CLI (§5.4).                                                   |
| `previous-version` | Production only: the version active BEFORE this deploy — the rollback target for `rollback-fastly`'s `rollback-to`. Empty on a first deploy. |
| `source-revision`  | Passthrough from the engine.                                                                                                                 |
| `app-cli-version`  | Passthrough from the engine.                                                                                                                 |

The wrapper sets `adapter: fastly`, `target: wasm32-wasip1`, the action-owned
`deploy-flags` (`--service-id …`, `--non-interactive`) so deployments cannot
prompt in CI or select an unintended service, and
`provider-env-clear: ["FASTLY_API_TOKEN", "FASTLY_SERVICE_ID", "FASTLY_ENDPOINT",
"FASTLY_CARGO_PROFILE", …]` so the engine clears Fastly auth/endpoint aliases
without the engine itself knowing Fastly's names. When `stage: true` it adds
`--stage` to the deploy flags (§5.4).

### 5.4 Fastly staging lifecycle (`deploy-fastly` stage mode, `healthcheck-fastly`, `rollback-fastly`)

Staging parity with `stackpop/trusted-server-actions` is supported for Fastly as
a **provider-specific lifecycle**. The generic engine is untouched; all Fastly
staging semantics live in the **EdgeZero CLI's Fastly adapter** and are invoked
through the app's own CLI. The three actions are thin wrappers over app-CLI
subcommands.

#### 5.4.1 CLI scaffolding (companion work, in `edgezero-adapter-fastly`)

The capability is scaffolded into the CLI, not reproduced in action shell:

| App-CLI invocation                                                                                | Fastly operations the adapter performs                                                                                                                              |
| ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `<cli> deploy --adapter fastly --service-id <id>` (production, existing)                          | `fastly compute deploy` → builds, uploads, **activates**; emits the activated version.                                                                              |
| `<cli> deploy --adapter fastly --service-id <id> --stage`                                         | `fastly compute update --autoclone --version=active` (upload to a new **draft** version, no activation) → `fastly service-version stage`; emits the staged version. |
| `<cli> healthcheck --adapter fastly --service-id <id> --version <v> --domain <d> [--staging]`     | Production: `curl` the domain. Staging: resolve `staging_ips` for `<v>` on `<id>` via the Fastly API, then `curl --connect-to` that IP; emits healthy/status.       |
| `<cli> rollback --adapter fastly --service-id <id> --version <v> [--rollback-to <p>] [--staging]` | Production: activate `<p>` on `<id>` (required — Fastly cannot infer the previous version). Staging: deactivate the staged `<v>` on `<id>`.                         |

Every Fastly subcommand takes `--service-id <id>` (the service the operation
targets). All of them read `FASTLY_API_TOKEN` from the environment EXCEPT a
PRODUCTION `healthcheck`, which only curls the public domain — it needs no token,
so the wrapper passes none (only `--staging`, which resolves the staging IP via
the API, requires one). The app CLI (built by
`build-app-cli`) exposes these subcommands the same way it exposes `deploy`/`config`.
The downstream CLI template gains `Healthcheck` and `Rollback` arms and a
deployment-version surface, tracked with the other companion CLI changes.

#### 5.4.2 Version output

Because staging must thread a version from deploy → healthcheck → rollback, the
Fastly path emits the service version. The app CLI prints it as a canonical, anchored
`version=<N>` line on stdout — the ONLY form the wrappers parse; `deploy-fastly` surfaces it as
the `fastly-version` output. This is the one **provider-specific** output; the
generic engine still exposes no deployment version.

A production `deploy-fastly` additionally emits `previous-version` — the version
active _before_ this deploy — captured by an `<cli> active-version` call before
the deploy supersedes it. It is the rollback target: because Fastly cannot infer
a previous version (§5.4.3), the caller threads `previous-version` into
`rollback-fastly`'s `rollback-to`. It is empty on a first-ever deploy.

#### 5.4.3 The three actions

- **`deploy-fastly` (`stage: true`)** — runs
  `<cli> deploy --adapter fastly --service-id <id> --stage` (the wrapper injects
  `--service-id` via `deploy-flags`); outputs `fastly-version` (the staged
  draft). Reuses the engine for build/source/credential scoping; only the
  `--stage` flag differs.
- **`healthcheck-fastly`** — thin wrapper: downloads the CLI artifact, takes
  `fastly-api-token`, `fastly-service-id`, `fastly-version`, `domain`,
  `deploy-to` (`production`/`staging`), retry/timeout inputs; runs
  `<cli> healthcheck --adapter fastly --service-id <id> --version <v> …` with
  `FASTLY_API_TOKEN` in the step env; outputs `healthy` and `status-code`. It
  **exits non-zero after retries when the probe is unhealthy** (so a caller can
  gate rollback on `if: failure()`), while still emitting the outputs. Needs no
  application source or build.
- **`rollback-fastly`** — thin wrapper: takes `fastly-api-token`,
  `fastly-service-id`, `fastly-version`, `rollback-to`, `deploy-to`; runs
  `<cli> rollback --adapter fastly --service-id <id> --version <v> …` with
  `FASTLY_API_TOKEN` in the step env; on production emits `rolled-back-to`. A
  production rollback **requires** `rollback-to` — Fastly's version metadata
  cannot distinguish a previously-live version from a staged draft, so the target
  cannot be inferred. Capture it at deploy time from `deploy-fastly`'s
  `previous-version` output and thread it through. Needs no application source or
  build.

`healthcheck-fastly` and `rollback-fastly` map `fastly-service-id` → the
`--service-id` flag and `fastly-api-token` → step-scoped `FASTLY_API_TOKEN`
(same credential discipline as deploy). They reuse only the CLI-artifact download
and credential-scoping helpers from `deploy-core`; they skip source resolution,
toolchain install, build, and cache, since they operate on Fastly service
versions via the API, not on application source. They need no Fastly CLI install
(they call the Fastly API, not `fastly compute …`).

#### 5.4.4 Composing the lifecycle

A caller wires the trio; the actions carry no orchestration policy of their own:

```yaml
- id: cli
  uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
  with: { app-cli-package: my-app-cli }

- id: stage
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    stage: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

- id: check
  uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    deploy-to: staging
    domain: staging.example.com
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

- if: failure() && steps.stage.outputs.fastly-version != ''
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    deploy-to: staging
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

Because these are Fastly-specific, future adapters do not inherit them; a new
adapter adds its own lifecycle actions if its provider supports staging.

### 5.5 Config push (`config-push-fastly`)

Deploy activates code; it never writes runtime config (§4). Pushing the typed
app config to the provider's config store is a **separate** action,
`config-push-fastly`, so a caller decides when config moves and can push it
independently of a code deploy.

It is a thin wrapper like `healthcheck-fastly` / `rollback-fastly`, not the heavy
`deploy` path: it consumes the prebuilt `build-app-cli` artifact, installs the
pinned Fastly CLI (the push shells out to `fastly config-store-entry update`),
and drives the **app's own CLI** (`<app-cli> config push --adapter fastly`)
directly. The token reaches the CLI under the adapter's own convention —
`FASTLY_API_TOKEN`, injected only into the push step — and every other `FASTLY_*`
alias is blanked, so an inherited `FASTLY_ENDPOINT` or `FASTLY_TOKEN` cannot
redirect or re-auth the push. (The heavier `provider-env` JSON boundary is for
`deploy`, which spawns arbitrary manifest build/deploy commands; config-push,
like the other lifecycle actions, does not.)

#### 5.5.1 Staging model — same store, different key

Fastly config stores are **not versioned** like the draft service versions the
deploy-staging path clones, so "push config to staging" cannot mean a draft. It
means the **same config store, a different key**:

- **Production** writes the config blob under the resolved key (the logical store
  id, or an explicit `--key`).
- **Staging** writes under the staging variant of the logical store id —
  `<logical-store-id>_staging` — in the **same** store. The staging key is
  _derived_, so `--key` is mutually exclusive with `--staging`.

The CLI gains a `config push --staging` flag (the same `--staging` verb
`deploy`/`healthcheck`/`rollback` already use); the wrapper exposes it as
`deploy-to: production | staging`, validated exactly and fail-closed like the
lifecycle actions.

#### 5.5.2 What makes a staged version _read_ the staged key

Writing `<logical>_staging` is only half of it. The runtime picks its config key from
the `EDGEZERO__STORES__CONFIG__<ID>__KEY` entry of a config store it opens by the
name `edgezero_runtime_env`. A staged deploy clones the active version, and **a
clone inherits its resource links** — so on its own, a staged version opens the
same selector store as production and reads production's key. Flipping that
shared store's selector is not an answer either: it would redirect production
too.

Fastly resource links are **per-version**, and a link's `name` is an overridable
alias. That is the seam:

- A staged deploy owns a second, **per-service** store,
  `edgezero_runtime_env_staging_<service-id>`, creating it on demand and
  **mirroring** production's runtime overrides into it — copying
  adapter/logging/`__NAME` entries verbatim and redirecting only each declared
  config store's selector to `<id>_staging`. Because the mirror runs at deploy
  time, the twin always reflects production's _current_ overrides. The name is
  per service because Fastly config stores are account-wide, versionless
  resources: a single shared twin would let one service's staged deploy clobber
  another's selectors.
- A staged deploy, while the draft is still editable, drops the inherited
  `edgezero_runtime_env` link and links the **staging store** under that same
  name. The runtime opens `edgezero_runtime_env` and gets the staging selector;
  the active version is untouched.

So the pieces compose:

| Version        | `edgezero_runtime_env` resolves to          | Config key read      |
| -------------- | ------------------------------------------- | -------------------- |
| active (prod)  | `edgezero_runtime_env`                      | `app_config`         |
| staged (draft) | `edgezero_runtime_env_staging_<service-id>` | `app_config_staging` |

A staged deploy **fails closed** when it cannot read the store listing (so it
cannot tell whether production config exists): a version that silently served
production config would be worse than a refused deploy. When the store listing is
readable, the twin is created and mirrored automatically — no separate setup
step. An app that declares no config store has no selector to isolate, so its
staged version keeps the inherited link (staged code, no config to isolate).

#### 5.5.3 Inputs / outputs

| Input               | Required | Default       | Meaning                                                                                                         |
| ------------------- | -------- | ------------- | --------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —             | The `build-app-cli` artifact to run.                                                                            |
| `fastly-api-token`  | Yes      | —             | Injected only into the push step.                                                                               |
| `working-directory` | No       | `.`           | App directory (holds the manifest + typed config).                                                              |
| `app-cli-bin`       | No       | from artifact | Binary name inside the artifact.                                                                                |
| `manifest`          | No       | empty         | `edgezero.toml` path relative to `working-directory`.                                                           |
| `app-config`        | No       | empty         | Typed config file path (default: resolved from the manifest).                                                   |
| `store`             | No       | empty         | Logical config-store id (default: the manifest's resolved id).                                                  |
| `key`               | No       | empty         | Explicit base key for a production push (default: the logical store id). Not allowed with `deploy-to: staging`. |
| `deploy-to`         | No       | `production`  | `staging` writes the `<logical-store-id>_staging` variant in the same store.                                    |

Outputs: `pushed-key` (the key that was written — the base key, or the derived
`_staging` variant), `store` (the resolved logical store id).

A staged deploy plus a staged config push and a healthcheck compose the same way
the lifecycle trio does (§5.4.4): push staging config, deploy the staged version,
probe it, roll back on failure.

## 6. Execution flow (engine)

1. Verify the runner is Linux x86-64 (`ubuntu-24.04` is the tested environment).
2. Validate that `adapter` is a well-formed, non-empty token. The engine does
   **not** enumerate the CLI's compiled adapters (there is no introspection
   command); an unsupported adapter surfaces as the CLI's own error at build or
   deploy time.
3. Validate exact boolean inputs.
4. Download the `app-cli-artifact` (a tar) into an action-owned directory below
   `RUNNER_TEMP`, extract it preserving permissions (or `chmod +x <app-cli-bin>`),
   read `app-cli-meta.json` for `app-cli-bin`/`app-cli-version` (a wrapper `app-cli-bin` input
   overrides), and prepend the directory to `PATH` for action steps only.
5. Parse `build-args`, `deploy-args`, `deploy-flags`, `provider-env-clear` as
   JSON string arrays. **`provider-env` is not present in these steps** — it is
   scoped to the deploy step's own `env:` and parsed only there (step 17), so
   setup/build never hold the secret-bearing blob.
6. In every non-deploy step (setup, build), unset each name in
   `provider-env-clear` (defense-in-depth against inherited caller `env:`).
7. Reject NUL-containing argument or value entries.
8. Apply the adapter's deploy-arg allowlist (wrapper-provided) to `deploy-args`.
9. Resolve `working-directory` beneath `github.workspace` using canonical paths
   and symlink resolution; fail if missing or not a directory.
10. If `manifest` is non-empty: resolve it under `working-directory`, fail if it
    escapes `github.workspace` or is not a regular file, and export
    `EDGEZERO_MANIFEST`.
11. Resolve the application **Git root**; record `source-revision`; fail on a
    dirty working tree (`build-app-cli` used an isolated `CARGO_TARGET_DIR`, so its
    CLI build did not dirty this tree).
12. Resolve the **Cargo workspace root** for `working-directory` (§11.1) for all
    Cargo-scoped operations that follow.
13. Resolve the application Rust toolchain (§7) and install it plus the
    **wrapper-provided** application `target` (Fastly → `wasm32-wasip1`). The
    engine does not map `adapter` → target.
14. If `cache: true`, restore the exact-key **Cargo workspace root** `target/`
    cache.
15. Print non-sensitive diagnostics.
16. Resolve `build-mode` (§8). If `always`, run
    `<app-cli-bin> build --adapter <adapter> -- <build-args…>` with **no** provider
    credentials in scope (the `provider-env-clear` names stay unset here).
17. In a separate deploy step whose step-scoped `env:` is the **only** place
    `provider-env` is exposed: clear the `provider-env-clear` aliases, parse
    `provider-env` and export only its values, and run:

    ```text
    <app-cli-bin> deploy --adapter <adapter> -- <deploy-flags…> <deploy-args…>
    ```

    For adapters whose deploy also compiles the application (Fastly's default),
    this step builds application code with credentials in scope — see §10.1.

18. Clean action-owned temporary tool, auth, log, and cache state with
    `if: always()`.
19. Save the application cache when enabled and safe.
20. Set outputs and write a non-sensitive GitHub step summary with
    `if: always()`.

When an argument array is empty, the trailing `--` may be omitted.

## 7. Toolchain resolution

Application Rust toolchain resolution precedence:

1. explicit `rust-toolchain` input when not `auto`;
2. nearest `rust-toolchain.toml` or `rust-toolchain`, walking from
   `working-directory` to the application Git root;
3. nearest `rust` entry in `.tool-versions` over the same path; and
4. the EdgeZero repository root `.tool-versions` `rust` entry.

At each directory, Rustup-native files take precedence over `.tool-versions`.
Malformed toolchain files fail instead of silently selecting a different
compiler. `build-app-cli` uses the same precedence for the CLI build.

## 8. Build behavior

| Value    | Behavior                                                         |
| -------- | ---------------------------------------------------------------- |
| `auto`   | Apply the adapter's default policy.                              |
| `always` | Run `<cli> build --adapter <adapter>` before deploy.             |
| `never`  | Skip the separate build; deploy builds or consumes the artifact. |

Fastly `auto` resolves to `never` because `<cli> deploy --adapter fastly`
builds unless a prebuilt package is provided. Other adapters define their own
default policy in their wrapper.

## 9. Passthrough arguments

`build-args`, `deploy-args`, and `deploy-flags` are JSON arrays so argument
boundaries are explicit:

```yaml
with:
  build-args: '["--verbose"]'
  deploy-args: '["--comment", "deployed by GitHub Actions"]'
```

`deploy-core` must:

- parse arrays with `jq` (no Python);
- reject non-arrays, non-string entries, and NUL bytes;
- construct commands as Bash arrays;
- never use `eval`; and
- avoid printing raw JSON inputs during validation.

Each adapter wrapper supplies an allowlist for caller `deploy-args`. For Fastly,
`deploy-args` are allowlisted to `--comment VALUE` / `--comment=VALUE`; all other
deploy args are rejected so caller input cannot override typed service selection,
authentication, non-interactive mode, endpoint, profile, or debug behavior. The
allowlist ships with accept/reject tests.

### 9.1 `deploy-args` under `--stage`

A staged deploy does not run `fastly compute deploy`; it runs
`fastly compute update` against a cloned draft version (§5.4). Flags that only
exist on `compute deploy` — `--env`, `--domain`, `--status-check-*`, `--dir` /
`-C`, `--no-default-domain` — are therefore **no-ops under `--stage`**: the
adapter drops them with a warning rather than passing an unsupported flag to
`compute update`.

`--comment` is a special case. `compute update` has no `--comment`, so the
adapter applies the comment out-of-band via `fastly service-version update
--comment …` **before** staging the version, preserving the caller's intent
without breaking the upload.

## 10. Provider credential contract

Credentials flow through the `provider-env` JSON object, which `deploy-core`
injects **only** into the deploy step:

```text
wrapper inputs (typed) → provider-env {NAME: value} → deploy step env → CLI
```

Rules:

- **`provider-env` is bound only to the deploy step's own `env:`** — a
  step-scoped environment, not a job/engine-global variable. Setup, `build-app-cli`,
  and separate build steps never receive the `provider-env` variable at all, so
  the secret-bearing blob is absent from their environments (not merely unset
  after the fact). The engine parses `provider-env` only inside the deploy step.
- Alias clearing is **wrapper-driven and provider-agnostic**, and is
  defense-in-depth for a _different_ threat — a caller who sets provider aliases
  through their own workflow `env:`. The engine unsets the wrapper-supplied
  `provider-env-clear` names in non-deploy steps, and clears them in the deploy
  step before exporting only the `provider-env` values. The engine hard-codes no
  provider names, so caller `env:` cannot override the typed contract.
- `provider-env` values never reach `GITHUB_ENV`, `GITHUB_OUTPUT`, caches, or
  summaries.
- **The engine's own private environment is scrubbed before the CLI is exec'd.**
  The wrapper necessarily carries the token into the deploy step twice — once as
  `EDGEZERO__<PROVIDER>_API_TOKEN` (so the step's YAML can build the JSON without
  interpolating a secret into a `run:` block, itself a template-injection sink),
  and once inside `EDGEZERO__PROVIDER_ENV`. Both are secret-bearing. `run-app-cli.sh`
  therefore unsets its entire private namespace (§10.2) after reading it, so the
  app CLI — and every subprocess it spawns, including a manifest
  `[adapters.*.commands]` shell command — receives **only** the typed provider
  aliases plus `EDGEZERO_MANIFEST`. Without this, an `env`-dumping build script
  would print the raw token under a name we never promised.
- Every step of a wrapper — shell steps and third-party `uses:` steps alike —
  blanks the full alias list in its own `env:`. The list a wrapper blanks and the
  list it passes as `provider-env-clear` are the same list.

Application (non-credential) configuration may still pass through normal
workflow `env:`.

### 10.1 Action-owned passthrough arguments

The deploy-arg allowlist (§9) governs **caller** input. A wrapper may also
prepend args of its own, which are not caller input and so are not
allowlist-checked. For Fastly that is `--non-interactive`.

It is needed because the two deploy paths differ. The built-in Fastly deploy
appends `--non-interactive` for itself, but a deploy declared as a manifest
command (`[adapters.fastly.commands] deploy = "fastly compute deploy"` — a
documented, common configuration) is run as a shell command with the adapter args
appended verbatim, so nothing would add it and the deploy could block on a TTY
prompt in CI. Supplying it from the wrapper covers both paths; the built-in path
de-duplicates it (both `--non-interactive` and `-i`).

Action-owned args are prepended, so a caller arg still wins where the provider
CLI takes last-wins. A caller cannot smuggle one in through `deploy-args` — the
allowlist still rejects it.

### 10.2 Environment variable convention

Every variable the actions own lives in one namespace:

```text
EDGEZERO__<SECTION>__<NAME>
         ^^        ^^
         `__` separates sections; `_` separates words within a section.
```

Sections in use: `ACTION` (action-owned dirs), `INPUT` (raw wrapper inputs),
`PROJECT` (working directory, manifest), `CLI` (the app CLI artifact), `BUILD` /
`DEPLOY` (argument files), `PROVIDER` (the credential JSON and its clear list),
`RUNNER`, `LIFECYCLE` (healthcheck/rollback parameters), `SUMMARY`, `FASTLY`
(provider-specific), and `TEST`.

This is not cosmetic. It is what makes the credential boundary (§10) a **single
rule**: `run-app-cli.sh` unsets everything matching `EDGEZERO__*` before exec'ing the
app CLI. With the previous mix of `DEPLOY_*`, `INPUT_*`, `SUMMARY_*`, and bare
`CLI_BIN` / `VERSION`, the scrub needed a hand-maintained list — and a variable
added later without touching that list would have silently leaked.

`EDGEZERO_MANIFEST` (**single** underscore) is deliberately outside the
namespace: it is the CLI's own public contract, not an action variable, and it is
the one variable the actions deliberately pass through.

### 10.3 Build-in-deploy caveat (trusted source requirement)

Some adapters compile the application **inside** the deploy step. Fastly's
default `build-mode: never` relies on `<cli> deploy`, which runs
`fastly compute deploy` — and that command builds the application unless a
prebuilt package already exists. Consequently, with the Fastly default, the
application is compiled while `FASTLY_API_TOKEN` is in scope.

This is an explicit, accepted boundary, not an oversight:

- The action still guarantees credentials are absent from setup, `build-app-cli`,
  and any separate `build-mode: always` build step.
- Because deploy may still recompile, a credential-free `always` prebuild does
  not remove the exposure; it only front-loads a validation build.
- Therefore callers **must** deploy only trusted, immutable source refs (full
  SHAs or protected tags) and use GitHub Environment approvals, so untrusted
  code never runs with the token in scope.

Adapters that support a genuinely credential-free prebuild followed by a
credential-only publish may set a different default in their wrapper; Fastly does
not today.

## 11. Caching

`cache` enables opt-in application build caching, `false` by default.

### 11.1 Git root vs Cargo workspace root

Two distinct roots are resolved from `working-directory`, and they are **not**
interchangeable:

- **Git root** — the enclosing repository. Used only for `source-revision` and
  the dirty-source guard.
- **Cargo workspace root** — resolved with `cargo locate-project --workspace`
  (or `cargo metadata`) from `working-directory`. Owns the real `Cargo.lock` and
  the real `target/` directory. In a monorepo or nested workspace this is often
  under `working-directory` (for example `apps/api/`), not the Git root.

Cargo-scoped operations — lockfile hashing, lockfile presence checks, and
`target/` caching — use the **Cargo workspace root**. Git-scoped operations use
the **Git root**.

The **application's Git root is also the toolchain search boundary.**
`build-app-cli` resolves the Rust toolchain by walking up from
`working-directory` looking for `rust-toolchain.toml`, `rust-toolchain`, then
`.tool-versions` — and that walk stops at the app's Git root, never at
`github.workspace`. In the separate-repository layout the deployer repo sits at
`github.workspace` with the application checked out into a subdirectory; walking
to `github.workspace` would cross the app's Git boundary and let the _deployer's_
`.tool-versions` silently decide which Rust compiles the application. Every path
is canonicalized before comparison, because a symlinked `TMPDIR` or checkout
would otherwise never match the boundary and the walk would climb straight past
it. When the app directory is not a Git checkout, the boundary falls back to
`github.workspace`; the walk never rises above it either way.

### 11.2 Cache contents and key

When enabled, cache only the resolved **Cargo workspace root** `target/`. Never
cache provider auth files, action-owned tool installs, logs, temporary deploy
state, or paths outside that `target/`.

The cache key is exact and includes at least: runner OS, runner architecture,
resolved Rust toolchain, resolved `target`, CLI version, application source
revision, and the **Cargo workspace root** `Cargo.lock` hash. No broad restore
prefixes. If `cache: true` and that lockfile is missing, fail before deployment
with a remediation message.

## 12. Logging and summary

Log and summarize non-sensitive facts only: adapter, workspace-relative
application directory, source revision, manifest path or default discovery, Rust
toolchain and target, CLI version, requested/effective build mode, cache
enabled/disabled and key fingerprint, and final result.

Never log provider credentials, full process environments, application secret
values, or provider auth state.

### 12.1 Action-owned logs are private and transient

The deploy, healthcheck, and rollback wrappers tee the CLI's combined output to a
file so they can parse a canonical `version=<N>` / `healthy=<bool>` line out of
it. Provider CLIs print request URLs and service metadata, and under debug flags
can print credential material — so that file is created with `mktemp` at mode
`600` and removed by an `EXIT` trap whatever the outcome. It is never left behind
in `RUNNER_TEMP` for a later step in the job to read.

Canonical lines are matched with a **fully anchored** pattern (`^<key>=[0-9]+$`).
A prefix match reads `version=15.2.0` as `15` and `version=12abc` as `12` —
threading a version that was never deployed into the healthcheck and rollback that
follow. If the value is not exactly digits, the version has not been parsed, it
has been guessed; the wrapper fails instead. The CLI's own parser is anchored the
same way, and falls back to the Fastly API rather than guessing.

### 12.2 Cleanup removes only what the action owns

Cleanup runs `rm -rf`, so it removes only real paths strictly beneath
`RUNNER_TEMP`, resolving symlinks before comparing. A directory named by an
inherited variable — or any path outside the action-owned temp root — is refused
with a diagnostic, not deleted. Every wrapper that installs a tool or extracts the
CLI artifact runs cleanup with `if: always()`.

## 13. Error handling

All validation and setup failures stop before invoking provider deployment.

| Failure                                         | Required diagnostic                                                             |
| ----------------------------------------------- | ------------------------------------------------------------------------------- |
| Missing/unknown `app-cli-package`               | State that the app must name a CLI package present in its own workspace.        |
| Missing `app-cli-artifact`                      | State that a compiled CLI artifact from `build-app-cli` is required.            |
| Malformed `adapter` token                       | Name the input and its allowed shape (the CLI validates support at run time).   |
| Invalid boolean                                 | Name the input and allowed values.                                              |
| Missing working directory                       | Print the workspace-relative requested path.                                    |
| Path escapes workspace                          | Name the input; require paths under `github.workspace`.                         |
| Missing explicit manifest                       | Print the workspace-relative requested path.                                    |
| Invalid JSON arguments/env                      | Name the invalid input without printing its value.                              |
| Non-string entry                                | State that every array/object value must be a string.                           |
| Disallowed deploy arg                           | State the allowlist and rejected position without printing the array.           |
| Rust toolchain cannot be resolved               | List files checked and suggest explicit `rust-toolchain`.                       |
| Dirty working tree                              | State that deployments require committed source.                                |
| Missing `Cargo.lock` when cache enabled         | Explain the exact-key cache requirement.                                        |
| Missing provider credential input               | Name the missing input, never its value.                                        |
| Build command fails                             | Preserve exit status; state that deploy was not attempted.                      |
| Deploy command fails                            | Preserve exit status; state that rollback is caller-owned.                      |
| Staged deploy fails                             | Preserve exit status; emit no `fastly-version` so the caller skips rollback.    |
| Missing `fastly-version` (healthcheck/rollback) | State it is required, sourced from the deploy/stage output.                     |
| Health check unhealthy after retries            | Exit non-zero and set `healthy=false`/`status-code` so the caller can rollback. |
| Rollback command fails                          | Preserve exit status; state the version was not rolled back.                    |
| Cleanup fails                                   | Mark the action failed; identify the area without printing secrets.             |

Provider CLI stderr passes through so provider API errors stay actionable. The
actions never construct error messages containing credentials.

## 14. Security requirements

1. Recommend readable released tags for third-party actions and, for production,
   full commit SHAs of the EdgeZero action ref where reproducibility matters.
2. Compile the CLI package the application provides, from the application
   checkout and its lockfile; do not build the EdgeZero monorepo CLI or the
   action's own revision.
3. Compile the CLI once in `build-app-cli`; deploy steps consume the artifact and
   never recompile.
4. Download provider tools and validation binaries only from official release
   locations and verify SHA-256 checksums.
5. Install action-owned binaries below `RUNNER_TEMP`.
6. Use Bash arrays; never use `eval`; never use Python.
7. Allow-list `adapter` before using it in file selection or command arguments.
8. Treat the checked-out application and `edgezero.toml` as executable code.
9. Scope provider credentials to the step that mutates or reads the provider.
   The generic deploy step receives them as data via `provider-env` (the engine
   hard-codes no provider names). The provider-specific lifecycle steps —
   rollback-target capture, healthcheck, rollback, and config-push — instead
   receive the token DIRECTLY under the adapter's own convention
   (`FASTLY_API_TOKEN`, what the Fastly CLI/API read), because they call the
   adapter, not the generic engine. Every OTHER inherited provider alias is
   blanked in those steps, so only the one typed token is in scope.
10. Never write provider credentials to `GITHUB_ENV`, `GITHUB_OUTPUT`, caches, or
    summaries.
11. Clear the wrapper-supplied `provider-env-clear` aliases from non-provider
    steps; the engine hard-codes no provider names.
12. Reject caller paths outside `github.workspace`, including symlink escapes.
13. Escape percent, carriage return, and newline characters before emitting
    user-influenced GitHub annotations or masking commands; reject CR/LF in
    single-line output values.
14. Disable caching by default; use exact keys only when enabled.
15. Do not auto-retry provider DEPLOYMENT (a deploy mutation runs at most once).
    Retries are confined to idempotent operations: installer downloads and the
    read-only healthcheck probe (`retry` attempts, for BOTH production and
    staging probes — they share one retry loop).
16. Do not use `github.token` for provider authentication.
17. Document least-privilege workflow permissions (`contents: read` unless the
    caller needs more) and caller-owned environment protection, concurrency, and
    timeouts.

## 15. Testing strategy

### 15.1 Static validation (no Python)

CI for the actions runs:

- `actionlint` from a **pinned release binary** over workflow and action files;
- `shellcheck` over shell scripts;
- YAML parsing for each `action.yml`;
- metadata contract tests for public inputs/outputs, ported into the Bash
  `tests/run.sh` harness (replacing the previous `python3` heredocs);
- a check that action tool versions agree with `.tool-versions`;
- `zizmor` from a **pinned release binary** (Rust; installed as a release
  artifact or via `cargo install zizmor --locked`, never `pip`); and
- Markdown/example validation.

Third-party actions used by CI (`actions/checkout`, `actions/cache`, artifact
upload/download) are pinned to readable released tags.

### 15.2 Script contract tests (Bash)

Use temporary directories and fake binaries to test, across the engine and the
Fastly wrapper:

- `adapter` well-formedness validation (unknown adapters surface as the CLI's
  own error, not an engine allowlist);
- app-provided `app-cli-package` build (fail on missing/unknown package), tar
  round-trip preserving the executable bit, and artifact consumption;
- exact boolean parsing;
- toolchain precedence and malformed-file failure;
- working-directory confinement and symlink-escape rejection;
- dirty-source rejection and source-revision output;
- explicit and default manifest behavior;
- JSON argument/env parsing and boundary preservation;
- rejected non-string and NUL-containing entries;
- adapter deploy-arg allowlist (accept `--comment`, reject service/auth/endpoint/
  profile/interactive/short-flag/debug overrides);
- build-mode resolution and build-failure-prevents-deploy;
- deploy exit-code propagation;
- credential presence validation and scoping (absent from build-app-cli/setup/build,
  present only in the provider-calling steps: deploy and the Fastly lifecycle
  steps such as rollback-target capture);
- cache key construction and missing-lockfile failure;
- staging lifecycle: `stage` flag adds `--stage`; `fastly-version` parsed from CLI
  output; `healthcheck-fastly` / `rollback-fastly` pass `--service-id` + version
  and scope `FASTLY_API_TOKEN`; healthcheck exits non-zero on unhealthy; staging
  vs production argv;
- cleanup on success and failure; and
- redaction of credentials from action-owned logs.

Tests must not need live provider credentials.

### 15.3 Composite smoke test

A workflow exercises the layered actions end to end with a minimal fixture
EdgeZero app: run `build-app-cli`, then `deploy-fastly` (both production and
`stage: true`), then `healthcheck-fastly` and `rollback-fastly`. Fake the
dependencies each action actually uses:

- for `deploy-fastly`, a fake `fastly` binary that writes marker files and prints
  a version instead of contacting Fastly;
- for `healthcheck-fastly` / `rollback-fastly`, a fake **app CLI** (or stubbed
  Fastly API / `curl` responses) — these actions call the Fastly API, not the
  `fastly` CLI, so no fake `fastly` binary is involved.

Assert CLI-artifact reuse, invocation order, working directory, argument
boundaries (`--service-id`, `--stage`), `fastly-version` threading stage →
healthcheck → rollback, cache behavior, credential scope, and public outputs.

### 15.4 Installer / live gates

- A path-filtered CI gate (`fastly-installer-check.yml`) verifies the pinned
  Fastly CLI installer still produces a runnable binary matching the expected
  version, without deploying. It runs on PRs/pushes that touch the installer, its
  pinned `versions.json`, or the shared `common.sh` — validating a version / URL
  / checksum bump against the real release exactly when it changes, rather than on
  a fixed schedule that would download the CLI on unrelated runs.
- A protected manual workflow may eventually deploy a disposable Fastly fixture
  before any stable version alias is created; it runs only from protected
  branches or approved dispatch, never from fork PRs, uses isolated resources,
  and treats rollback/cleanup as caller-owned.

## 16. Documentation requirements

User-facing docs must cover: the three-layer model and when to use each action;
how `build-app-cli` compiles the app-provided CLI package; supported adapters and how new adapters
layer on; runner support; same-repo, separate-repo, and monorepo checkout
examples; complete input/output tables per action; typed provider credential
guidance and why credentials must not pass through caller `env:`; build-mode and
cache behavior with security caveats; least-privilege permissions and
environment/concurrency/timeout recommendations; explicit non-goals; and future
adapter notes.

## 17. Acceptance criteria

The design is implemented when:

1. A caller can compile the CLI once with `build-app-cli` and deploy a checked-out
   EdgeZero application with `deploy-fastly`, reusing the same CLI artifact.
2. `build-app-cli` compiles the app-provided `app-cli-package` from the application
   checkout and never builds the EdgeZero monorepo CLI or the action's own
   revision.
3. `deploy-core` contains no provider-specific credential names, service
   concepts, endpoints, or CLI flags — only `provider-env`, `provider-env-clear`,
   `deploy-flags`, and `deploy-args` carry them.
4. Adding a second adapter is a new minimal wrapper plus target/allowlist data,
   with no engine fork.
5. Deploy steps consume the prebuilt CLI artifact and never recompile it.
6. Typed provider credentials reach only the steps that call the provider — the
   deploy, and the Fastly lifecycle steps (rollback-target capture, staging
   healthcheck, rollback, config-push) — and never appear in outputs, caches,
   action-owned logs, or summaries. A production healthcheck needs no token and
   receives none.
7. Passthrough argument boundaries are preserved; no `eval`.
8. `cache: true` uses exact keys and caches only the **Cargo workspace root**
   `target/` (§11.1), so nested-workspace monorepos cache the right artifacts.
9. All CI, tooling, and tests run without Python; `actionlint` and `zizmor` run
   from pinned release binaries.
10. Third-party actions are pinned to readable released tags.
11. Fastly staging lifecycle works end to end: `deploy-fastly` `stage: true`
    stages a draft and outputs `fastly-version`; `healthcheck-fastly` probes the
    staged version (via its staging IP) and exits non-zero when unhealthy;
    `rollback-fastly` deactivates the staged version (or, for production,
    activates the `rollback-to` version captured from `deploy-fastly`'s
    `previous-version` output). All three thread `--service-id` and
    `fastly-version` and scope `FASTLY_API_TOKEN`; the generic engine is
    unchanged.
12. Static checks, Bash contract tests, and the composite smoke test pass.
13. Docs include same-repo, separate-repo, and monorepo examples across the
    three-layer model, plus a Fastly staging-lifecycle example.

## 18. Risks and mitigations

| Risk                                                  | Mitigation                                                                                         |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| CLI and application manifest schema incompatible      | CLI is the app's own package, built from the app checkout, so they cannot diverge.                 |
| Provider deploy builds while credentials are in scope | Keep the separate build credential-free; document caching caveats; require trusted immutable refs. |
| Mutable refs execute unexpected manifest commands     | Caller owns checkout; document tag/SHA protection and GitHub Environment approvals.                |
| Caching stores sensitive generated output             | Disable by default; exact keys only; cache only `target/`.                                         |
| Provider CLI installer changes or disappears          | Pin versions and checksums; a path-filtered gate runs the real installer when its pins change.     |
| Monorepo has multiple provider manifests              | Require deterministic `working-directory` or explicit `edgezero.toml`; the actions do not guess.   |
| Engine grows provider-specific behavior               | Keep provider concepts in wrappers and the CLI; keep `deploy-core` provider-neutral.               |

## 19. Future work

1. Cloudflare Workers deployment (`deploy-cloudflare` wrapper).
2. Spin/Fermyon Cloud preview deployment (`deploy-spin` wrapper).
3. Staging / health-check / rollback lifecycle for adapters **beyond Fastly**
   (Fastly's is delivered in §5.4).
4. Optionally consume a prebuilt/attested CLI binary matching the application's
   pinned version instead of compiling from source.
5. Release artifact reuse between build and deploy jobs beyond the CLI.
6. Stable version aliases such as `v1`.
7. Linux arm64, macOS, or other runner support.

## 20. References

- EdgeZero CLI reference: `docs/guide/cli-reference.md`
- EdgeZero Fastly adapter: `crates/edgezero-adapter-fastly/src/cli.rs`
- EdgeZero CLI dispatch: `crates/edgezero-cli/src/main.rs`
- Fastly Compute deploy reference: <https://www.fastly.com/documentation/reference/cli/compute/deploy/>
- GitHub Actions secure use reference: <https://docs.github.com/en/actions/security-guides/security-hardening-for-github-actions>
