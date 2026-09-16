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
`EDGEZERO__STORES__SECRETS__TRUSTED_SERVER_SECRETS__NAME` into unscoped,
mutable entries. It also creates a mutable per-service staging selector store.
That model has several correctness failures:

1. services linked to the same physical selector store overwrite one another;
2. all versions of one service observe a mutable selector at once, so a selector
   change cannot be atomic with a version activation or rollback;
3. a staged clone retains production Config, KV, and Secret Store links;
4. production can select a store that is not linked to the activated version;
5. a failed staging attempt can modify the selector store used by the currently
   staged version;
6. existing service-scoped and current unscoped installations are not migrated;
7. manifest variable defaults reach a manifest deploy subprocess but not the
   parent-side Fastly finalizer;
8. lifecycle-owned Fastly flags can re-enter through direct CLI passthrough;
9. generic custom adapters with declared stores are forced to register solely
   because Fastly needs finalization;
10. service-ID validation differs across actions; and
11. the Trusted Server workflow example selects the requested domain directly
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
- Migrate installations written by PR #344 without retaining that user-facing
  naming scheme as the new configuration contract.
- Preserve manifest-defined deploy commands for adapters that do not claim a
  managed deployment.

## 3. Non-goals

- Recover from a store being deleted by an operator during a deployment.
- Add concurrent deployments for the same Fastly service. The deployer continues
  to serialize lifecycle operations per service.
- Add automatic garbage collection of old version descriptors in this change.
  A later bounded GC can remove descriptors after the corresponding Fastly
  versions are no longer rollback or staging targets.
- Promote a staged Fastly version to production. The deployer builds production
  and staging through their respective GitHub Environments.
- Change Cloudflare, Spin, or Axum runtime configuration semantics.

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
    "EDGEZERO__STORES__CONFIG__TRUSTED_SERVER_CONFIG__NAME": "ts_config_staging",
    "EDGEZERO__STORES__CONFIG__TRUSTED_SERVER_CONFIG__KEY": "trusted_server_config_staging",
    "EDGEZERO__STORES__SECRETS__TRUSTED_SERVER_SECRETS__NAME": "ts_secrets_staging"
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

## 7. Managed Fastly deployment

The adapter registry exposes provider-neutral deploy preflight and ownership
capabilities. The default preflight accepts the arguments and the default owner
is `ManifestCommand`, preserving existing behavior for custom or unregistered
adapters. A registered adapter may validate its provider arguments and return
`AdapterManaged` for a specific typed deployment context.

Fastly claims adapter-managed deployment when staging is requested or when the
manifest declares Config, KV, or Secret Stores. Store-free production deploys
may continue to use a manifest command. This avoids provider-name conditionals
in the generic CLI and does not require an unregistered custom adapter merely
because its manifest declares stores.

Production and staging share three phases.

### 7.1 Read-only preflight

Before creating or editing a Fastly version, the adapter:

1. validates the alphanumeric service ID and parses passthrough arguments;
2. resolves the effective environment and builds the canonical descriptor in
   memory, including its size check;
3. resolves every selected physical Config, KV, and Secret Store by name;
4. resolves the exact `edgezero_runtime_env` physical store;
5. resolves the current active version, if any;
6. reads that version's descriptor and resource links, or inventories both
   legacy selector formats during migration;
7. when there is no active version, resolves the service's existing initial
   editable version and inventories all versioned configuration on that exact
   draft; and
8. constructs and validates the complete desired link plan.

These calls do not change provider state. The Compute build is also completed
before creating a remote draft.

### 7.2 Editable version

When an active version exists, the adapter:

1. runs `fastly compute update --autoclone --version=active` with the validated
   package arguments;
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
2. uploads the built package with `fastly compute update
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
- preserve unrelated resource links;
- preserve a shared link when production and staging select the same resource;
- reject alias collisions between different resource IDs; and
- finish before stage or activation.

The managed alias set comes from a deployment-side descriptor parser. Unlike the
runtime parser, it is intentionally independent of the new manifest's declared
store IDs: after validating the descriptor structure and format, it recognizes
every exact
`EDGEZERO__STORES__<CONFIG|KV|SECRETS>__<LOGICAL_ID>__NAME` entry and records its
validated physical-name value. This lets a later manifest remove or rename a
store and still remove the alias selected by the prior version. It ignores
unknown entry shapes and never turns arbitrary descriptor data into a deletion.

During migration, the adapter also inventories aliases found in the current
service's `EDGEZERO__SERVICES__<SERVICE_ID>__...` entries, the current unscoped
canonical entries, and logical-ID defaults. Legacy data is used only when
classifying an alias already linked to the source version; it never supplies a
new selector value. If ownership of a legacy linked alias is ambiguous, the
adapter preserves it. After the first version-scoped descriptor is active,
subsequent removal and rename reconciliation is exact.

## 9. Migration from PR #344

Migration is deployment-owned, not a permanent runtime fallback. It covers both
formats that may exist before this design:

- PR #344 service-scoped entries in the production `edgezero_runtime_env` store;
  and
- this branch's unscoped canonical entries, including entries in a linked
  `edgezero_runtime_env_staging_<service-id>` twin.

1. if the active version has no version descriptor, read only the current
   service's scoped entries and canonical unscoped entries for migration
   inventory;
2. if the source version links a staging twin under `edgezero_runtime_env`, read
   that linked resource as the unscoped source rather than assuming the
   production store;
3. resolve the new descriptor only from the normal contract in §6: parent
   environment, then manifest defaults, then logical defaults; legacy values do
   not participate in selector precedence because two legacy formats can
   coexist and there is no reliable way to infer which one the active binary
   reads;
4. use scoped, unscoped, and default logical aliases only to classify inherited
   links already present on the source version, preserving an alias whenever
   legacy ownership is ambiguous;
5. write the new version descriptor before stage or activation; and
6. leave legacy entries intact while old Fastly versions may still be rollback
   targets.

New runtime versions read only their version descriptor. Users do not create or
set service-scoped environment variables. Legacy staging-twin stores remain
physically present but new versions never link them. Thus the one-store rule
applies to newly prepared versions during migration; deleting old twins and
legacy entries is deferred until no old rollback or staged version depends on
them.

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
forwarding unknown tokens. It accepts `--comment` (detached or inline), package
selection (`--package`, `--package=...`, `-p`, `-p=...`, or `-pVALUE`), and the
documented non-targeting global booleans (`--accept-defaults`/`-d`,
`--auto-yes`/`-y`, `--debug-mode`, `--non-interactive`/`-i`, `--quiet`/`-q`, and
`--verbose`/`-v`). It rejects unknown flags and positional arguments. A manifest
command may keep its other custom arguments after the universal reserved-flag
scan because EdgeZero does not reinterpret that command's private interface.

## 11. Trusted Server deployer documentation

The workflow preflight job validates the requested domain and emits the exact
GitHub Environment name. The deploy job uses that output:

```yaml
jobs:
  deploy:
    needs: preflight
    environment: ${{ needs.preflight.outputs.environment }}
    env:
      EDGEZERO__STORES__CONFIG__TRUSTED_SERVER_CONFIG__NAME: ${{ vars.EDGEZERO__STORES__CONFIG__TRUSTED_SERVER_CONFIG__NAME }}
      EDGEZERO__STORES__SECRETS__TRUSTED_SERVER_SECRETS__NAME: ${{ vars.EDGEZERO__STORES__SECRETS__TRUSTED_SERVER_SECRETS__NAME }}
```

`inputs.domain` remains the actual Fastly domain and healthcheck hostname. For
the Trusted Server deployer, preflight maps `ts.example.com` to production and
`staging.ts.example.com` to staging.

## 12. Error handling

- Resolve and validate the complete desired environment, stores, legacy state,
  and link plan before creating a remote draft. Local build work is not a
  provider mutation.
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
- migration from service-scoped entries without reading another service;
- migration from unscoped production entries and a linked legacy staging twin;
- environment values overriding manifest defaults and defaults filling absent
  parent values;
- production and staging selecting equal and different physical stores;
- removal of inherited production Config, KV, and Secret aliases from staging;
- store removal and rename after a prior version-scoped descriptor, including a
  prior logical ID absent from the new manifest;
- ambiguous legacy aliases being preserved while unambiguous legacy aliases are
  removed;
- preservation of unrelated and intentionally shared links;
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
  dispatch; and
- the validated GitHub Environment output remaining distinct from the hostname.

Required gates remain the repository's Rust workspace checks, Fastly/Cloudflare/
Spin target checks, action smoke and lifecycle tests, shell/static analysis, and
documentation format/lint/build.
