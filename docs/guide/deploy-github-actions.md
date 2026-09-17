# Deploying with GitHub Actions

EdgeZero's Fastly lifecycle actions consume a verified, immutable application
release. The application pipeline builds that release once. A deployment job
selects the archive and its SHA-256 digest, downloads it, and then supplies only
runtime configuration, a destination service, a publication target, and provider
credentials.

The release archive contains all application-owned deploy inputs:

- the application CLI used by deploy, config push, healthcheck, and rollback;
- the prebuilt Fastly package;
- the application's exact `edgezero.toml`; and
- the exact Fastly manifest referenced by that application manifest.

For one release, those four members are byte-identical across every publisher,
deployer, production deployment, and staging deployment. A later application
release may change them. Runtime variables, selected physical Config/KV/Secret
stores, destination service, credentials, and publication target may vary without
changing the release.

## Producer and deployer boundary

The application release pipeline is the sole producer. It checks out an approved
source revision, builds the application CLI and Fastly package without a GitHub
Environment or provider credential, records their digests and both manifests in
`release.json`, archives the result, and publishes the archive plus its SHA-256.

The deployer is a consumer. It resolves the application source revision and
release digest before selecting a publisher GitHub Environment. It downloads the
existing archive, verifies the pinned digest, and passes the same archive to every
lifecycle action. It does not check out or rebuild application source. A publisher
GitHub Environment supplies runtime values and credentials; it must never supply
the release reference or release digest.

A typical application release layout is:

```text
release.json
cli/app-cli.tar
package/app.tar.gz
edgezero.toml
adapter/fastly.toml
```

The four member paths are release metadata, so another release may arrange them
differently. `release.json` records `format: 1`, the 40- or 64-character lowercase
hexadecimal source revision, adapter `fastly`, and the relative path and SHA-256
for each file. Verification rejects unknown or duplicate fields, unsafe paths,
symlinks, extra files or directories, digest mismatches, and manifests that do
not match the application's recorded manifest relationship. The deployer cannot
substitute a package or manifest after verification.

## Production deployment

This job assumes an earlier release-selection job returned an artifact name and
digest from trusted application-release data:

```yaml
jobs:
  deploy:
    needs: release
    runs-on: ubuntu-latest
    environment: production
    permissions:
      contents: read
      actions: read
    steps:
      - uses: actions/checkout@v4
        with:
          persist-credentials: false

      - uses: actions/download-artifact@v4
        with:
          name: ${{ needs.release.outputs.artifact }}
          path: app-release

      - id: deploy
        uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
        with:
          app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
          app-release-sha256: ${{ needs.release.outputs.sha256 }}
          fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
          fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
          deploy-args: '["--comment","production release"]'
          deploy-to: production
```

`FASTLY_SERVICE_ID` must contain ASCII letters and digits only. The shared
validation used by deploy, healthcheck, and rollback rejects whitespace,
punctuation, shell syntax, and empty IDs before provider mutation.

The deployment output includes:

- `fastly-version`: the prepared version;
- `previous-version`: the production version active before this run, if any;
- `package-digest`: the verified SHA-256 of the Fastly package in the release;
- `source-revision` and `app-cli-version` from the verified release;
- `provider-cli-version`; and
- `mutation-attempted`.

The action emits `fastly-version` as soon as the target version is known. In
particular, it emits `fastly-version` before later preparation failures. Its
public `package-digest` becomes available only after the deploy step emits
adapter `package-sha256`; it may be absent when
preflight fails before the application CLI runs. The version output identifies
the exact provider version but does not prove its current Fastly state. Read
outputs from a failed step in a follow-up step guarded with GitHub Actions'
`always()` condition.

## Runtime configuration and stores

Fastly Compute has no process environment. EdgeZero keeps one physical
`edgezero_runtime_env` Config Store and links it under that reserved alias. Each
service version reads one immutable descriptor at:

```text
EDGEZERO__SERVICES__<SERVICE_ID>__VERSIONS__<VERSION>__ENV_V1
```

The descriptor contains the fixed runtime allowlist and only selectors for stores
declared by the bundled `edgezero.toml`. Canonical environment variables never
contain the service ID:

```text
EDGEZERO__LOGGING__LEVEL
EDGEZERO__STORES__CONFIG__<ID>__NAME
EDGEZERO__STORES__CONFIG__<ID>__KEY
EDGEZERO__STORES__KV__<ID>__NAME
EDGEZERO__STORES__SECRETS__<ID>__NAME
```

Secret Store selectors name a physical store; they never contain secret values.
Secrets remain optional, and deployments without a `[stores.secrets]` declaration
do not select or link a Secret Store.

For a managed deployment, EdgeZero verifies the release and resolves the complete
Config, KV, and Secret inventories before mutation. It uploads the verified
package to an unreachable draft, reconciles the descriptor and exact resource
links, reads them back, revalidates the source and target, and prepares the
descriptor and exact resource links before staging or activation. A lookup,
collision, malformed inventory, changed source, descriptor mismatch, or readback
failure stops publication.

When the active source has no version descriptor, clean cutover classifies its
inherited links by the complete physical resource inventories. It removes every
inherited Config/KV/Secret link that is not desired and preserves links whose
resource IDs are outside those inventories. It does not inspect any legacy
selector entry.

PR #344 service-scoped and unscoped selector entries and old physical staging
twin stores are unsupported. Current deploys never read or write them. They are
inert after cutover and operators may remove them separately after confirming no
older application still depends on them.

## Staging, healthcheck, and rollback

Use the same archive and digest for staging. The following steps run in the same
job as the earlier `actions/download-artifact` step, so they use its runner-local
archive:

```yaml
- id: stage
  uses: stackpop/edgezero/.github/actions/deploy-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    deploy-args: '["--comment","staging release"]'
    deploy-to: staging

- id: probe
  uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    domain: staging.example.com
    path: /health
    deploy-to: staging

- name: Undo a failed staged release
  if: ${{ (failure() || cancelled()) && steps.stage.outputs.fastly-version != '' }}
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    fastly-version: ${{ steps.stage.outputs.fastly-version }}
    deploy-to: staging
```

Staging and production can select different physical stores and runtime values,
but both use the exact release package and manifests. Staging publication uses a
version-scoped descriptor in the same physical runtime store; it does not create
another runtime Config Store.

For production rollback, capture `previous-version` from deploy and pass it as
`rollback-to`:

```yaml
- name: Roll back production
  if: ${{ (failure() || cancelled()) && steps.deploy.outputs.fastly-version != '' && steps.deploy.outputs.previous-version != '' }}
  uses: stackpop/edgezero/.github/actions/rollback-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    fastly-version: ${{ steps.deploy.outputs.fastly-version }}
    rollback-to: ${{ steps.deploy.outputs.previous-version }}
    deploy-to: production
```

If a deploy fails after emitting `fastly-version`, the version's current state may
have changed. Inspect the exact version state first: reuse it only if it is
inactive; deactivate it only if it is staged; if it is active and
`previous-version` is present, run production rollback. If its state is unknown,
reconcile provider state manually without guessing. If no version was emitted but
`mutation-attempted` is true, reconcile provider state manually as well. A first
deployment has no previous production version.

## Config push

Config push uses the application CLI and `edgezero.toml` from the same verified
release. The deployer cannot choose another manifest. Supply exactly one typed
config source:

```yaml
- uses: stackpop/edgezero/.github/actions/config-push-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    working-directory: publisher-config
    app-config: app.toml
    store: app_config
    deploy-to: production
```

`app-config` and `app-config-inline` are mutually exclusive. With
`deploy-to: staging`, the selected GitHub Environment's canonical
`EDGEZERO__STORES__CONFIG__<ID>__KEY` value wins; when absent, the key falls back
to `<logical-id>_staging`. An explicit action `key` input remains production-only.
Config push changes typed runtime data; it does not change the application
release, package, or manifests.

## Action reference

### `deploy-fastly`

| Input                 | Required | Default      | Meaning                                      |
| --------------------- | -------- | ------------ | -------------------------------------------- |
| `app-release-archive` | Yes      | —            | Local path to the immutable release archive. |
| `app-release-sha256`  | Yes      | —            | Expected lowercase SHA-256 of the archive.   |
| `fastly-api-token`    | Yes      | —            | Token exposed only to provider operations.   |
| `fastly-service-id`   | Yes      | —            | Alphanumeric destination service ID.         |
| `deploy-args`         | No       | `[]`         | At most one `--comment` in the JSON array.   |
| `deploy-to`           | No       | `production` | `production` activates; `staging` stages.    |

### `config-push-fastly`

| Input                 | Required | Default      | Meaning                                                                      |
| --------------------- | -------- | ------------ | ---------------------------------------------------------------------------- |
| `app-release-archive` | Yes      | —            | The same pinned release archive.                                             |
| `app-release-sha256`  | Yes      | —            | Expected archive SHA-256.                                                    |
| `fastly-api-token`    | Yes      | —            | Token for the Config Store write.                                            |
| `working-directory`   | No       | `.`          | Publisher-owned typed-config directory.                                      |
| `app-config`          | No\*     | empty        | Typed config file under `working-directory`.                                 |
| `app-config-inline`   | No\*     | empty        | Inline typed config.                                                         |
| `no-env`              | No       | `false`      | Skip the typed runtime environment overlay.                                  |
| `store`               | No       | manifest     | Logical Config Store ID.                                                     |
| `key`                 | No       | empty        | Explicit production key; otherwise use the canonical key or target fallback. |
| `deploy-to`           | No       | `production` | Select the canonical environment key, then the production/staging fallback.  |

\* Exactly one typed config input is required.

### `healthcheck-fastly`

`healthcheck-fastly` requires the release archive and digest, service ID, version,
and real deployment `domain`. `path`, retry count, retry delay, timeout, and
`deploy-to` are optional. Staging also requires the Fastly token to resolve the
version's staging IP; a production probe does not receive the token.

### `rollback-fastly`

`rollback-fastly` requires the release archive and digest, Fastly token, service
ID, failed version, and target. Production additionally requires `rollback-to`;
staging deactivates the supplied staged version.

## Managed deploy arguments

The action accepts JSON `deploy-args`, but Fastly's adapter-managed lifecycle
owns targeting, cloning, package selection, credentials, and publication. The
action wrapper accepts only `--comment` for a version comment. A direct managed
CLI invocation also accepts the non-targeting global booleans listed in the CLI
reference. Unsupported, duplicated, positional, or malformed arguments fail
before provider mutation.

## Security and concurrency

- Pin the release archive by digest and pin action references to a released tag or
  full commit SHA.
- Select release identity outside the publisher GitHub Environment.
- Give the application release producer no provider credential.
- Scope Fastly credentials to lifecycle steps and use protected GitHub
  Environments for runtime configuration.
- Serialize deployments per Fastly service. Recovery assumes another run cannot
  publish a different version between capture and rollback.
- Use ephemeral runners. Cleanup is best effort after cancellation or process
  termination.
