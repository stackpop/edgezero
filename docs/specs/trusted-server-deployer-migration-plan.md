# Trusted Server Deployer Migration Plan

**Status:** Draft migration plan

**Repository reviewed:** the Trusted Server deployer repository

## Current deployer behavior

The deployer repository currently orchestrates Trusted Server Fastly deployments with:

- `.github/workflows/deploy.yml` for manual deploys;
- `.github/workflows/daily-deploy.yml` for scheduled deploys;
- `stackpop/trusted-server-actions/fastly/deploy@v2`;
- `stackpop/trusted-server-actions/fastly/healthcheck@v2`; and
- `stackpop/trusted-server-actions/fastly/rollback@v2`.

The old deploy action checks out Trusted Server internally, accepts `trusted-server-ref`, expands `trusted-server-config`, supports Fastly staging, and returns `fastly-version`.

## Compatibility gap

The EdgeZero generic deploy action intentionally does not support:

- internal Trusted Server checkout;
- `trusted-server-ref`;
- `trusted-server-config` JSON expansion;
- automatic conversion of legacy JSON config to Fastly Config Store TOML;
- Fastly staging;
- Fastly service-version output;
- health checks; or
- rollback.

Therefore the deployer cannot switch all existing behavior to the generic action in one step.

## Recommended migration

Start with a new production-only workflow and leave existing staging on the old action path until a Fastly-specific staging design exists.

Production workflow shape:

1. checkout `trusted-server-deployer` with `persist-credentials: false`;
2. checkout Trusted Server source separately at the selected ref into `trusted-server`;
3. write pre-deploy summary;
4. capture the currently active Fastly version before deploy;
5. invoke `stackpop/edgezero/.github/actions/deploy@<ref>` with:
   - `adapter: fastly`;
   - `working-directory: trusted-server`;
   - `manifest: edgezero.toml` when present on all deployed refs;
   - `cache: true` if trusted and safe;
   - typed Fastly credentials;
   - optional safe `deploy-args` such as `--comment`;
6. run deployer-owned production health check;
7. on health-check failure, reactivate the captured previous Fastly version;
8. write post-deploy summary from generic action outputs and rollback baseline.

## Required deployer changes

- Add explicit Trusted Server checkout; the generic action will not call `actions/checkout`.
- Pin action references to full SHAs for production examples.
- Use `ubuntu-24.04`, least-privilege `contents: read`, `timeout-minutes`, protected environments, and per-domain concurrency with `cancel-in-progress: false`.
- Update post-deploy summary to stop requiring `steps.deploy.outputs.fastly-version`.
- Move health check and rollback logic into deployer-local scripts/actions or keep old provider-specific actions pinned to full SHAs.
- Audit `TRUSTED_SERVER_CONFIG`; if still needed, keep config expansion and provider mutation in the deployer workflow or a Trusted Server-specific helper before invoking the generic deploy action.
- Confirm the canonical Trusted Server repository/ref has root `Cargo.lock`, `Cargo.toml`, `fastly.toml`, and preferably `edgezero.toml`.

## Gotchas

- The current `daily-deploy.yml` appears to stage but health-check/rollback production by default. Decide whether the scheduled workflow is production or staging and fix the mismatch before migration.
- Generic deploy has no Fastly staging contract. Keep staging legacy or design a separate Fastly staging action.
- Generic deploy has no Fastly version output. Capture the active Fastly version before deploy for production rollback.
- The reviewed local Trusted Server checkout has root `edgezero.toml` and `fastly.toml`, but the existing old action targets `IABTechLab/trusted-server`; verify the actual deployment refs before switching.
