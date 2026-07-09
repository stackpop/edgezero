# EdgeZero Deploy Actions — Adoption Guide

**Status:** Adoption guide for any EdgeZero application repository

**Spec:** `docs/specs/edgezero-deploy-github-action.md`

The layered deploy actions are for **any** EdgeZero application repository, not a
single deployer. This guide describes the general adoption shape and then walks
through the Trusted Server deployer as one concrete migration example.

## 1. What a consumer gets

Three composable actions:

- `stackpop/edgezero/.github/actions/build-cli@<ref>` — compile the CLI package
  the application provides (a crate in the app's own workspace) once, publish it
  as an artifact;
- `stackpop/edgezero/.github/actions/deploy-fastly@<ref>` — deploy a checked-out
  Fastly application using the prebuilt CLI artifact;
- future `deploy-cloudflare` / `deploy-spin` wrappers over the same engine.

The actions own repeatable deploy setup. The consumer owns checkout, ref
selection, permissions, environments, concurrency, timeouts, and any health
check or rollback.

## 2. Checkout layouts

The adapters your CLI supports come from your app's own `Cargo.toml`, so
`build-cli` takes no `adapters` input — it builds your CLI package as declared.

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
        uses: stackpop/edgezero/.github/actions/build-cli@<ref>
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
      ref: ${{ inputs.ref }}
      path: app
      persist-credentials: false
      # A private app repo is NOT readable with the deployer's default
      # GITHUB_TOKEN. Supply a token scoped to the app repo — a GitHub App
      # installation token (preferred) or a fine-grained PAT with
      # `contents: read` on the app repo:
      token: ${{ steps.app-token.outputs.token }}

  - id: cli
    uses: stackpop/edgezero/.github/actions/build-cli@<ref>
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
    uses: stackpop/edgezero/.github/actions/build-cli@<ref>
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
  `build-cli` compiles that package from your checkout, so the CLI and your app
  never disagree on schema. `build-cli` does not use the EdgeZero monorepo CLI.
- Provide typed provider credentials through wrapper inputs, not caller `env:`.
- Ensure the deployed ref has committed source (no dirty working tree) and, for
  caching, a root `Cargo.lock`.
- Pin action references to readable released tags, or full SHAs for production
  reproducibility.
- Use least-privilege permissions (`contents: read`), protected environments,
  `timeout-minutes`, and appropriate concurrency.

## 4. What the actions intentionally do not do

The deploy actions do not perform: internal application checkout; config
expansion or JSON→provider config conversion; provider staging; provider
service-version output; health checks; or rollback. Config push and provisioning
are explicit CLI subcommands (`edgezero config push`, `edgezero provision`) a
consumer may run as separate steps, not deploy side effects.

Consumers needing those behaviors keep them in their own workflow steps or in
provider-specific actions pinned separately.

## 5. Worked example — Trusted Server deployer migration

### 5.1 Current behavior

The Trusted Server deployer orchestrates Fastly deploys with:

- `.github/workflows/deploy.yml` (manual) and `daily-deploy.yml` (scheduled);
- `stackpop/trusted-server-actions/fastly/deploy@v2`;
- `stackpop/trusted-server-actions/fastly/healthcheck@v2`; and
- `stackpop/trusted-server-actions/fastly/rollback@v2`.

The old deploy action checks out Trusted Server internally, accepts
`trusted-server-ref`, expands `trusted-server-config`, supports Fastly staging,
and returns `fastly-version`.

### 5.2 Compatibility gaps vs the generic actions

The generic actions do not provide: internal checkout, `trusted-server-ref`,
`trusted-server-config` expansion, legacy JSON→Config Store TOML conversion,
Fastly staging, service-version output, health checks, or rollback. So the
deployer cannot switch all behavior in one step.

### 5.3 Recommended migration

Start with a production-only workflow and leave existing staging on the old
action path until a Fastly-specific staging design exists.

Production workflow shape:

1. check out `trusted-server-deployer` with `persist-credentials: false`;
2. check out Trusted Server source separately at the selected ref into
   `trusted-server`;
3. write a pre-deploy summary;
4. capture the currently active Fastly version before deploy (for rollback
   baseline);
5. run `build-cli` with `cli-package: <trusted-server-cli-crate>` and
   `working-directory: trusted-server` (compiling Trusted Server's own CLI
   package, whose `Cargo.toml` already pins the Fastly adapter);
6. run `deploy-fastly` with:
   - `cli-artifact` from the `build-cli` output;
   - `working-directory: trusted-server`;
   - `manifest: edgezero.toml` when present on all deployed refs;
   - `cache: true` if trusted and safe;
   - typed Fastly credentials; and
   - optional safe `deploy-args` such as `--comment`;
7. run the deployer-owned production health check;
8. on health-check failure, reactivate the captured previous Fastly version; and
9. write a post-deploy summary from action outputs and the rollback baseline.

### 5.4 Required deployer changes

- Add explicit Trusted Server checkout; the generic actions do not call
  `actions/checkout`.
- Split CLI build (`build-cli`) from deploy (`deploy-fastly`) and pass the CLI
  artifact between them.
- Pin action references to readable released tags, or full SHAs for production.
- Update the post-deploy summary to stop requiring
  `steps.deploy.outputs.fastly-version`.
- Move health check and rollback into deployer-local scripts/actions, or keep the
  old provider-specific actions pinned.
- Audit `TRUSTED_SERVER_CONFIG`; if still needed, keep config expansion in the
  deployer workflow or run `edgezero config push` as an explicit step before or
  after deploy.
- Confirm the canonical Trusted Server repository/ref has root `Cargo.lock`,
  `Cargo.toml`, `fastly.toml`, and preferably `edgezero.toml`.

### 5.5 Gotchas

- `daily-deploy.yml` appears to stage but health-check/rollback production by
  default. Decide whether the scheduled workflow is production or staging and fix
  the mismatch before migration.
- The generic actions have no Fastly staging contract. Keep staging legacy or
  design a separate Fastly staging action.
- The generic actions expose no Fastly version. Capture the active version before
  deploy for production rollback.
- The old action targets `IABTechLab/trusted-server`; verify the actual
  deployment refs before switching.
