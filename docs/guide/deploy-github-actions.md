# Deploying from GitHub Actions

EdgeZero ships a set of reusable GitHub composite actions that deploy a
checked-out EdgeZero application to Fastly Compute. They are **layered** so that
adding another provider later does not rewrite the deploy engine, and the
**EdgeZero CLI is the boundary** — the actions never reproduce provider build or
deploy logic in YAML; they compile your CLI, scope credentials, and invoke it.

The design reference lives in
[`docs/specs/edgezero-deploy-github-action.md`](https://github.com/stackpop/edgezero/blob/main/docs/specs/edgezero-deploy-github-action.md);
this page is the practical how-to.

## The three layers

| Action               | Role                                                                                      |
| -------------------- | ----------------------------------------------------------------------------------------- |
| `build-app-cli`      | Compile the CLI package **your app provides** once, publish it as an artifact.            |
| `deploy-fastly`      | Deploy a checked-out Fastly app using that CLI artifact (production, or a staged draft).  |
| `healthcheck-fastly` | Probe a deployed/staged version; exit non-zero when unhealthy so you can gate a rollback. |
| `rollback-fastly`    | Production: activate the previous version. Staging: deactivate the staged version.        |

Under the hood a private `deploy-core` engine (a set of shared scripts) holds all
provider-neutral behavior; the wrappers above are thin.

**Runner support:** Linux x86-64 only (`ubuntu-24.04` is tested).

## What you provide

- **Checkout.** The actions never call `actions/checkout` — you own checkout, ref
  selection, permissions, environments, concurrency, and timeouts.
- **A CLI package.** Name a Cargo package in your own workspace (the crate that
  builds your `edgezero`-based CLI binary) via `app-cli-package`. `build-app-cli`
  compiles exactly that, from your checkout's `Cargo.lock`, so the CLI and your
  app can never disagree on schema.
  - **Required command surface.** The deploy actions drive your CLI, so it must
    expose the built-in commands they invoke: `build`, `deploy`, and — for the
    Fastly lifecycle — `active-version`, `healthcheck`, and `rollback` (plus
    `config` for `config-push-fastly`). The scaffolded template wires all of
    them; if you hand-write your CLI, dispatch each to its `edgezero_cli::run_*`
    handler. Two easy-to-miss requirements when hand-writing:
    - **Initialise the logger.** Call `edgezero_cli::init_cli_logger()` in `main`.
      The handlers print their machine-readable contract lines (`version=<N>`,
      `pushed-key=<key>`, `pushed-store=<id>`, `rolled-back-to=<N>`) via
      `log::info!`; without the logger they are swallowed, so the provider
      mutation SUCCEEDS and the wrapper then fails to parse the output.
    - **Route `config` through the TYPED path.** Dispatch `config push` to
      `run_config_push_typed::<YourAppConfig>` (and `validate`/`diff` likewise) —
      the bundled untyped path returns an unsupported error. It must emit BOTH
      `pushed-key=` and `pushed-store=`, which `config-push-fastly` requires.

    A production deploy runs `active-version` to capture the rollback target and
    fails fast (before touching the provider) if it is missing.

- **Typed provider credentials.** Pass `fastly-api-token` / `fastly-service-id`
  through the wrapper inputs — never through workflow `env:`. They reach only the
  steps that call the provider (the deploy, and the Fastly lifecycle steps such
  as rollback-target capture); a production healthcheck needs none.

## Quick start (same repository)

```yaml
jobs:
  deploy:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - id: cli
        uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
        with:
          app-cli-package: my-app-cli # the CLI crate in your workspace

      - uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

Use a trusted `@<ref>` — a released tag, or a full commit SHA when you need a
reproducible production deploy.

## Separate deployer and application repositories

Check the application into a path and point both actions at it. A **private** app
repository is not readable with the deployer job's default `GITHUB_TOKEN` — mint
an app-scoped token first (a GitHub App installation token, or a fine-grained PAT
with `contents: read`) and pass it to the application checkout.

```yaml
steps:
  - name: Checkout deployer
    uses: actions/checkout@v4
    with:
      path: deployer
      persist-credentials: false

  - name: Checkout application
    uses: actions/checkout@v4
    with:
      repository: stackpop/my-edgezero-app
      ref: ${{ inputs.ref }}
      path: app
      persist-credentials: false
      token: ${{ steps.app-token.outputs.token }} # app-scoped token

  - id: cli
    uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
    with:
      app-cli-package: my-app-cli
      working-directory: app

  - uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
    with:
      app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
      working-directory: app
      fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
      fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Monorepo application

Select the app subdirectory and, when needed, an explicit manifest. Caching keys
on the **Cargo workspace root** for that subdirectory (which in a nested
workspace may be the subdirectory itself), so a monorepo caches the right
`target/`.

```yaml
- id: cli
  uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
  with:
    app-cli-package: api-cli
    working-directory: apps/api

- uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    working-directory: apps/api
    manifest: edgezero.toml
    cache: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Inputs and outputs

### `build-app-cli`

| Input               | Required | Default             | Meaning                                                          |
| ------------------- | -------- | ------------------- | ---------------------------------------------------------------- |
| `app-cli-package`   | Yes      | —                   | Cargo package name of the CLI, in your app's workspace.          |
| `app-cli-bin`       | No       | `<app-cli-package>` | Binary name the package produces.                                |
| `working-directory` | No       | `.`                 | App directory (relative to `github.workspace`).                  |
| `rust-toolchain`    | No       | `auto`              | Explicit toolchain, or `auto` (rustup files → `.tool-versions`). |
| `app-cli-artifact`  | No       | `edgezero-cli`      | Uploaded artifact name.                                          |

Outputs: `app-cli-version`, `app-cli-package`, `app-cli-bin`, `app-cli-artifact`.

### `deploy-fastly`

| Input               | Required | Default         | Meaning                                                                                      |
| ------------------- | -------- | --------------- | -------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                         |
| `fastly-api-token`  | Yes      | —               | Injected only into the provider steps (rollback-target capture + deploy); blanked elsewhere. |
| `fastly-service-id` | Yes      | —               | Passed as the typed `--service-id` flag.                                                     |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                             |
| `working-directory` | No       | `.`             | App directory.                                                                               |
| `manifest`          | No       | empty           | Optional `edgezero.toml` path relative to `working-directory`.                               |
| `build-mode`        | No       | `auto`          | `auto` (→ `never` for Fastly), `always`, or `never`.                                         |
| `build-args`        | No       | `[]`            | JSON array passed to `<cli> build`. No secrets.                                              |
| `deploy-args`       | No       | `[]`            | JSON array — allowlisted to `--comment` for Fastly. No secrets.                              |
| `stage`             | No       | `false`         | Deploy to a staged draft version instead of activating.                                      |
| `cache`             | No       | `false`         | Exact-key Cargo-workspace `target/` caching.                                                 |

Outputs: `fastly-version`, `source-revision`, `app-cli-version`, and (production
only) `previous-version` — the version that was active _before_ this deploy.
Thread `previous-version` into `rollback-fastly`'s `rollback-to` so a later
rollback has a real target (Fastly cannot infer one — see `rollback-fastly`).

The action always adds `--non-interactive` to the deploy itself, so a deploy
declared as an `edgezero.toml` command (`[adapters.fastly.commands] deploy =
"fastly compute deploy"`) cannot block on a prompt in CI. You do not need to —
and cannot — pass it through `deploy-args`.

### `healthcheck-fastly`

| Input               | Required | Default         | Meaning                                                                                                        |
| ------------------- | -------- | --------------- | -------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                           |
| `fastly-api-token`  | Staging  | —               | Needed only for `deploy-to: staging` (staging-IP resolution); a production probe needs none and receives none. |
| `fastly-service-id` | Yes      | —               | Service to probe.                                                                                              |
| `fastly-version`    | Yes      | —               | Version to probe — thread the deploy's `fastly-version`.                                                       |
| `domain`            | Yes      | —               | Domain to probe, e.g. `www.example.com`.                                                                       |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                               |
| `deploy-to`         | No       | `production`    | `staging` probes the staged version via its resolved edge IP.                                                  |
| `retry`             | No       | `3`             | Attempts before declaring the deployment unhealthy.                                                            |
| `retry-delay`       | No       | `5`             | Seconds between attempts.                                                                                      |
| `timeout`           | No       | `10`            | Per-attempt timeout in seconds.                                                                                |

Outputs: `healthy`, `status-code`.

**This action fails when the deployment is unhealthy** — that is the point. Gate
your rollback on the step failing (`if: failure()`), not on the `healthy` output.

### `rollback-fastly`

| Input               | Required | Default         | Meaning                                                                                                                                                               |
| ------------------- | -------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                                                                                  |
| `fastly-api-token`  | Yes      | —               | Fastly API token.                                                                                                                                                     |
| `fastly-service-id` | Yes      | —               | Service to roll back.                                                                                                                                                 |
| `fastly-version`    | Yes      | —               | The current (bad) version to roll back **from**.                                                                                                                      |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                                                                                      |
| `rollback-to`       | No\*     | empty           | **Production only:** the version to re-activate. Wire it from `deploy-fastly`'s `previous-version` output. Required for `deploy-to: production`; ignored for staging. |
| `deploy-to`         | No       | `production`    | `production` activates `rollback-to`; `staging` deactivates the staged one.                                                                                           |

Outputs: `rolled-back-to` (production only — the version that was activated).

\* Fastly's version metadata cannot distinguish a previously-live version from a
staged draft, so a production rollback **cannot infer its target** — you must
supply it. Capture it at deploy time: `deploy-fastly` emits `previous-version`
(the version active _before_ that deploy), which you thread straight into
`rollback-to`. A production rollback with no `rollback-to` fails closed rather
than guess a version.

### `config-push-fastly`

Pushes your app's typed config to a Fastly config store. This is **separate from
deploy** — deploy activates code, it never writes runtime config — so you run it
as its own step, whenever config should move.

| Input               | Required | Default         | Meaning                                                                                                             |
| ------------------- | -------- | --------------- | ------------------------------------------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                                                                |
| `fastly-api-token`  | Yes      | —               | Fastly API token. Injected only into the push step.                                                                 |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                                                    |
| `working-directory` | No       | `.`             | App directory (holds the manifest + typed config).                                                                  |
| `manifest`          | No       | empty           | `edgezero.toml` path relative to `working-directory`.                                                               |
| `app-config`        | No       | empty           | Typed config file path (default: resolved from the manifest).                                                       |
| `store`             | No       | empty           | Logical config-store id (default: the manifest's resolved id).                                                      |
| `key`               | No       | empty           | Explicit base key for a **production** push (default: the logical store id). Not allowed with `deploy-to: staging`. |
| `deploy-to`         | No       | `production`    | `staging` writes the `<logical-store-id>_staging` variant in the **same** store.                                    |

Outputs: `pushed-key` (the key written — the base key, or the derived `_staging`
variant) and `store` (the logical store id, when supplied).

**Staging config is the same store, a different key.** Fastly config stores are
not versioned like staged service versions, so `deploy-to: staging` writes your
config under `<logical-store-id>_staging` alongside the production key — never
overwriting what the live service reads. The staging key is _derived_ from the
store's logical id, so `key` is production-only: combining `key` with
`deploy-to: staging` is rejected up front (an explicit staging key would be
written where no staged version ever reads).

What makes a _staged version_ actually read that key is the other half: a staged
deploy re-points its own `edgezero_runtime_env` link at a **per-service**
`edgezero_runtime_env_staging_<service-id>` selector store, mirroring
production's runtime overrides into it and redirecting only the config selectors
to `<logical-store-id>_staging`. The staged deploy creates and populates that
twin on demand — no separate setup step — so the staged version reads
`<logical>_staging` while production keeps reading `<logical>`. (The store is
named per service because Fastly config stores are account-wide and versionless,
so a shared twin could let one service's staged deploy clobber another's.) If the deploy
cannot even read the store listing (so it cannot tell whether production config
exists), it fails closed rather than risk serving production config. A typo like
`deploy-to: Staging` is likewise rejected up front, never silently pushed to
production.

## Strict lifecycle values (fail closed)

`stage` and `deploy-to` are validated exactly, and a bad value **fails the run**
rather than falling back to production:

- `stage` must be exactly `true` or `false`.
- `deploy-to` must be exactly `production` or `staging`.

A typo like `stage: True` or `deploy-to: Staging` is rejected up front — it will
never silently deploy to, probe, or roll back production.

## Credentials

Fastly credentials are typed inputs, not workflow `env:`. Setup and build steps
never see the token, and it never reaches outputs, caches, logs, or step
summaries. Do not duplicate provider credentials in `env:`; prefer
provider-managed runtime secret stores for application secrets.

The deploy step enforces a hard credential boundary: before the CLI runs, every
known provider alias (`FASTLY_TOKEN`, `FASTLY_ENDPOINT`, `FASTLY_API_URL`, …) is
**cleared**, and only the typed values you passed are exported. An inherited
`FASTLY_ENDPOINT` or `FASTLY_TOKEN` from the surrounding workflow cannot reach
the deploy.

Deploy runs trusted application code: because Fastly's default `build-mode:
never` lets `fastly compute deploy` build during deploy, the application is
compiled while the token is in scope. **Deploy only trusted, immutable refs**
(full SHAs or protected tags) and use GitHub Environment approvals.

## Fastly staging lifecycle

Staging parity with `stackpop/trusted-server-actions` is supported for Fastly.
The capability is scaffolded into the CLI's Fastly adapter and exposed through
your app CLI; the actions are thin wrappers. You wire the trio — the actions
carry no orchestration policy of their own.

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

- `deploy-fastly` with `stage: true` clones the active version, uploads the built
  package to a new draft, marks it staged, and outputs `fastly-version`.
- `healthcheck-fastly` resolves the staged version's Fastly staging IP and probes
  it, retrying and exiting non-zero when unhealthy.
- `rollback-fastly` deactivates the staged version (or, for `deploy-to:
production`, activates `rollback-to`).

### Production rollback needs an explicit target

A production version, once superseded, cannot be told apart from a staged draft
in Fastly's version metadata — so a production rollback **cannot infer** what to
re-activate. Capture the target at deploy time and thread it through:

```yaml
- id: deploy
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}

# ... run your production health checks here ...

- if: failure() && steps.deploy.outputs.previous-version != ''
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-cli-artifact: ${{ steps.cli.outputs.app-cli-artifact }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    fastly-version: ${{ steps.deploy.outputs.fastly-version }}
    rollback-to: ${{ steps.deploy.outputs.previous-version }}
```

`previous-version` is empty on a first-ever deploy (there is nothing to roll back
to), which is why the rollback step guards on it.

## Build behavior and caching

`build-mode: auto` resolves to `never` for Fastly, because `fastly compute
deploy` builds unless a prebuilt package is provided. `always` runs a separate
credential-free validation build first; the deploy may still recompile.

Caching is opt-in (`cache: false` by default) and, when enabled, caches only the
Cargo workspace root `target/` under an exact key (runner OS/arch, toolchain,
target, CLI version, source revision, and `Cargo.lock` hash). Enable it only for
trusted, immutable refs.

## Recommended job hardening

Serialize on the **Fastly service**, not the ref. Every deploy and rollback for a
service mutates the same live resource, so a service-scoped group is what
actually prevents two workflows (or two refs) from racing each other:

```yaml
permissions:
  contents: read
concurrency:
  # Service-scoped: all deploys AND rollbacks for this service run one at a time.
  group: fastly-${{ vars.FASTLY_SERVICE_ID }}
  cancel-in-progress: false
```

**Why this matters for rollback.** A production rollback checks that the version
it is rolling back _from_ is still the active one and refuses otherwise, so a
stale rollback will not clobber a much newer deploy. That check is **best-effort,
not atomic**: Fastly's activate endpoint takes no precondition, so a deploy that
lands in the window between the check and the activation can still be
overwritten. The guard only narrows that window.

Serialization closes it **only when every mutation shares one deployment
authority.** A GitHub `concurrency` group is scoped to a single repository's
runs, so it serializes deploys and rollbacks _within that repo_ — it cannot
serialize another repository's workflow, a `fastly` CLI run from a laptop, or a
Fastly-console activation. If more than one authority can activate versions on
the service, route them all through the same serialized workflow (or accept the
residual race).

Add `timeout-minutes`, a protected GitHub Environment with required reviewers,
and pin third-party actions to readable released tags (or full SHAs for
production).

## Non-goals

The actions do not check out source, expand or convert configuration, or push
runtime config as a side effect of deploy. Config push and provisioning are
explicit subcommands you run as separate steps — via the `config-push-fastly`
action, or your **app-owned** CLI's `<app-cli> config push` / `<app-cli> provision`
(the typed `config push` is only available on your app's CLI; the bundled
`edgezero` binary has no typed config in scope and returns an unsupported error).
Cloudflare and Spin deploy wrappers are future work; today these actions target
Fastly.
