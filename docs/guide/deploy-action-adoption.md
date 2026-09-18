# EdgeZero deployment action adoption

The Fastly lifecycle actions deploy an immutable application release. They do not
build the application or choose its source. This split keeps application bytes
independent from a publisher's credentials and runtime configuration.

## Application release producer

Each application owns a release pipeline that:

1. checks out one approved source revision;
2. builds the application CLI and Fastly package without provider credentials;
3. bundles that CLI, package, `edgezero.toml`, every adapter manifest it
   references, and strict `release.json` metadata;
4. publishes the archive under an immutable reference; and
5. publishes the archive's lowercase SHA-256 digest.

The producer does not select a publisher GitHub Environment. One application
release supplies a byte-identical application CLI, Fastly package,
`edgezero.toml`, and complete referenced adapter-manifest set to every publisher
and to both production and staging. Different applications or later releases can
have different manifests. A deployer or runtime environment cannot replace them.

Use the release packager after the application CLI and Fastly package have been
built in the credential-free producer job:

```yaml
- id: release
  uses: stackpop/edgezero/.github/actions/package-fastly-application-release@<ref>
  with:
    app-cli-archive: release-inputs/app-cli.tar
    fastly-package: pkg/app.tar.gz
    application-manifest: edgezero.toml
    adapter-manifest: adapters/fastly/fastly.toml
    source-revision: ${{ github.sha }}
    artifact-name: application-release-${{ github.sha }}
```

The action verifies the application CLI's lifecycle command surface, writes
strict metadata with `format: 1` and `lifecycle_protocol: 1`, verifies the
assembled archive through the same consumer validator, uploads
`app-release.tar.gz`, and returns its SHA-256. It accepts only prebuilt inputs
beneath `github.workspace` and receives no provider credentials. The explicit
`adapter-manifest` input identifies the Fastly manifest; the packager follows
`edgezero.toml` and includes every other referenced adapter manifest at its
declared relative path.

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
    run-id: ${{ needs.release.outputs.run-id }}
    repository: ${{ needs.release.outputs.producer-repository }}
    github-token: ${{ secrets.APPLICATION_RELEASE_TOKEN }}

- id: deploy
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
```

`producer-repository` is the application repository in `owner/name` form. For a
cross-repository release, `APPLICATION_RELEASE_TOKEN` must be a fine-grained
personal access token or GitHub App token with Actions read access to that
repository. The built-in workflow token is sufficient only when producer and
deployer are the same repository.

Deploy, config push, healthcheck, and rollback extract the application CLI from
that release. Deploy uploads its recorded Fastly package. Config push uses its
recorded `edgezero.toml`; Fastly deployment verifies the recorded referenced
manifest. There are no deployer build flags or alternate manifest inputs.

## Environment contract

Publisher GitHub Environments can set canonical runtime variables such as:

```text
EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME
EDGEZERO__STORES__KV__CACHE__NAME
EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME
```

They may choose different values and physical resources for each publisher and
for production/staging. The variable names contain no service ID. Secret Store
selectors contain only a store name, never a secret value. Fastly logging comes
from the immutable application manifest. Every target uses `<logical-id>` as
the config key. The selected Environment chooses the physical
store with `__NAME`; using the same value shares config and using different
values isolates it.

Fastly resolves each selected physical store and attaches it to the target
version under the manifest's logical ID before staging or activation. Failed
resolution or verification withholds publication. See the
[Fastly adapter guide](./adapters/fastly.md#store-selection-and-deployment).

## Example application deployer

The example application has one real hostname, `app.example.com`, and two
publication targets. Production selects the `app.example.com` GitHub
Environment; staging selects `staging.app.example.com`. Both targets still pass
`app.example.com` to Fastly health checks. Validate the domain and target in a
provider-credential-free preflight job, derive the Environment identifier, and
query GitHub before the deploy job references it. This prevents GitHub from
silently creating a missing Environment without its expected protection rules
or secrets. Private repositories use the built-in token with `actions: read`;
the preflight receives no provider secret.

```yaml
name: Deploy Application

on:
  workflow_dispatch:
    inputs:
      domain:
        description: Application hostname
        required: true
        type: string
        default: app.example.com
      deploy-to:
        description: Publication target
        required: true
        type: choice
        options:
          - staging
          - production
      source-revision:
        description: Approved application revision
        required: true
        type: string
      producer-repository:
        description: Repository that produced the application release (owner/name)
        required: true
        type: string
      release-run-id:
        description: Trusted producer workflow run containing the immutable release
        required: true
        type: string
      release-sha256:
        description: Lowercase SHA-256 recorded by the trusted producer
        required: true
        type: string

jobs:
  preflight:
    runs-on: ubuntu-latest
    permissions:
      actions: read
    outputs:
      environment: ${{ steps.environment.outputs.environment-name }}
      concurrency-key: ${{ steps.target.outputs.concurrency-key }}
      deploy-to: ${{ steps.target.outputs.deploy-to }}
      producer-repository: ${{ steps.release.outputs.producer-repository }}
      release-artifact: ${{ steps.release.outputs.artifact }}
      release-run-id: ${{ steps.release.outputs.run-id }}
      sha256: ${{ steps.release.outputs.sha256 }}
      source-revision: ${{ steps.release.outputs.source-revision }}
    steps:
      - id: release
        env:
          SOURCE_REVISION: ${{ inputs.source-revision }}
          PRODUCER_REPOSITORY: ${{ inputs.producer-repository }}
          RELEASE_RUN_ID: ${{ inputs.release-run-id }}
          RELEASE_SHA256: ${{ inputs.release-sha256 }}
        run: |
          {
            [[ "$SOURCE_REVISION" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || {
              echo "::error::source-revision must be a full lowercase Git SHA"; exit 1;
            }
            [[ "$RELEASE_RUN_ID" =~ ^[1-9][0-9]*$ ]] || {
              echo "::error::release-run-id must be a positive integer"; exit 1;
            }
            [[ "$RELEASE_SHA256" =~ ^[0-9a-f]{64}$ ]] || {
              echo "::error::release-sha256 must be 64 lowercase hexadecimal characters"; exit 1;
            }
            [[ "$PRODUCER_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
              echo "::error::producer-repository must use owner/name syntax"; exit 1;
            }
            echo "artifact=application-release-$SOURCE_REVISION"
            echo "producer-repository=$PRODUCER_REPOSITORY"
            echo "run-id=$RELEASE_RUN_ID"
            echo "sha256=$RELEASE_SHA256"
            echo "source-revision=$SOURCE_REVISION"
          } >>"$GITHUB_OUTPUT"

      - id: target
        env:
          DOMAIN: ${{ inputs.domain }}
          DEPLOY_TO: ${{ inputs.deploy-to }}
        run: |
          case "$DOMAIN" in
            app.example.com) concurrency_key=app-example-com ;;
            *) echo "::error::unsupported application domain"; exit 1 ;;
          esac
          case "$DEPLOY_TO" in
            production) environment="$DOMAIN" ;;
            staging) environment="staging.$DOMAIN" ;;
            *) echo "::error::unsupported publication target"; exit 1 ;;
          esac
          {
            echo "environment=$environment"
            echo "concurrency-key=$concurrency_key"
            echo "deploy-to=$DEPLOY_TO"
          } >>"$GITHUB_OUTPUT"

      - id: environment
        uses: stackpop/edgezero/.github/actions/require-github-environment@<ref>
        with:
          environment-name: ${{ steps.target.outputs.environment }}
          repository: ${{ github.repository }}
          github-token: ${{ github.token }}

  deploy:
    needs: preflight
    runs-on: ubuntu-latest
    environment: ${{ needs.preflight.outputs.environment }}
    env:
      EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME: ${{ vars.EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME }}
      EDGEZERO__STORES__KV__CACHE__NAME: ${{ vars.EDGEZERO__STORES__KV__CACHE__NAME }}
      EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME: ${{ vars.EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME }}
    concurrency:
      group: application-${{ needs.preflight.outputs.concurrency-key }}
      cancel-in-progress: false
    permissions:
      contents: read
      actions: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - name: Download the selected application release
        uses: actions/download-artifact@v4
        with:
          name: ${{ needs.preflight.outputs.release-artifact }}
          path: app-release
          run-id: ${{ needs.preflight.outputs.release-run-id }}
          repository: ${{ needs.preflight.outputs.producer-repository }}
          github-token: ${{ secrets.APPLICATION_RELEASE_TOKEN }}

      - id: config
        uses: stackpop/edgezero/.github/actions/config-push-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          expected-source-revision: ${{ needs.preflight.outputs.source-revision }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          working-directory: publisher-config
          app-config: app.toml
          store: app_config
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - name: Record required config reconciliation
        if: ${{ always() && steps.config.outcome == 'failure' && steps.config.outputs.mutation-attempted == 'true' }}
        env:
          RELEASE_SHA256: ${{ needs.preflight.outputs.sha256 }}
          DEPLOY_TO: ${{ needs.preflight.outputs.deploy-to }}
        run: |
          echo "::error::Config mutation was attempted. Re-run config push with release $RELEASE_SHA256 and target $DEPLOY_TO, or verify the selected store and logical key before deploying."
          exit 1

      - id: deploy
        uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          expected-source-revision: ${{ needs.preflight.outputs.source-revision }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          deploy-args: '["--comment","Application release"]'
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - id: health
        uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          expected-source-revision: ${{ needs.preflight.outputs.source-revision }}
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
          app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
          app-release-sha256: ${{ needs.preflight.outputs.sha256 }}
          expected-source-revision: ${{ needs.preflight.outputs.source-revision }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          fastly-version: ${{ steps.deploy.outputs.fastly-version }}
          rollback-to: ${{ steps.deploy.outputs.previous-version }}
          deploy-to: ${{ needs.preflight.outputs.deploy-to }}

      - name: Require config reconciliation after a later failure
        if: ${{ always() && steps.config.outcome == 'success' && (steps.deploy.outcome == 'failure' || steps.health.outcome == 'failure' || cancelled()) }}
        run: |
          echo "::error::Config push succeeded but deployment did not. Restore the previous config value or re-push the previous release's config to the same physical store and logical key before retrying."
          exit 1
```

The example pins the producer run, source revision, and archive digest outside
both publisher Environments. `preflight` validates that identity and proves the
derived GitHub Environment already exists. The deploy job downloads the exact
artifact from that producer repository and run once; each lifecycle action
independently verifies the same archive, digest, and source revision. It receives
environment-scoped runtime values only afterward. The optional `CREDENTIALS` selector is a Secret Store
name from `vars`, never a secret value. `inputs.domain` remains the actual
hostname passed to healthcheck for both targets. The deployer checkout contains
only publisher config; it never checks out or rebuilds application source.

For a staging rollback, omit `rollback-to`; for production, skip rollback when
`previous-version` is empty because a first deployment has no earlier version.
The staging rollback action inspects the exact version and deactivates it only
when staged; it succeeds without mutation when deployment failed while the
version was still an unpublished draft. Use an additional guard in a
production-only workflow if it can perform a first deployment.

## Failed deploy recovery

The action's public `package-digest` becomes available only after the deploy step
emits adapter `package-sha256`; it may be absent when preflight fails before the
application CLI runs. The action emits `fastly-version` as soon as the target
version is known, so it can remain available if link reconciliation or publication
work fails later. The output identifies the exact provider version but does not
prove its current Fastly state.

In a follow-up step guarded with GitHub Actions' `always()` condition, inspect the
failed step's outputs. The rollback action handles the exact staging version
state and refuses incompatible state. If a production version is active and
`previous-version` is present, run production rollback. If its state is unknown,
reconcile provider state manually without guessing. If `mutation-attempted` is
true for config push, re-run the same pinned release, logical store, payload, and
target or verify that exact physical store and logical key before deployment.
Config pushed before deployment must remain backward-compatible with the current
published release until deployment and healthcheck finish. If a later step
fails, restore the prior value or re-push the prior release's config to the same
physical store and logical key; version the physical store when that compatibility
cannot be guaranteed.

Serialize deployments by service. Without serialization, a provider lookup after
failure may observe another run's version.

## Runner requirements

The actions are tested on `ubuntu-latest`. A self-hosted runner must be Linux
x86-64 and provide Bash, `jq`, Mike Farah `yq` v4, `tar`, `curl`, `git`, `base64`,
`realpath`, and either `sha256sum` or `shasum`. The application CLI archive must
contain a Linux x86-64 executable.

## Adoption checklist

- Produce and publish one strict application release without provider credentials.
- Resolve its trusted source revision and SHA-256 outside publisher Environments.
- Download from the explicit producer repository and verify the archive, digest,
  and selected source revision; do not checkout application source in deploy.
- Pass the same archive, digest, and source revision to deploy, config push, healthcheck, and
  rollback.
- Keep runtime variables and credentials in protected publisher Environments.
- Validate that `FASTLY_SERVICE_ID` contains ASCII letters and digits only.
- Thread `deploy-to` consistently through deploy, healthcheck, and rollback.
- Record `package-digest`, `fastly-version`, and `previous-version` for recovery.
- Pin action references and serialize deployments per service.
