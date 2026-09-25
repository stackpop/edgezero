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

For one release, those application-owned members are byte-identical across every
publisher, deployer, production deployment, and staging deployment. A later
application release may change them. Runtime variables, selected physical
Config/KV/Secret stores, destination service, credentials, and publication target
may vary without changing the release.

## Producer and deployer boundary

The application release pipeline is the sole producer. It checks out an approved
source revision, builds the application CLI and Fastly package without a GitHub
Environment or provider credential, records their digests and the selected
Fastly manifest in `release.json`, archives the result, and publishes the archive
plus its SHA-256.

The deployer is a consumer. It resolves the application source revision and
release digest before selecting a publisher GitHub Environment. It downloads the
existing archive, verifies the pinned digest, and passes the same archive to every
lifecycle action. It does not check out or rebuild application source. A publisher
GitHub Environment supplies runtime values and credentials; it must never supply
the release reference or release digest.

The supplied producer actions retain GitHub Artifacts for 14 days. Deploy,
healthcheck, and rollback all require the same archive and digest. Copy them to
durable immutable storage when deployments must remain recoverable after that
retention window.

A typical application release layout is:

```text
release.json
cli/app-cli.tar
package/app.tar.gz
edgezero.toml
adapters/fastly/fastly.toml
```

The member paths are release metadata, so another release may arrange them
differently. `release.json` records `format: 1`, the 40- or 64-character lowercase
hexadecimal source revision, adapter `fastly`, required
`lifecycle_protocol: 1`, and the relative path and SHA-256 for each file.
Verification rejects unknown or duplicate fields, unsafe paths,
symlinks, extra files or directories, digest mismatches, and manifests that do
not match the application's recorded manifest relationship. The deployer cannot
substitute a package or manifest after verification.
The packager preserves the selected Fastly manifest at the exact relative path
declared by `edgezero.toml`. Other adapters can remain declared in the unchanged
application manifest, but their manifests are not Fastly deployment inputs. The
packager also verifies every lifecycle command and action-owned flag before
assigning `lifecycle_protocol: 1`.

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
          deploy-args: '["--comment","production release"]'
          deploy-to: production
```

For cross-repository downloads, the producer repository is explicit and the
token must have Actions read access there. The built-in workflow token works
only when the artifact was produced in the deployer repository.

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
as the key for every target. Selecting the same physical store shares config;
selecting a different `__NAME` isolates it. A conflicting `__KEY` fails before
provider mutation.
Fastly logging settings come from `[adapters.fastly.logging]` in the application
manifest and are baked into the package.

Managed deploy verifies the immutable release, including its selected source
revision, resolves complete Config, KV, and Secret inventories, prepares an
unreachable draft, and reconciles declared
`(resource kind, logical ID)` links. A declared link pointing at a different
physical resource is replaced. Undeclared inherited Config, KV, and Secret
Store links are preserved; an unknown provider resource type fails closed.
Before mutation, EdgeZero records the source version's protected Compute
configuration: domains, backends, health checks, logging endpoints, and
settings. For a locked source, it explicitly clones and verifies that the fresh
draft has the same configuration before uploading the package. After all
mutations, EdgeZero re-reads the exact links, source and draft state, protected
configuration, and provider-visible package identity immediately before staging or activation.
Any lookup, malformed inventory, changed source, package mismatch, or readback
failure stops publication.

Runtime descriptors and service-scoped selector keys are unsupported. Managed
deploy reconciles only the stores declared by the application and preserves
other recognized Config, KV, and Secret Store links.

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
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    fastly-service-id: ${{ vars.FASTLY_SERVICE_ID }}
    deploy-args: '["--comment","staging release"]'
    deploy-to: staging

- id: probe
  uses: stackpop/edgezero/.github/actions/healthcheck-fastly@<ref>
  with:
    app-release-archive: ${{ github.workspace }}/app-release/app-release.tar.gz
    app-release-sha256: ${{ needs.release.outputs.sha256 }}
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
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
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
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
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
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
    expected-source-revision: ${{ needs.release.outputs.source-revision }}
    fastly-api-token: ${{ secrets.FASTLY_API_TOKEN }}
    working-directory: publisher-config
    app-config: app.toml
    store: app_config
    deploy-to: production
```

`app-config` and `app-config-inline` are mutually exclusive. With
`deploy-to: staging`, Fastly still writes `<logical-id>` to the physical store
selected by that Environment. A conflicting canonical `__KEY` fails before
mutation. The
deprecated action `key` input is retained only to fail with migration guidance
to the physical-store `__NAME` selector when nonempty. Config push changes typed runtime data; it does not change the
application release, package, or manifests.
Config pushed before deployment must remain backward-compatible with the current
published release until deploy and healthcheck complete. If either later step
fails, restore the prior config value or re-push the prior release's config to
the same physical store and logical key before retrying. Use versioned physical
stores when compatibility cannot be guaranteed.

## Action reference

### `deploy-fastly`

| Input                      | Required | Default      | Meaning                                      |
| -------------------------- | -------- | ------------ | -------------------------------------------- |
| `app-release-archive`      | Yes      | —            | Local path to the immutable release archive. |
| `app-release-sha256`       | Yes      | —            | Expected lowercase SHA-256 of the archive.   |
| `expected-source-revision` | Yes      | —            | Source revision selected by the deployer.    |
| `fastly-api-token`         | Yes      | —            | Token exposed only to provider operations.   |
| `fastly-service-id`        | Yes      | —            | Alphanumeric destination service ID.         |
| `deploy-args`              | No       | `[]`         | At most one `--comment` in the JSON array.   |
| `deploy-to`                | No       | `production` | `production` activates; `staging` stages.    |

### `config-push-fastly`

| Input                      | Required | Default      | Meaning                                                               |
| -------------------------- | -------- | ------------ | --------------------------------------------------------------------- |
| `app-release-archive`      | Yes      | —            | The same pinned release archive.                                      |
| `app-release-sha256`       | Yes      | —            | Expected archive SHA-256.                                             |
| `expected-source-revision` | Yes      | —            | Source revision selected by the deployer.                             |
| `fastly-api-token`         | Yes      | —            | Token for the Config Store write.                                     |
| `working-directory`        | No       | `.`          | Publisher-owned typed-config directory.                               |
| `app-config`               | No\*     | empty        | Typed config file under `working-directory`.                          |
| `app-config-inline`        | No\*     | empty        | Inline typed config.                                                  |
| `no-env`                   | No       | `false`      | Skip the typed runtime environment overlay.                           |
| `store`                    | No       | manifest     | Logical Config Store ID.                                              |
| `key`                      | No       | empty        | Deprecated; any nonempty value fails before mutation.                 |
| `deploy-to`                | No       | `production` | Publication target whose selected physical store receives the config. |

\* Exactly one typed config input is required.

### `healthcheck-fastly`

`healthcheck-fastly` requires the release archive, digest, expected source
revision, service ID, version, and real deployment `domain`. `path`, retry
count, retry delay, timeout, and `deploy-to` are optional. Staging also requires
the Fastly token to resolve the version's staging IP; a production probe does
not receive the token.

### `rollback-fastly`

`rollback-fastly` requires the release archive, digest, expected source revision,
Fastly token, service ID, failed version, and target. Production additionally
requires `rollback-to`; staging inspects the supplied version, deactivates it
only when staged, and no-ops when it is still an unpublished draft.

## Managed deploy arguments

The action accepts JSON `deploy-args`, but Fastly's adapter-managed lifecycle
owns targeting, cloning, package selection, credentials, and publication. The
action wrapper accepts only `--comment` for a version comment. A direct managed
CLI invocation also accepts the non-targeting global booleans listed in the CLI
reference. Unsupported, duplicated, positional, or malformed arguments fail
before provider mutation.

## Runner requirements

The actions are tested on `ubuntu-latest`. A self-hosted runner must be Linux
x86-64 and provide Bash, `jq`, Mike Farah `yq` v4, `tar`, `curl`, `git`, `base64`,
`realpath`, and either `sha256sum` or `shasum`. The application CLI archive must
contain a Linux x86-64 executable.

## Security and concurrency

- Pin the release archive by digest and pin action references to a released tag or
  full commit SHA.
- Select release identity outside the publisher GitHub Environment.
- Give the application release producer no provider credential.
- Scope Fastly credentials to lifecycle steps and use protected GitHub
  Environments for runtime configuration.
- Serialize every deployment and other version mutator per Fastly service,
  including changes made outside EdgeZero. Source and draft snapshot checks
  assume no actor can mutate the service between a successful check and the next
  provider operation; recovery likewise assumes another run cannot publish a
  different version between capture and rollback.
- Use ephemeral runners. Cleanup is best effort after cancellation or process
  termination.
