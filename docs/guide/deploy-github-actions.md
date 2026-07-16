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
- **Typed provider credentials.** Pass `fastly-api-token` / `fastly-service-id`
  through the wrapper inputs — never through workflow `env:`. They reach only the
  deploy step.

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

| Input               | Required | Default         | Meaning                                                         |
| ------------------- | -------- | --------------- | --------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                            |
| `fastly-api-token`  | Yes      | —               | Injected only into the deploy step.                             |
| `fastly-service-id` | Yes      | —               | Passed as the typed `--service-id` flag.                        |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                |
| `working-directory` | No       | `.`             | App directory.                                                  |
| `manifest`          | No       | empty           | Optional `edgezero.toml` path relative to `working-directory`.  |
| `build-mode`        | No       | `auto`          | `auto` (→ `never` for Fastly), `always`, or `never`.            |
| `build-args`        | No       | `[]`            | JSON array passed to `<cli> build`. No secrets.                 |
| `deploy-args`       | No       | `[]`            | JSON array — allowlisted to `--comment` for Fastly. No secrets. |
| `stage`             | No       | `false`         | Deploy to a staged draft version instead of activating.         |
| `cache`             | No       | `false`         | Exact-key Cargo-workspace `target/` caching.                    |

Outputs: `fastly-version`, `source-revision`, `app-cli-version`.

The action always adds `--non-interactive` to the deploy itself, so a deploy
declared as an `edgezero.toml` command (`[adapters.fastly.commands] deploy =
"fastly compute deploy"`) cannot block on a prompt in CI. You do not need to —
and cannot — pass it through `deploy-args`.

### `healthcheck-fastly`

| Input               | Required | Default         | Meaning                                                       |
| ------------------- | -------- | --------------- | ------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                          |
| `fastly-api-token`  | Yes      | —               | Needed to resolve a staged version's IP.                      |
| `fastly-service-id` | Yes      | —               | Service to probe.                                             |
| `fastly-version`    | Yes      | —               | Version to probe — thread the deploy's `fastly-version`.      |
| `domain`            | Yes      | —               | Domain to probe, e.g. `www.example.com`.                      |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                              |
| `deploy-to`         | No       | `production`    | `staging` probes the staged version via its resolved edge IP. |
| `retry`             | No       | `3`             | Attempts before declaring the deployment unhealthy.           |
| `retry-delay`       | No       | `5`             | Seconds between attempts.                                     |
| `timeout`           | No       | `10`            | Per-attempt timeout in seconds.                               |

Outputs: `healthy`, `status-code`.

**This action fails when the deployment is unhealthy** — that is the point. Gate
your rollback on the step failing (`if: failure()`), not on the `healthy` output.

### `rollback-fastly`

| Input               | Required | Default         | Meaning                                                                            |
| ------------------- | -------- | --------------- | ---------------------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                               |
| `fastly-api-token`  | Yes      | —               | Fastly API token.                                                                  |
| `fastly-service-id` | Yes      | —               | Service to roll back.                                                              |
| `fastly-version`    | Yes      | —               | The current (bad) version to roll back **from**.                                   |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                                   |
| `deploy-to`         | No       | `production`    | `production` activates the previous version; `staging` deactivates the staged one. |

Outputs: `rolled-back-to` (production only — the version that was activated).

### `config-push-fastly`

Pushes your app's typed config to a Fastly config store. This is **separate from
deploy** — deploy activates code, it never writes runtime config — so you run it
as its own step, whenever config should move.

| Input               | Required | Default         | Meaning                                                             |
| ------------------- | -------- | --------------- | ------------------------------------------------------------------- |
| `app-cli-artifact`  | Yes      | —               | The `build-app-cli` artifact to run.                                |
| `fastly-api-token`  | Yes      | —               | Fastly API token. Injected only into the push step.                 |
| `app-cli-bin`       | No       | artifact's name | Binary name inside the artifact.                                    |
| `working-directory` | No       | `.`             | App directory (holds the manifest + typed config).                  |
| `manifest`          | No       | empty           | `edgezero.toml` path relative to `working-directory`.               |
| `app-config`        | No       | empty           | Typed config file path (default: resolved from the manifest).       |
| `store`             | No       | empty           | Logical config-store id (default: the manifest's resolved id).      |
| `key`               | No       | empty           | Explicit base key (default: the logical store id).                  |
| `deploy-to`         | No       | `production`    | `staging` writes the `<key>_staging` variant in the **same** store. |

Outputs: `pushed-key` (the key written — the base key, or its `_staging` variant)
and `store` (the logical store id, when supplied).

**Staging config is the same store, a different key.** Fastly config stores are
not versioned like staged service versions, so `deploy-to: staging` writes your
config under `<key>_staging` alongside the production key — never overwriting what
the live service reads. Which key the service reads is decided separately by the
runtime-override store (`edgezero provision` scaffolds it). A typo like
`deploy-to: Staging` is rejected up front, never silently pushed to production.

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
production`, activates the previous version).

## Build behavior and caching

`build-mode: auto` resolves to `never` for Fastly, because `fastly compute
deploy` builds unless a prebuilt package is provided. `always` runs a separate
credential-free validation build first; the deploy may still recompile.

Caching is opt-in (`cache: false` by default) and, when enabled, caches only the
Cargo workspace root `target/` under an exact key (runner OS/arch, toolchain,
target, CLI version, source revision, and `Cargo.lock` hash). Enable it only for
trusted, immutable refs.

## Recommended job hardening

```yaml
permissions:
  contents: read
concurrency:
  group: deploy-${{ github.ref }}
  cancel-in-progress: false
```

Add `timeout-minutes`, a protected GitHub Environment with required reviewers,
and pin third-party actions to readable released tags (or full SHAs for
production).

## Non-goals

The actions do not check out source, expand or convert configuration, or push
runtime config as a side effect of deploy. Config push and provisioning are
explicit CLI subcommands (`edgezero config push`, `edgezero provision`) you run
as separate steps. Cloudflare and Spin deploy wrappers are future work; today
these actions target Fastly.
