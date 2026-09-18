# Fastly Logical Resource Links Design

## Status

Accepted on 2026-09-17. This design supersedes the version-scoped runtime
descriptor design. EdgeZero does not put a Fastly service ID or version in an
`EDGEZERO__*` name.

## Problem

A Fastly application release must contain identical package bytes for staging,
production, and every publisher. The selected GitHub Environment may still
choose the same or different physical Config, KV, and Secret Stores for each
deployment target.

The discarded design copied deployment variables into one account-wide Config
Store and looked them up with a service/version key. That added a second source
of truth, required ownership metadata for resource links, and made deployment
correctness depend on mutable shared state.

Fastly already provides the two primitives EdgeZero needs:

- resource links bind a physical store to a service version under a link name;
- service-version environment records identify where a version is published.

## Public contract

Applications declare logical store IDs in `edgezero.toml`. Deployment-time
variables select physical resources:

```text
EDGEZERO__STORES__CONFIG__<ID>__NAME
EDGEZERO__STORES__KV__<ID>__NAME
EDGEZERO__STORES__SECRETS__<ID>__NAME
```

These variables are deployment inputs. They are not copied into Fastly runtime
configuration. Their names never contain a Fastly service ID.

When `__NAME` is absent, the physical store name defaults to the logical ID.
When it is present, including when a workflow exports an unset GitHub variable
as an empty string, it must contain a non-empty printable value or validation
fails before provider I/O.

Fastly config entry keys are deterministic:

```text
production: <logical-id>
staging:    <logical-id>
```

Fastly rejects a `__KEY` selection or CLI `--key` whose value differs from the
logical ID. Validation runs before config push, diff, or deploy performs
provider I/O. The selected deployment environment chooses the physical store
with `__NAME`; selecting the same store shares config, while selecting different
stores isolates it. Other adapters retain their existing `__KEY` behavior. The
adapter registry owns this difference; generic CLI and action code must not
branch on the adapter name.

A declared Secret Store remains optional at the application-data level: typed
optional secret references may be absent. When `[stores.secrets]` is omitted,
Fastly creates no Secret Store registry or resource link.

## Deployment model

For every declared store, EdgeZero resolves the selected physical name against
the complete Fastly inventory. It creates a resource link on the unpublished
target draft whose alias is the stable logical ID and whose resource ID is the
selected physical store. Resource-link identity is `(resource kind, logical
alias)`, not alias alone. EdgeZero parses and validates Fastly's provider
`resource_type` values (`config`, `kv-store`, and
`secret-store`); duplicate links for the same identity, an unknown resource
type, or a resource ID that conflicts with its reported type fails preflight.
The same logical ID may be used independently by Config, KV, and Secret Store
declarations.

When the source version already contains that resource kind and logical alias:

- the same physical resource is retained;
- a different physical resource causes the inherited link to be deleted and
  the selected link to be created before publication.

Links whose identities are not declared by the current manifest are preserved.
EdgeZero does not infer ownership from account inventories and does not delete
undeclared Config, KV, or Secret Store links.

During preflight, EdgeZero records the source version's Compute configuration
from the versioned domain, backend, health-check, logging, and settings
collections. Immediately before the first mutation it revalidates that
snapshot. A locked source is cloned explicitly; before package upload, the
fresh clone's protected configuration and resource links must exactly match
the preflight source. An initial editable draft must still match its preflight
snapshot immediately before package upload.

After every EdgeZero draft mutation is complete, EdgeZero performs one final
publication barrier immediately before staging or activation. The barrier
reads the exact draft links, provider-visible package identity, source version
state, draft version state, and the same protected Compute configuration.
EdgeZero captures the post-mutation collection snapshot and requires the
immediate final read to match after normalizing provider-owned version and
timestamp metadata. EdgeZero performs no mutation between this barrier and the
publication call. It does not use Fastly's version-diff endpoint because that
endpoint fails for Compute services.

The caller must serialize every deployment and other version mutator for one
service, including changes outside EdgeZero. Fastly exposes no compare-and-swap
token for publication, so an external actor can still create a
time-of-check/time-of-use race after a successful read. The source and clone
checks prevent an earlier foreign mutation from becoming the accepted baseline;
the final barrier detects later observed interference. Any mismatch fails
without publication.

Staging and production use different service versions of the same Fastly
service. A deployment clones its selected locked source, replaces declared
logical links with the target's selections, verifies the draft, and stages or
activates it. It never activates a staging-configured version directly.
Version-state decisions use exact `environments` records. Fastly's `deployed`
and `staging` version fields are unused and do not control source selection or
rollback; `locked` only determines whether a draft can be edited.

## Runtime model

The Fastly runtime opens stores by the stable logical IDs baked into the
application release. It never reads a physical store name, service ID, version,
deployment environment name, or runtime descriptor.

Config, KV, and Secret Stores require no target-specific runtime selector. The
resource link chooses the physical store, and Config Stores always read the
logical ID as the entry key.

Local Viceroy config push, diff, and runtime lookup use the logical aliases and
logical key. Remote config operations resolve the selected physical `__NAME`.
Tests prove the Fastly runtime does not branch on the publication target.

## Logging

Fastly logging settings come from `[adapters.fastly.logging]` in the application
manifest and are baked into application metadata by `app!`. `run_app` uses that
metadata unless the application owns logger initialization. Deployment-time
`EDGEZERO__LOGGING__*` variables are not Fastly runtime inputs after the
descriptor is removed. Changing Fastly logging settings requires a new
application release, and every publisher of that release receives identical
logging bytes and behavior.

## Selector validation

An exported but empty or invalid `__NAME` or `__KEY` is an error. It must not
be treated as absent. Config push, config diff, and managed deployment use the
same checked `EnvConfig` resolution before any provider mutation. Diagnostics
name the variable but do not print its value.

The Fastly config-key policy is an adapter hook. Generic configuration code
passes the checked canonical selection to the adapter. Fastly accepts only the
logical ID for every target; other adapters accept their configured key
unchanged. The application and runtime never receive a staging flag to choose a
store or key.

## Rollback

Production rollback keeps the existing active-version staleness check and
activates the captured previous version.

Staging rollback first reads the exact requested version state:

- a staged version is deactivated from staging;
- an unpublished editable draft is a successful no-op because publication did
  not occur;
- an active, missing, ambiguous, or otherwise incompatible version fails before
  mutation.

This makes a recovery step safe both before and after staging publication.
After deactivating a first deployment, the highest locked version with no
environment record is a retired staging source. A subsequent deployment clones
that source. If a failed retry already left one highest editable draft beside a
staged source, the next deployment revalidates and reuses that draft rather than
creating an ambiguous chain of clones.

## Action compatibility

`config-push-fastly` retains the old `key` input only as a deprecated guard. A
non-empty value fails during input validation before release extraction or any
provider mutation. The diagnostic directs callers to the canonical selector
contract and Fastly's fixed logical key.

`build-app-cli` describes its artifact as an input to application-release
assembly. Lifecycle actions consume the verified immutable application release,
not the standalone CLI artifact directly.

## Failure behavior

Deployment fails before publication when:

- a present selector is blank, contains control characters, or selects a
  missing or ambiguous physical store;
- a declared logical alias collides with an incompatible existing resource;
- link deletion, creation, or exact readback fails;
- the source version changes during deployment;
- the provider-visible package identity differs from the verified release.

A failed reconciliation leaves only an unpublished draft. Existing active and
staged versions retain their frozen packages and resource links.

The immutable application release preserves the Fastly manifest at the exact
relative path declared by `edgezero.toml`. `lifecycle_protocol: 1` is assigned
only after the packager verifies every lifecycle command and action-owned flag.
Every lifecycle consumer verifies that `release.json.source_revision` equals
the source revision selected before entering a publisher Environment.

Config push is a separate mutable operation. Config used by the current release
must remain backward-compatible until deployment and healthcheck complete. A
later failure requires restoring the prior value or re-pushing the prior
release's config to the same physical store and logical key; deployments that
cannot provide compatibility must use versioned physical stores.

## Verification requirements

Tests must prove:

- no runtime lookup, provisioning, descriptor read/write, or deployment logic
  uses a selector store;
- no runtime or deployment path constructs a service/version `EDGEZERO__*`
  key;
- production and staging deploy identical package bytes;
- the same physical Config Store and logical alias use the same config entry;
- different physical Config, KV, and Secret Stores are linked under the same
  stable logical aliases on their respective versions;
- the same logical ID can be reconciled independently for Config, KV, and
  Secret Store links by using resource kind as part of the identity;
- declared aliases are replaced while undeclared inherited links survive;
- invalid present selectors fail push and deploy before mutation;
- optional secrets continue to work;
- Fastly logging is sourced from baked application-manifest metadata and does
  not vary by publisher;
- failure before staging makes rollback a no-op, while failure after staging
  deactivates the exact version, and duplicate staging records anywhere in the
  service fail closed;
- deprecated `key` input fails before mutation;
- the packager preserves the manifest path and verifies the complete lifecycle
  command surface;
- every consumer binds the release to the selected source revision;
- preflight source snapshots and fresh-clone verification precede draft
  mutation, and the immediate final barrier re-reads exact links,
  provider-visible package identity, source state, draft state, and the
  protected Compute configuration before stage or activation.
