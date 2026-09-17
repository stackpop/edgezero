# Fastly Version-Scoped Runtime Selectors Design

**Status:** Approved

**Date:** 2026-09-16

**Supersedes:** the Fastly selector-store lifecycle in
`docs/superpowers/specs/2026-07-08-edgezero-deploy-github-actions-design.md`
§5.5.2

## 1. Problem

Fastly Compute has no ordinary process environment at request time. EdgeZero
therefore reads runtime settings from a Config Store linked under the stable
alias `edgezero_runtime_env`.

The current deployment work writes canonical selectors such as
`EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME` into unscoped,
mutable entries. It also creates a mutable per-service staging selector store.
That model has several correctness failures:

1. services linked to the same physical selector store overwrite one another;
2. all versions of one service observe a mutable selector at once, so a selector
   change cannot be atomic with a version activation or rollback;
3. a staged clone retains production Config, KV, and Secret Store links;
4. production can select a store that is not linked to the activated version;
5. a failed staging attempt can modify the selector store used by the currently
   staged version;
6. legacy service-scoped, unscoped, and staging-twin selector paths make the
   lifecycle harder to reason about and must be removed rather than extended;
7. manifest variable defaults reach a manifest deploy subprocess but not the
   parent-side Fastly finalizer;
8. lifecycle-owned Fastly flags can re-enter through direct CLI passthrough;
9. generic custom adapters with declared stores are forced to register solely
   because Fastly needs finalization;
10. service-ID validation differs across actions; and
11. the application workflow example selects the requested domain directly
    instead of the validated GitHub Environment name.

Green CI does not prove these properties because the existing fakes do not model
multiple services, old and new versions reading concurrently, inherited
production links, or failures between activation and selector writes.

## 2. Goals

- Keep public deployment inputs canonical and adapter-independent:
  `EDGEZERO__STORES__<KIND>__<LOGICAL_ID>__NAME` and Config Store `__KEY`.
- Allow production and staging to select the same or different physical Config,
  KV, and Secret Stores.
- Keep exactly one physical `edgezero_runtime_env` Config Store. Do not create a
  selector Config Store per service or per version.
- Make a selector immutable for the lifetime of one service version.
- Prepare selectors and exact resource links before staging or activation.
- Keep service IDs out of GitHub variable names. Service and version scoping is
  an internal Fastly storage detail.
- Remove the PR #344 selector persistence path and the mutable staging-twin
  lifecycle. New deploys never read or write their scoped or unscoped entries.
- Preserve manifest-defined deploy commands for adapters that do not claim a
  managed deployment.
- Treat the prebuilt application CLI, compiled Fastly package, and app-owned
  manifests as one immutable application release. Build it once, identify it by
  digest, and use the exact same bytes for every service, publisher, staging
  environment, and production environment selected for that release.
- Keep deployment-time runtime configuration out of every package build input.
  GitHub Environment values may change descriptors and resource links, but never
  the package bytes or the manifest used to describe the application.

## 3. Non-goals

- Recover from a store being deleted by an operator during a deployment.
- Add concurrent deployments for the same Fastly service. The deployer continues
  to serialize lifecycle operations per service.
- Add automatic garbage collection of old version descriptors in this change.
  A later bounded GC can remove descriptors after the corresponding Fastly
  versions are no longer rollback or staging targets.
- Promote a staged Fastly version to production. The deployer publishes the same
  immutable release separately through the production and staging GitHub
  Environments.
- Change Cloudflare, Spin, or Axum runtime configuration semantics.
- Define where an application publishes or retains its immutable release
  artifacts. EdgeZero validates and consumes the release; the application may
  distribute it through GitHub artifacts, a release, or another immutable
  artifact store.
- Migrate or fall back to PR #344 service-scoped entries, unscoped selector
  entries, or staging-twin stores. They are unsupported input to the new
  lifecycle.

## 4. Options considered

### 4.1 Chosen: one version-scoped descriptor entry

Keep one physical `edgezero_runtime_env` store and write one JSON descriptor for
each Fastly service version. The runtime derives the descriptor key from
Fastly's runtime `service_id()` and `service_version()` values.

This keeps old and new selectors independent without proliferating physical
stores. It also makes rollback natural: reactivating an old version makes that
version read its original descriptor.

### 4.2 Rejected: stable logical resource-link aliases only

Linking each selected physical resource under the logical store ID removes most
`__NAME` lookups. It does not solve Config Store `__KEY` selection when staging
and production share one physical Config Store, and it changes the runtime
binding contract more broadly than necessary.

### 4.3 Rejected: reorder writes around activation

Writing one shared selector before activation changes the old active version;
writing it afterward exposes the new version to the old selector. No ordering
can make a versionless mutable entry atomic with Fastly activation.

## 5. Descriptor contract

The physical store remains linked under `edgezero_runtime_env`. A descriptor key
is internal and deterministic:

```text
EDGEZERO__SERVICES__<FASTLY_SERVICE_ID>__VERSIONS__<VERSION>__ENV_V1
```

This is not an environment-variable name and is never configured in GitHub.
Fastly service IDs and versions come from the provider, so GitHub's variable-name
normalization is irrelevant.

The value is a versioned JSON object:

```json
{
  "format": 1,
  "entries": {
    "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME": "config-stage",
    "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY": "app_config_staging",
    "EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME": "credentials-stage"
  }
}
```

Only EdgeZero's fixed runtime allowlist and selectors for store IDs declared by
the application are serialized. Keys are sorted for deterministic tests and
diagnostics. The deploy fails before stage or activation if the serialized value
exceeds Fastly's Config Store entry limit.

The runtime uses a fallible `RuntimeDescriptor` parser rather than feeding raw
JSON directly to `EnvConfig::from_vars`. The parser:

1. opens `edgezero_runtime_env`;
2. derives the descriptor key from the current Fastly service and version;
3. parses format `1` with a serde map visitor that detects duplicate fields and
   duplicate entry names rather than accepting JSON's last value;
4. requires every entry name to appear in the runtime allowlist derived from the
   app's declared stores;
5. validates values by key type: store names and keys must be nonblank and free
   of control characters, ports and booleans must parse, and logging levels must
   be recognized; and
6. builds `EnvConfig` only after the whole descriptor passes validation.

`runtime_env_config` becomes fallible and `run_app` propagates its typed error to
request handling before constructing store registries. Custom Fastly entry
points must handle the same result. A missing selector store or descriptor is
allowed only when the app declares no stores, in which case the empty
`EnvConfig` preserves baked defaults with a diagnostic. An app that declares
any Config, KV, or Secret Store requires a valid version descriptor. Malformed,
unsupported, missing, or invalid data fails closed. Managed production and
staging therefore write a descriptor even when every selected physical name is
the logical default.

## 6. Environment resolution

`AdapterDeployContext` carries non-secret manifest variable defaults as typed
data. The Fastly adapter constructs the deployment environment with this
precedence:

1. parent process environment;
2. applicable manifest `[environment.variables]` default; and
3. baked logical store defaults.

The same resolved values are used by manifest subprocesses and adapter-owned
deployment work. Secrets and provider credentials remain in the existing
credential boundary and are not copied into a debug-printable context object.

The GitHub Environment supplies identical canonical variable names for
production and staging. It may give them equal values to share a physical store
or different values to isolate the environments.

These values are deployment inputs only. They are captured after the immutable
application release has been selected and must never be inherited by a compiler,
build script, package command, or manifest-selection step.

### 6.1 Immutable application release

An application release is a gzip-compressed tar archive supplied through the
typed `app-release-archive` action input. A required
`app-release-sha256` input pins the complete archive. The archive contains the
prebuilt application CLI, prebuilt Fastly package, and the exact `edgezero.toml`
and referenced Fastly manifest that defined that package. Its strict, versioned
metadata records the source revision and confined relative paths and SHA-256
digests for all four files. Duplicate or unknown metadata fields, absolute paths, traversal,
symlinks, non-regular files, extra archive members, and digest mismatches are
rejected before a provider mutation. The release is created before a deployer
selects a GitHub Environment and before it receives provider credentials.

The metadata is strict JSON with this shape:

```json
{
  "format": 1,
  "source_revision": "<full commit ID>",
  "adapter": "fastly",
  "app_cli": {
    "path": "cli/app-cli.tar.gz",
    "sha256": "<64 lowercase hex characters>"
  },
  "package": {
    "path": "pkg/app.tar.gz",
    "sha256": "<64 lowercase hex characters>"
  },
  "manifests": {
    "edgezero": {
      "path": "edgezero.toml",
      "sha256": "<64 lowercase hex characters>"
    },
    "adapter": {
      "path": "path/to/fastly.toml",
      "sha256": "<64 lowercase hex characters>"
    }
  }
}
```

`source_revision` is a full 40- or 64-character lowercase hexadecimal Git
object ID. Metadata paths use `/`, are relative to the release root, and resolve
to distinct regular files within it. Apart from required parent directory
members, the archive contains exactly `release.json` and those four files. The
application CLI archive retains the existing strict `app-cli-meta.json` plus
executable contract.

Typed app-config content is deliberately absent from the release because it is
runtime configuration and may differ by publisher or environment. Consequently,
`config-push-fastly` removes its manifest selector, always passes the bundled
`edgezero.toml` explicitly, and requires exactly one of a publisher-supplied
`app-config` file or `app-config-inline` value. It never falls back to a config
file beside the bundled manifest.

The application owns these manifests. Different applications may publish
different manifests, and a later application release may change them. A deployer
cannot replace, patch, or select a different manifest for staging, production,
or a particular publisher. It selects one immutable release and supplies only
the destination service, publish target, credentials, and canonical runtime
configuration.

Adapter-managed Fastly deployment requires this release input. It verifies the
metadata and digests before provider mutation, passes the verified package to
`fastly compute update --package`, and never invokes `cargo build`,
`fastly compute build`, or any manifest build command. The package digest is
emitted as deployment metadata so separate publisher and environment runs can
prove they deployed identical bytes.

The Fastly action extracts the verified archive into its action-owned temporary
workspace, sets `EDGEZERO_MANIFEST` to the one manifest recorded by the release,
and supplies the confined release root through the top-level typed
`--application-release <path>` deploy option. The generic CLI copies that path
and the exact loaded application-manifest path into its provider-neutral deploy
context. The Fastly adapter independently validates the release metadata, the
loaded application-manifest path, the referenced Fastly-manifest path, and all
file digests. The generic CLI carries paths and dispatch ownership only; it does
not branch on Fastly or interpret provider package metadata.

The application CLI is control-plane tooling rather than deployed workload code,
but it is pinned inside the same release so deployment behavior cannot vary by
publisher. Deploy, config push, healthcheck, and rollback extract and run that
exact CLI. It cannot rebuild or rewrite the selected application release during
deployment.

The application release pipeline is the sole producer. It checks out one full
source revision, builds the application CLI and Fastly package without a GitHub
Environment or provider credentials, copies that revision's app-owned manifests,
writes the strict metadata, creates the archive once, and publishes the archive
with its SHA-256. Publisher deployers consume that published archive; they never
recreate it from source. This change defines and validates the release format
while leaving the choice of artifact registry to the application.

## 7. Managed Fastly deployment

The adapter registry exposes provider-neutral deploy preflight and ownership
capabilities. The default preflight accepts the arguments and the default owner
is `ManifestCommand`, preserving existing behavior for custom or unregistered
adapters. A registered adapter may validate its provider arguments and return
`AdapterManaged` for a specific typed deployment context.

Fastly claims adapter-managed deployment when an application release is present,
staging is requested, or the manifest declares Config, KV, or Secret Stores.
Every `deploy-fastly` action invocation supplies a release, so its store-free and
store-aware deployments both use the same no-build state machine. A direct,
store-free production CLI invocation without a release may continue to use a
manifest command. This avoids provider-name conditionals in the generic CLI and
does not require an unregistered custom adapter merely because its manifest
declares stores.

Production and staging share three phases.

### 7.1 Read-only preflight

Before creating or editing a Fastly version, the adapter:

1. validates the alphanumeric service ID and parses passthrough arguments;
2. validates the application release and derives its package path and digest;
3. resolves the effective environment and builds the canonical descriptor in
   memory, including its size check;
4. lists complete account inventories for Config, KV, and Secret Stores,
   validates every record, rejects duplicate or cross-kind resource IDs, and
   resolves every selected physical store from those inventories;
5. resolves the exact `edgezero_runtime_env` physical store from the Config Store
   inventory;
6. resolves the current active version, if any;
7. reads that version's descriptor, when present, and its resource links without
   reading legacy selector entries or staging-twin stores;
8. when there is no active version, resolves the service's existing initial
   editable version and inventories all versioned configuration on that exact
   draft; and
9. constructs and validates the complete desired link plan.

These calls do not change provider state. Release metadata and digests are
validated before creating a remote draft; there is no Compute build in the
deployment lifecycle.

### 7.2 Editable version

When an active version exists, the adapter:

1. runs `fastly compute update --autoclone --version=active` with the one
   verified release package;
2. parses and immediately emits the exact draft `version=<N>`; and
3. applies the optional comment to that exact editable version.

When the service has no active version, the adapter does not create a blank
version. A newly created Fastly service already has an initial editable version,
and that is where Fastly's first-deploy setup, domains, backends, logging
endpoints, and other versioned configuration belong. During preflight the
adapter lists the service versions and requires the highest-numbered version to
be an unlocked, unstaged draft. It inventories that draft's domains, backends,
logging endpoints, resource links, and service-version settings, and uses the
same version as the link-plan source and deployment target. It then:

1. emits that exact numeric version before any draft mutation;
2. uploads the verified release package with `fastly compute update
   --service-id=<service-id> --version=<N>` and no `--autoclone`; and
3. applies the optional comment to that exact editable version.

The adapter never synthesizes or guesses first-deploy infrastructure from
`fastly.toml`. If the service has no version, the latest version is locked or
staged, or the inventory changes between preflight and mutation, deployment
fails before activation and tells the operator to initialize or repair the
Fastly service configuration. This preserves configuration created through the
Fastly UI, API, Terraform, or an earlier first-deploy setup instead of replacing
it with an empty version.

If cloning or upload fails, nothing new is active or staged. Once the adapter
has selected or Fastly has returned a version, the action retains that output
even when a later step fails. The inactive version is the recovery target and
may be inspected or reused manually; Fastly does not support deleting an
individual inactive version. A version that was already staged can be
deactivated explicitly.

### 7.3 Prepare and publish

For either editable-version path, the adapter:

1. reconciles the already-validated resource-link plan on the draft;
2. creates the version descriptor only if absent;
3. accepts an existing descriptor only when its canonical bytes equal the
   desired value, making an identical retry idempotent;
4. rejects an existing descriptor with different contents rather than
   overwriting a version's environment;
5. reads the descriptor back and requires exact canonical equality; and
6. stages or activates the prepared version.

All fallible selector and link work happens before traffic can reach the new
version. Provider mutations after preflight affect only the new, unreachable
editable version and its unique descriptor key. A failure may leave an inactive
draft, partially reconciled links on that draft, or a descriptor that no live
version reads; it cannot modify the active or currently staged version. First
deployment has no previous traffic or rollback target. It follows the same
upload, prepare, revalidate, and publish ordering on the existing initial draft.
Immediately before publication, the adapter re-reads that draft's version
metadata and resource links and rejects concurrent or unexpected changes.

The action records a valid emitted version even if a later command fails, then
returns the original nonzero status. Recovery therefore knows which draft,
staged version, or newly active first version was created.

## 8. Resource-link reconciliation

The desired link plan is fully resolved before remote draft mutation. Each entry
contains the logical store ID, store kind, selected physical name, resolved
resource ID, and required link alias.

Reconciliation must:

- require `edgezero_runtime_env` to resolve to the expected Config Store ID;
- require every selected Config, KV, and Secret Store link to resolve to the
  selected resource ID;
- delete an inherited EdgeZero-managed alias when the new descriptor does not
  select it;
- preserve unrelated non-store resource links;
- preserve a shared link when production and staging select the same resource;
- reject alias collisions between different resource IDs; and
- finish before stage or activation.

When the source version has a version-scoped descriptor, the managed alias set
comes from a deployment-side descriptor parser. Unlike the runtime parser, it is
intentionally independent of the new manifest's declared store IDs: after
validating the descriptor structure and format, it recognizes every exact
`EDGEZERO__STORES__<CONFIG|KV|SECRETS>__<LOGICAL_ID>__NAME` entry and records its
validated physical-name value. This lets a later manifest remove or rename a
store and still remove the alias selected by the prior version. It ignores
unknown entry shapes and never turns arbitrary descriptor data into a deletion.

If the source version has no version-scoped descriptor, the adapter performs a
clean cutover on the unreachable draft. It classifies linked Config, KV, and
Secret Store resources from the complete, validated provider resource-ID
inventories, removes inherited store links that are not in the desired link set,
preserves links whose resource IDs are absent from all three store inventories,
and creates the exact desired links. Any inventory failure, malformed record,
duplicate ID, or cross-kind ambiguity fails before draft mutation. This
guarantees staging isolation without consulting old selector data. After the
first version-scoped descriptor is active, later removal and rename
reconciliation is exact.

## 9. Cutover from PR #344

The new lifecycle has no backward-compatibility reader or migration inventory
for PR #344. In particular, it never reads or writes:

- legacy service-scoped selector entries of the form
  `EDGEZERO__SERVICES__<SERVICE_ID>__<ADAPTER|LOGGING|STORES>__...`;
- unscoped canonical selector entries; or
- `edgezero_runtime_env_staging_<service-id>` stores.

The new descriptor key
`EDGEZERO__SERVICES__<SERVICE_ID>__VERSIONS__<VERSION>__ENV_V1` is explicitly
outside that legacy grammar and is the only service-scoped Config Store entry
the new lifecycle reads or writes.

Provisioning continues to create the one physical `edgezero_runtime_env` store
needed by the new runtime, but it no longer persists store mappings into that
store or prints unscoped-selector and staging-twin instructions. Deployment
resolves the new descriptor only from the normal contract in §6: parent
environment, then manifest defaults, then logical defaults.

Old entries and twins may remain as inert provider data until an operator deletes
them. New versions neither inspect nor link twins, and the CLI contains no code
to maintain them. Existing versions keep their own provider state; this change
does not add rollback compatibility logic for their selector format.

## 10. Validation and passthrough

Fastly service IDs are validated everywhere as `^[A-Za-z0-9]+$`, matching
Fastly's documented identifier contract. Deploy, active-version capture,
healthcheck, rollback, Rust CLI operations, and documentation examples use one
shared rule.

Fastly preflight runs before ownership dispatch whenever the Fastly adapter is
registered, including store-free manifest-command deploys. It first scans tokens
with an exact flag parser and rejects every spelling of lifecycle-owned flags,
including detached, inline, equals-short, and attached-short forms:

- `--service-id`, `--service-id=...`, `-s`, `-s=...`, and `-sVALUE`;
- `--service-name` and `--service-name=...`;
- `--version` and `--version=...`;
- `--autoclone`; and
- `--token`, `--token=...`, `-t`, `-t=...`, and `-tVALUE`.

For adapter-managed deployment, a second parser uses an allowlist rather than
forwarding unknown tokens. It accepts `--comment` (detached or inline) and the
documented non-targeting global booleans (`--accept-defaults`/`-d`,
`--auto-yes`/`-y`, `--debug-mode`, `--non-interactive`/`-i`, `--quiet`/`-q`, and
`--verbose`/`-v`). The verified package path comes only from the typed release
context; a caller cannot replace it with `--package`. Unknown flags and
positional arguments are rejected. A manifest command may keep its other custom
arguments after the universal reserved-flag scan because EdgeZero does not
reinterpret that command's private interface.

## 11. Application deployer documentation

The workflow preflight job validates the requested domain and emits the exact
GitHub Environment name. The deploy job uses that output:

```yaml
jobs:
  deploy:
    needs: preflight
    environment: ${{ needs.preflight.outputs.environment }}
    env:
      EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME: ${{ vars.EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME }}
      EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME: ${{ vars.EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME }}
```

`inputs.domain` remains the actual Fastly domain and healthcheck hostname. The
deployer preflight maps `app.example.com` to production and
`staging.app.example.com` to staging.

The application release pipeline publishes one archive and SHA-256 for each
approved source revision. The deployer resolves the requested application release
to that immutable archive before selecting the GitHub Environment. The archive
path and digest are not GitHub Environment variables and cannot be overridden per
publisher. Production and staging jobs then supply their own canonical runtime
variables while deploying the same package digest.

## 12. Error handling

- Resolve and validate the complete desired environment, stores, source links,
  prior version descriptor when present, and link plan before creating a remote
  draft. Validate the immutable release metadata and file digests in the same
  read-only phase. Deployment performs no local build work.
- Treat store-inventory failure, malformed records, duplicate resource IDs, and
  cross-kind classification ambiguity as read-only preflight failures.
- Never activate or stage a version after a selector, resource lookup, link, or
  descriptor verification failure.
- Emit the exact version immediately after Fastly creates the draft.
- Preserve nonzero status after publishing recovery metadata.
- Include the logical store ID and selected physical name in diagnostics, never
  secret values or credentials.
- Treat unsupported descriptor formats as an upgrade requirement rather than
  overwriting or guessing.

## 13. Verification

Tests must cover:

- two services sharing one `edgezero_runtime_env` physical store;
- old and new versions reading different immutable descriptors;
- absence of every PR #344
  `EDGEZERO__SERVICES__<SERVICE_ID>__<ADAPTER|LOGGING|STORES>__...`, unscoped,
  and staging-twin read/write path while the new version descriptor is still
  created and read;
- clean cutover from a source with no version descriptor, removing undesired
  inherited Config, KV, and Secret Store links without changing the source;
- zero provider mutations on store-inventory, record-validation, duplicate-ID,
  or cross-kind classification failure;
- environment values overriding manifest defaults and defaults filling absent
  parent values;
- production and staging selecting equal and different physical stores;
- removal of inherited production Config, KV, and Secret aliases from staging;
- store removal and rename after a prior version-scoped descriptor, including a
  prior logical ID absent from the new manifest;
- preservation of unrelated non-store and intentionally shared desired links;
- lookup, link, descriptor-write, verification, stage, and activation failures;
- first deployment through the service's existing initial editable version,
  preserving domains, backends, logging endpoints, and version settings;
- first deployment rejecting a missing, locked, staged, or concurrently changed
  initial version before activation;
- idempotent descriptor retries and rejection of conflicting descriptor bytes;
- fallible runtime parsing for missing, malformed, duplicate, unknown, and
  invalid descriptor data;
- version output surviving a later deployment failure;
- rejection of lifecycle-owned passthrough spellings;
- alphanumeric service-ID validation across every action and Rust path;
- unregistered custom adapters with declared stores retaining manifest-command
  dispatch;
- the validated GitHub Environment output remaining distinct from the hostname;
- one release package digest remaining identical across staging, production, and
  multiple publisher services while descriptors and resource links vary;
- rejection before provider mutation when the application CLI, release package,
  or either bundled manifest does not match its recorded digest; and
- absence of every compiler and package-build invocation from managed deploys,
  including when runtime selector variables are present.

Required gates remain the repository's Rust workspace checks, Fastly/Cloudflare/
Spin target checks, action smoke and lifecycle tests, shell/static analysis, and
documentation format/lint/build.
