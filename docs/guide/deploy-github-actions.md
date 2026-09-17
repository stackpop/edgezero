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

The application package contains only portable logical store IDs from
`edgezero.toml`. At deployment, these variables select the physical Fastly
resources for that target:

```text
EDGEZERO__STORES__CONFIG__<ID>__NAME
EDGEZERO__STORES__KV__<ID>__NAME
EDGEZERO__STORES__SECRETS__<ID>__NAME
```

If a `__NAME` variable is absent, the physical name defaults to the logical ID.
A present blank or invalid selector fails before provider mutation. Secret Store
selectors contain store names, never secret values. Omitting `[stores.secrets]`
creates no Secret Store link.

For each declared store, managed deploy links the selected physical resource to
the unpublished Fastly version under the stable logical ID. The runtime opens
that logical alias, so production and staging may select the same or different
physical stores without changing package bytes. Config data uses the logical ID
as the production key and `<ID>_staging` as the staging key. Conflicting
`EDGEZERO__STORES__CONFIG__<ID>__KEY` values fail; omit them for Fastly.
Fastly logging settings come from `[adapters.fastly.logging]` in the application
manifest and are baked into the package.

Managed deploy verifies the immutable release, resolves complete Config, KV, and
Secret inventories, prepares an unreachable draft, and reconciles declared
`(resource kind, logical ID)` links. A declared link pointing at a different
physical resource is replaced. Undeclared inherited links are preserved. After
all mutations, EdgeZero re-reads the exact links, source and draft state, and
provider-visible package identity immediately before staging or activation.
Any lookup, malformed inventory, changed source, package mismatch, or readback
failure stops publication.

The PR #344 runtime descriptor and service-scoped selector keys are unsupported.
Deploy removes only the exact inherited legacy Config link whose alias and
physical store are both `edgezero_runtime_env`; it leaves the account-wide store
for older versions and rollback. Operators may remove that store after no older
version depends on it.

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
    domain: app.example.com
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

Staging and production can select different physical stores, while both use the
exact release package and manifests. Fastly version resource links bind each
target to its selected stores. The `domain` remains the application's real Fastly
hostname for both targets; a workflow may use a separate value such as
`staging.app.example.com` only as its GitHub Environment identifier.

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

If a staging deploy fails after emitting `fastly-version`, the rollback action
reads that exact version first. It deactivates a staged version, succeeds without
mutation for an unpublished editable draft, and refuses active, missing,
ambiguous, or incompatible state. Production rollback still requires
`previous-version`. If no version was emitted but `mutation-attempted` is true,
reconcile provider state manually. A first deployment has no previous production
version.

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
`deploy-to: staging`, Fastly writes `<logical-id>_staging`; production writes
`<logical-id>`. A conflicting canonical `__KEY` fails before mutation. The
deprecated action `key` input is retained only to fail with migration guidance
when nonempty. Config push changes typed runtime data; it does not change the
application release, package, or manifests.

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

| Input                 | Required | Default      | Meaning                                               |
| --------------------- | -------- | ------------ | ----------------------------------------------------- |
| `app-release-archive` | Yes      | —            | The same pinned release archive.                      |
| `app-release-sha256`  | Yes      | —            | Expected archive SHA-256.                             |
| `fastly-api-token`    | Yes      | —            | Token for the Config Store write.                     |
| `working-directory`   | No       | `.`          | Publisher-owned typed-config directory.               |
| `app-config`          | No\*     | empty        | Typed config file under `working-directory`.          |
| `app-config-inline`   | No\*     | empty        | Inline typed config.                                  |
| `no-env`              | No       | `false`      | Skip the typed runtime environment overlay.           |
| `store`               | No       | manifest     | Logical Config Store ID.                              |
| `key`                 | No       | empty        | Deprecated; any nonempty value fails before mutation. |
| `deploy-to`           | No       | `production` | Select `<logical-id>` or `<logical-id>_staging`.      |

\* Exactly one typed config input is required.

### `healthcheck-fastly`

`healthcheck-fastly` requires the release archive and digest, service ID, version,
and real deployment `domain`. `path`, retry count, retry delay, timeout, and
`deploy-to` are optional. Staging also requires the Fastly token to resolve the
version's staging IP; a production probe does not receive the token.

### `rollback-fastly`

`rollback-fastly` requires the release archive and digest, Fastly token, service
ID, failed version, and target. Production additionally requires `rollback-to`;
staging inspects the supplied version, deactivates it only when staged, and
no-ops when it is still an unpublished draft.

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
