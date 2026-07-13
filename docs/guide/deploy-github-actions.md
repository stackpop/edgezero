# Deploy with GitHub Actions

EdgeZero includes a pre-release composite GitHub Action for deploying a checked-out application to Fastly Compute.

::: warning Pre-release
The deploy action is pre-release. Use a trusted ref; a full commit SHA is recommended when you need reproducible production deploys.
:::

## Action path

```yaml
uses: stackpop/edgezero/.github/actions/deploy@<ref>
```

The action lives inside the EdgeZero monorepo at `.github/actions/deploy`.

## Supported adapters

| Adapter      | Status                              |
| ------------ | ----------------------------------- |
| `fastly`     | Supported in v0                     |
| `cloudflare` | Future work                         |
| `spin`       | Future work                         |
| `axum`       | Not supported for remote deployment |

The `adapter` input is required and must be `fastly` in v0.

## Minimal same-repository workflow

```yaml
name: Deploy

on:
  workflow_dispatch:

permissions:
  contents: read

jobs:
  deploy:
    runs-on: ubuntu-24.04
    environment: production
    timeout-minutes: 30
    concurrency:
      group: fastly-production
      cancel-in-progress: false
    steps:
      - uses: actions/checkout@<full-commit-sha>
        with:
          persist-credentials: false

      - uses: stackpop/edgezero/.github/actions/deploy@<ref>
        with:
          adapter: fastly
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Separate deployer and application repositories

```yaml
steps:
  - name: Checkout deployer
    uses: actions/checkout@<full-commit-sha>
    with:
      path: deployer
      persist-credentials: false

  - name: Checkout application
    uses: actions/checkout@<full-commit-sha>
    with:
      repository: stackpop/my-edgezero-app
      ref: ${{ inputs.ref }}
      path: app
      persist-credentials: false

  - name: Deploy application
    uses: stackpop/edgezero/.github/actions/deploy@<ref>
    with:
      adapter: fastly
      working-directory: app
      fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
      fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Monorepo or non-root manifest

```yaml
- uses: stackpop/edgezero/.github/actions/deploy@<ref>
  with:
    adapter: fastly
    working-directory: apps/api
    manifest: edgezero.toml
    cache: true
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## Inputs

| Input               | Required | Default | Description                                                                     |
| ------------------- | -------- | ------- | ------------------------------------------------------------------------------- |
| `adapter`           | Yes      | none    | Must be `fastly`.                                                               |
| `working-directory` | No       | `.`     | App directory relative to `github.workspace`.                                   |
| `manifest`          | No       | empty   | Optional `edgezero.toml` path relative to `working-directory`.                  |
| `rust-toolchain`    | No       | `auto`  | Explicit Rust toolchain or automatic discovery.                                 |
| `build-mode`        | No       | `auto`  | `auto`, `always`, or `never`. Fastly `auto` resolves to `never`.                |
| `build-args`        | No       | `[]`    | JSON array passed after `edgezero build --adapter fastly --`.                   |
| `deploy-args`       | No       | `[]`    | JSON array of Fastly comment args appended after the action-owned deploy flags. |
| `cache`             | No       | `false` | Enables exact-key application `target/` caching.                                |
| `fastly-api-token`  | Yes      | none    | Fastly token, scoped to the deploy step.                                        |
| `fastly-service-id` | Yes      | none    | Fastly service ID used by the action-owned deploy flag.                         |

## Outputs

| Output                 | Description                                      |
| ---------------------- | ------------------------------------------------ |
| `adapter`              | Normalized adapter, `fastly`.                    |
| `source-revision`      | Git revision deployed from `working-directory`.  |
| `edgezero-revision`    | EdgeZero action/CLI revision used by the action. |
| `provider-cli-version` | Installed Fastly CLI version.                    |
| `effective-build-mode` | Resolved build behavior.                         |

## Credential scope

Pass Fastly credentials through typed inputs, not `env:`:

```yaml
with:
  fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
  fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

The action maps the token to `FASTLY_API_TOKEN` only for the deploy step, and passes the service ID as the action-owned Fastly `--service-id` deploy flag. Setup and separate build steps do not receive those typed credentials.

Application settings may still use workflow `env:` when the app genuinely needs them during build or deploy. Do not duplicate Fastly credentials in `env:`.

## EdgeZero CLI revision

The action builds and runs the `edgezero` binary from the EdgeZero repository revision selected by the `uses:` ref. It does not install the CLI from the application repository or from the application's Cargo dependencies. Use a full commit SHA when you need to pin the CLI implementation exactly.

## Build behavior

`build-mode` controls whether the action runs a separate EdgeZero build before deploy:

| Value    | Behavior                                                                                       |
| -------- | ---------------------------------------------------------------------------------------------- |
| `auto`   | Uses the Fastly v0 policy.                                                                     |
| `always` | Runs `edgezero build --adapter fastly` before deploy.                                          |
| `never`  | Skips the separate build and relies on Fastly deploy/publish to build or consume the artifact. |

For Fastly v0, `auto` resolves to `never` because Fastly `compute deploy`/`compute publish` builds unless a prebuilt package is supplied. Use `always` only when you want a separate validation build and can tolerate a possible second compile during deploy.

## Manifest selection

By default, the action runs in `working-directory` and leaves `EDGEZERO_MANIFEST` unset. EdgeZero then loads `edgezero.toml` from the working directory when present, or uses its built-in Fastly fallback.

Set `manifest` when the application manifest is not the default `working-directory/edgezero.toml`:

```yaml
with:
  adapter: fastly
  working-directory: apps/api
  manifest: edgezero.toml
```

The action does not guess between multiple `fastly.toml` files in a monorepo. Choose a deterministic `working-directory` or define explicit Fastly commands in `edgezero.toml`.

## Passthrough arguments

`build-args` and `deploy-args` are JSON arrays of strings. For v0, `deploy-args` is intentionally narrow: only Fastly deploy comments are accepted (`--comment VALUE` or `--comment=VALUE`). Service, auth, endpoint, interactive, debug, short override, and unknown future Fastly flags are rejected.

## Caching

Caching is disabled by default. When `cache: true`, the action caches only the application Git root `target/` directory with an exact key that includes the runner, Rust toolchain, Fastly target, EdgeZero revision, source revision, and `Cargo.lock` hash.

Enable caching only for trusted immutable refs and applications whose builds do not write secret-derived data into `target/`.

## Security and workflow policy

Use least-privilege permissions unless your workflow needs more:

```yaml
permissions:
  contents: read
```

Deployment jobs should use protected GitHub Environments, explicit timeouts, and per-environment concurrency with `cancel-in-progress: false` so two deployments do not race the same Fastly service.

Check out trusted immutable refs and use `persist-credentials: false` on checkout steps. EdgeZero manifests and provider commands are executable deployment code; do not deploy untrusted pull request refs with provider credentials.

## Fastly-specific behavior kept out of v0

The generic action does not expose a Fastly service version. If a workflow needs health checks or rollback, keep that logic in the caller workflow or a future Fastly-specific action. For production rollback, prefer recording the active Fastly version before deploy and reactivating it if a caller-owned health check fails.

## Non-goals

The deploy action does not check out source, provision Fastly resources, push secrets, stage deployments, perform health checks, roll back failures, or expose Fastly service versions.
