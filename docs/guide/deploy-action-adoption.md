# EdgeZero deployment action adoption

The Fastly lifecycle actions deploy an immutable application release. They do not
build the application or choose its source. This split keeps application bytes
independent from a publisher's credentials and runtime configuration.

## Application release producer

Each application owns a release pipeline that:

1. checks out one approved source revision;
2. builds the application CLI and Fastly package without provider credentials;
3. bundles that CLI, package, `edgezero.toml`, its referenced `fastly.toml`, and
   strict `release.json` metadata;
4. publishes the archive under an immutable reference; and
5. publishes the archive's lowercase SHA-256 digest.

The producer does not select a publisher GitHub Environment. One application
release supplies a byte-identical application CLI, Fastly package,
`edgezero.toml`, and `fastly.toml` to every publisher and to both production and
staging. Different applications or later releases can have different manifests.
A deployer or runtime environment cannot replace them.

## Deployment consumer

The deployment repository resolves a trusted application release, downloads its
archive, verifies the pinned digest, and passes it to the lifecycle actions. The
deployer never checks out or rebuilds application source. It supplies only:

- the destination Fastly service and production/staging target;
- runtime variables and selected physical Config/KV/Secret stores;
- a typed config payload when running config push; and
- provider credentials.

Select the source revision and release digest before choosing the publisher
GitHub Environment. A release reference or digest is application-release data and
cannot come from a publisher GitHub Environment. This prevents a publisher or
environment administrator from selecting different executable code or manifests.

Download the selected release once and reuse it:

```yaml
- uses: actions/download-artifact@v4
  with:
    name: application-release-${{ needs.release.outputs.source-revision }}
    path: app-release

- id: deploy
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

Deploy, config push, healthcheck, and rollback extract the application CLI from
that release. Deploy uploads its recorded Fastly package. Config push uses its
recorded `edgezero.toml`; Fastly deployment verifies the recorded referenced
manifest. There are no deployer build flags or alternate manifest inputs.

## Environment contract

Publisher GitHub Environments can set canonical runtime variables such as:

```text
EDGEZERO__LOGGING__LEVEL
EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME
EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY
EDGEZERO__STORES__KV__CACHE__NAME
EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME
```

They may choose different values and physical resources for each publisher and
for production/staging. The variable names contain no service ID. Secret Store
selectors contain only a store name, never a secret value.

Fastly prepares the version-scoped descriptor and exact resource links before
staging or activation. Failed resolution or verification withholds publication.
See the [Fastly adapter guide](./adapters/fastly.md#runtime-descriptor) for the
descriptor format and clean-cutover behavior.

## Example application deployer

The example application has two hostnames:

- `app.example.com` selects the `production` GitHub Environment;
- `staging.app.example.com` selects the `staging` GitHub Environment.

The hostname remains the real Fastly and healthcheck domain. It is not itself a
GitHub Environment name. Validate and map it in a credential-free preflight job,
then use the preflight output on the deploy job.

```yaml
name: Deploy Application

on:
  workflow_dispatch:
    inputs:
      domain:
        description: Application hostname
        required: true
        type: choice
        options:
          - app.example.com
          - staging.app.example.com
      source-revision:
        description: Approved application revision
        required: true
        type: string

jobs:
  release:
    runs-on: ubuntu-latest
    outputs:
      release-ref: ${{ steps.select.outputs['release-ref'] }}
      sha256: ${{ steps.select.outputs.sha256 }}
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - id: select
        env:
          SOURCE_REVISION: ${{ inputs.source-revision }}
        run: |
          # Resolve SOURCE_REVISION through trusted application-release metadata.
          # Never read the archive reference or digest from a publisher Environment.
          ./scripts/select-application-release "$SOURCE_REVISION"

  preflight:
    needs: release
    runs-on: ubuntu-latest
    outputs:
      environment: ${{ steps.target.outputs.environment }}
      deploy-to: ${{ steps.target.outputs.deploy-to }}
      release-ref: ${{ needs.release.outputs['release-ref'] }}
      sha256: ${{ needs.release.outputs.sha256 }}
    steps:
      - id: target
        env:
          DOMAIN: ${{ inputs.domain }}
        run: |
          case "$DOMAIN" in
            app.example.com) environment=production ; deploy_to=production ;;
            staging.app.example.com) environment=staging ; deploy_to=staging ;;
            *) echo "::error::unsupported application domain"; exit 1 ;;
          esac
          echo "environment=$environment" >>"$GITHUB_OUTPUT"
          echo "deploy-to=$deploy_to" >>"$GITHUB_OUTPUT"

  deploy:
    needs: preflight
    runs-on: ubuntu-latest
    environment: ${{ needs.preflight.outputs.environment }}
    env:
      EDGEZERO__LOGGING__LEVEL: ${{ vars.EDGEZERO__LOGGING__LEVEL }}
      EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME: ${{ vars.EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME }}
      EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY: ${{ vars.EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY }}
      EDGEZERO__STORES__KV__CACHE__NAME: ${{ vars.EDGEZERO__STORES__KV__CACHE__NAME }}
      EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME: ${{ vars.EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME }}
    concurrency:
      group: application-${{ vars.FASTLY_SERVICE_ID }}
      cancel-in-progress: false
    permissions:
      contents: read
      actions: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - name: Download the selected application release
        env:
          RELEASE_REF: ${{ needs.preflight.outputs['release-ref'] }}
        run: |
          ./scripts/download-application-release \
            "$RELEASE_REF" \
            "$GITHUB_WORKSPACE/app-release.tar.gz"

      - id: config
        uses: stackpop/edgezero/.github/actions/config-push-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          working-directory: publisher-config
          app-config: app.toml
          store: app_config
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - id: deploy
        uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          deploy-args: '["--comment","Application release"]'
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - id: health
        uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          fastly-api-token: ${{ needs.preflight.outputs.deploy-to == 'staging' && secrets.FASTLY_API_TOKEN || '' }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          fastly-version: ${{ steps.deploy.outputs.fastly-version }}
          domain: ${{ inputs.domain }}
          path: /health
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - name: Roll back failed deployment
        if: ${{ (failure() || cancelled()) && steps.deploy.outputs.fastly-version != '' && (needs.preflight.outputs.deploy-to == 'staging' || steps.deploy.outputs.previous-version != '') }}
        uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          fastly-version: ${{ steps.deploy.outputs.fastly-version }}
          rollback-to: ${{ steps.deploy.outputs.previous-version }}
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}
```

The example keeps application release selection outside both publisher
Environments. `preflight` waits for that selection and forwards the immutable
reference and digest with its validated environment name. The deploy job then
downloads the release once; each lifecycle action independently verifies the
same archive and digest. It receives environment-scoped runtime values only
after release selection. The optional `CREDENTIALS` selector is a Secret Store
name from `vars`, never a secret value. `inputs.domain` remains the actual
hostname passed to healthcheck. The deployer checkout is only the deployment
repository; it never checks out or rebuilds application source.

For a staging rollback, omit `rollback-to`; for production, skip rollback when
`previous-version` is empty because a first deployment has no earlier version.
Use an additional guard in a production-only workflow if it can perform a first
deployment.

## Failed deploy recovery

The action's public `package-digest` becomes available only after the deploy step
emits adapter `package-sha256`; it may be absent when preflight fails before the
application CLI runs. The action emits a recoverable `fastly-version` immediately
after Fastly creates or selects an inactive draft. Those outputs can remain
available if descriptor, link, or publication work fails later. In a follow-up
step guarded with GitHub Actions' `always()` condition, inspect the failed step's
outputs:

- if `fastly-version` is present, inspect and reuse that exact inactive draft;
- deactivate it only when it is staged;
- if production activated it and `previous-version` is present, roll back to the
  recorded previous version;
- if `mutation-attempted` is true but no version is available, reconcile provider
  state manually rather than guessing.

Serialize deployments by service. Without serialization, a provider lookup after
failure may observe another run's version.

## Adoption checklist

- Produce and publish one strict application release without provider credentials.
- Resolve its trusted source revision and SHA-256 outside publisher Environments.
- Download and verify the archive; do not checkout application source in deploy.
- Pass the same archive and digest to deploy, config push, healthcheck, and
  rollback.
- Keep runtime variables and credentials in protected publisher Environments.
- Validate that `FASTLY_SERVICE_ID` contains ASCII letters and digits only.
- Thread `deploy-to` consistently through deploy, healthcheck, and rollback.
- Record `package-digest`, `fastly-version`, and `previous-version` for recovery.
- Pin action references and serialize deployments per service.
