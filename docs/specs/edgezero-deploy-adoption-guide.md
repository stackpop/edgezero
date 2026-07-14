# EdgeZero Deploy Actions — Adoption Guide

**Status:** Adoption guide for any EdgeZero application repository

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

The layered deploy actions are for **any** EdgeZero application repository, not a
single deployer. This guide describes the general adoption shape and then walks
through the Trusted Server deployer as one concrete migration example.

## 1. What a consumer gets

Composable actions:

- `build-app-cli` — compile the CLI package the application provides (a crate in the
  app's own workspace) once, publish it as an artifact;
- `deploy-fastly` — deploy a checked-out Fastly application using the prebuilt
  CLI artifact, to production or (with `stage: true`) a staged draft version;
- `healthcheck-fastly` / `rollback-fastly` — the Fastly staging lifecycle (§4);
- future `deploy-cloudflare` / `deploy-spin` wrappers over the same engine.

The actions own repeatable deploy setup and the Fastly staging mechanisms. The
consumer owns checkout, ref selection, permissions, environments, concurrency,
timeouts, and **orchestrating** the health-check / rollback flow.

## 2. Checkout layouts

The adapters your CLI supports come from your app's own `Cargo.toml`, so
`build-app-cli` takes no `adapters` input — it builds your CLI package as declared.

### 2.1 Same-repository application

The app and its deploy workflow live in one repo.

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
          cli-package: my-app-cli # the CLI crate in your app's workspace

      - uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          cli-artifact: ${{ steps.cli.outputs.artifact-name }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

### 2.2 Separate deployer and application repositories

A deployment repo drives a separate application repo. Check the app into a path
and point both actions at it.

> **Private app repos need their own token.** The deployer job's default
> `GITHUB_TOKEN` cannot read a _different_ private repository. Mint an app-scoped
> token first — e.g. with `actions/create-github-app-token` for a GitHub App
> installed on the app repo, or a fine-grained PAT with `contents: read` — and
> pass it to the application checkout's `token:`. The step below assumes an
> earlier `id: app-token` step produced `steps.app-token.outputs.token`.

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
      # MUST be a trusted, immutable ref (a full commit SHA, or a protected tag)
      # — never an arbitrary branch. Fastly's default `build-mode: never` means
      # `fastly compute deploy` COMPILES the application while the API token is
      # in scope, so untrusted code would run with your credentials (spec §10.1).
      ref: ${{ inputs.ref }}
      path: app
      persist-credentials: false
      # A private app repo is NOT readable with the deployer's default
      # GITHUB_TOKEN. Supply a token scoped to the app repo — a GitHub App
      # installation token (preferred) or a fine-grained PAT with
      # `contents: read` on the app repo:
      token: ${{ steps.app-token.outputs.token }}

  - id: cli
    uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
    with:
      cli-package: my-app-cli
      working-directory: app

  - uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
    with:
      cli-artifact: ${{ steps.cli.outputs.artifact-name }}
      working-directory: app
      fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
      fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

### 2.3 Monorepo application

Select the app subdirectory and, when needed, an explicit manifest. Caching
resolves `target/` and `Cargo.lock` at the **Cargo workspace root** for that
subdirectory (which in a nested workspace may be the subdirectory itself, not the
repo root), so a monorepo caches the right artifacts.

```yaml
steps:
  - uses: actions/checkout@v4
    with:
      persist-credentials: false

  - id: cli
    uses: stackpop/edgezero/.github/actions/build-app-cli@<ref>
    with:
      cli-package: api-cli
      working-directory: apps/api

  - uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
    with:
      cli-artifact: ${{ steps.cli.outputs.artifact-name }}
      working-directory: apps/api
      manifest: edgezero.toml
      cache: true
      fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
      fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

## 3. Consumer requirements

- Check out application source yourself; the actions never call
  `actions/checkout`.
- Provide a CLI package in your own workspace and name it via `cli-package`;
  `build-app-cli` compiles that package from your checkout, so the CLI and your app
  never disagree on schema. `build-app-cli` does not use the EdgeZero monorepo CLI.
- Provide typed provider credentials through wrapper inputs, not caller `env:`.
- Ensure the deployed ref has committed source (no dirty working tree) and a
  `Cargo.lock` at your app's **Cargo workspace root** (the workspace that owns
  `cli-package` — in a nested-workspace monorepo this may be your app
  subdirectory, not the repo root). `build-app-cli` requires it, and caching keys on
  it.
- Pin action references to readable released tags, or full SHAs for production
  reproducibility.
- Use least-privilege permissions (`contents: read`), protected environments,
  `timeout-minutes`, and appropriate concurrency.

## 4. Fastly staging lifecycle

For Fastly, staging deploy, health checks, and rollback are supported as a
provider-specific trio, scaffolded into the CLI and exposed through your app CLI:

- `deploy-fastly` with `stage: true` — deploy to a **staged** draft version
  (Fastly `service-version stage`) instead of activating production; outputs
  `fastly-version`.
- `healthcheck-fastly` — verify a version; for staging it resolves the Fastly
  staging IP and probes the staged version specifically.
- `rollback-fastly` — production: activate the previous version; staging:
  deactivate the staged version.

You wire the trio; the actions carry no orchestration policy (see the spec §5.4
for a worked staging workflow).

## 5. What the actions intentionally do not do

The deploy actions do not perform: internal application checkout; config
expansion or JSON→provider config conversion. Config push and provisioning are
explicit CLI subcommands (`edgezero config push`, `edgezero provision`) a
consumer may run as separate steps, not deploy side effects. The generic engine
stays provider-neutral — staging/health/rollback exist only as Fastly-specific
actions (§4), not engine behavior.

## 6. Worked example — Trusted Server deployer migration

### 6.1 Current behavior

The Trusted Server deployer orchestrates Fastly deploys with:

- `.github/workflows/deploy.yml` (manual) and `daily-deploy.yml` (scheduled);
- `stackpop/trusted-server-actions/fastly/deploy@v2`;
- `stackpop/trusted-server-actions/fastly/healthcheck@v2`; and
- `stackpop/trusted-server-actions/fastly/rollback@v2`.

The old deploy action checks out Trusted Server internally, accepts
`trusted-server-ref`, expands `trusted-server-config`, supports Fastly staging,
and returns `fastly-version`.

### 6.2 Compatibility with the EdgeZero actions

The EdgeZero actions now cover Fastly **staging, health checks, rollback, and the
`fastly-version` output**, so the Trusted Server deployer can move off the legacy
`fastly/deploy|healthcheck|rollback@v2` actions. The remaining differences the
deployer must handle itself are: internal checkout, `trusted-server-ref`,
`trusted-server-config` expansion, and legacy JSON→Config Store TOML conversion.

### 6.3 Recommended migration

Map the legacy trio onto the EdgeZero staging trio:

| Legacy action                              | EdgeZero replacement                               |
| ------------------------------------------ | -------------------------------------------------- |
| `fastly/deploy@v2` (with `fastly-staging`) | `build-app-cli` + `deploy-fastly` (`stage:` input) |
| `fastly/healthcheck@v2`                    | `healthcheck-fastly`                               |
| `fastly/rollback@v2`                       | `rollback-fastly`                                  |

Workflow shape:

1. check out `trusted-server-deployer` with `persist-credentials: false`;
2. check out Trusted Server source separately at the selected ref into
   `trusted-server`;
3. run `build-app-cli` with `cli-package: <trusted-server-cli-crate>` and
   `working-directory: trusted-server` (Trusted Server's own CLI package, whose
   `Cargo.toml` already pins the Fastly adapter);
4. run `deploy-fastly` (set `stage: true` for staging) with the CLI artifact,
   `working-directory: trusted-server`, typed Fastly credentials, and optional
   `deploy-args: ["--comment", …]`; capture `fastly-version`;
5. run `healthcheck-fastly` with the CLI artifact, typed Fastly credentials
   (`fastly-api-token`, `fastly-service-id`), `deploy-to`, `domain`, and the
   captured `fastly-version`;
6. on failure, run `rollback-fastly` with the CLI artifact, typed Fastly
   credentials (`fastly-api-token`, `fastly-service-id`), and the same
   `deploy-to` / `fastly-version`; and
7. write a summary from the action outputs.

### 6.4 Required deployer changes

- Add explicit Trusted Server checkout; the EdgeZero actions do not call
  `actions/checkout`.
- Replace the legacy `fastly/*@v2` trio with `build-app-cli` + `deploy-fastly` +
  `healthcheck-fastly` + `rollback-fastly`.
- Pin action references to readable released tags, or full SHAs for production.
- Read the version from `steps.<deploy>.outputs.fastly-version` (same concept as
  the legacy `fastly-version`).
- Audit `TRUSTED_SERVER_CONFIG`; if still needed, keep config expansion in the
  deployer workflow or run `edgezero config push` as an explicit step before or
  after deploy.
- Confirm the canonical Trusted Server repository/ref has a `Cargo.lock` at the
  CLI package's Cargo workspace root, plus `Cargo.toml`, `fastly.toml`, and
  preferably `edgezero.toml`.

### 6.5 Gotchas

- `daily-deploy.yml` appears to stage but health-check/rollback production by
  default. Decide whether the scheduled workflow is production or staging and set
  `deploy-to` / `stage` consistently before migration.
- The old action targets `IABTechLab/trusted-server`; verify the actual
  deployment refs before switching.
