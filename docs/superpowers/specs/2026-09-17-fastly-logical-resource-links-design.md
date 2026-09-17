# Fastly Logical Resource Links Design

## Status

Accepted on 2026-09-17. This design supersedes the version-scoped runtime
descriptor design. EdgeZero does not create or use `edgezero_runtime_env` as a
runtime configuration store, and it does not put a Fastly service ID or version
in an `EDGEZERO__*` name.

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
- the runtime reports whether a request is executing in Fastly staging.

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
staging:    <logical-id>_staging
```

An absent Fastly `EDGEZERO__STORES__CONFIG__<ID>__KEY` resolves to this
deterministic target key. Fastly rejects a `__KEY` selection or a CLI `--key`
whose final value differs from the target key. Validation runs after the usual
CLI, environment, and fallback precedence is resolved, and before config push
or diff performs provider I/O. Other adapters retain their existing `__KEY`
behavior. The adapter registry owns this difference; generic CLI and action
code must not branch on the adapter name.

A declared Secret Store remains optional at the application-data level: typed
optional secret references may be absent. When `[stores.secrets]` is omitted,
Fastly creates no Secret Store registry or resource link.

## Deployment model

For every declared store, EdgeZero resolves the selected physical name against
the complete Fastly inventory. It creates a resource link on the unpublished
target draft whose alias is the stable logical ID and whose resource ID is the
selected physical store. Resource-link identity is `(resource kind, logical
alias)`, not alias alone. EdgeZero parses and validates Fastly's
`resource_type`; duplicate links for the same identity, an unknown resource
type, or a resource ID that conflicts with its reported type fails preflight.
The same logical ID may be used independently by Config, KV, and Secret Store
declarations.

When the source version already contains that resource kind and logical alias:

- the same physical resource is retained;
- a different physical resource causes the inherited link to be deleted and
  the selected link to be created before publication.

Links whose identities are not declared by the current manifest are preserved.
EdgeZero does not infer ownership from account inventories and does not delete
undeclared Config, KV, or Secret Store links. The sole cleanup exception is a
Config Store link whose alias is `edgezero_runtime_env`, whose resource ID
resolves to the physical Config Store also named `edgezero_runtime_env`, and
whose identity is not declared by the application. EdgeZero removes that exact
legacy link from newly prepared versions. A same-named link of another kind, a
Config link to another physical store, or an application declaration using
that logical ID is preserved. The account-wide legacy store is left intact for
older versions and rollback.

After every EdgeZero draft mutation is complete, EdgeZero performs one final
publication barrier immediately before staging or activation. The barrier
reads the exact draft links, provider-visible package identity, source version
state, and draft version state and requires all four to match the preflight
plan. EdgeZero performs no mutation between this barrier and the publication
call. Deployments for one service must be serialized by the caller. Fastly does
not expose a compare-and-swap token for publication, so an external actor can
still create a time-of-check/time-of-use race; the final barrier minimizes and
detects earlier interference but cannot make the provider API atomic. Any
observed mismatch fails without publication.

Staging and production use different service versions of the same Fastly
service. A staging deploy clones the active version, replaces declared logical
links with staging selections, verifies the draft, and stages it. A production
deploy creates a new draft and reconciles production selections before
activation. It never activates a staging-configured version directly.

## Runtime model

The Fastly runtime opens stores by the stable logical IDs baked into the
application release. It never reads a physical store name, service ID, version,
deployment environment name, or runtime descriptor.

For Config Stores, `fastly::compute_runtime::is_staging()` selects the entry
key. Production reads the logical ID; staging reads the logical ID plus
`_staging`. KV and Secret Stores require no target-specific runtime selector
because the resource link chooses the physical store.

Local Viceroy config push, diff, and runtime lookup use the logical aliases and
production logical key. Remote config operations still resolve the selected
physical `__NAME`. Tests exercise staging key derivation through a pure helper
that accepts a Boolean target instead of relying on a provider runtime.

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
deterministic production or staging key; other adapters accept their configured
key unchanged.

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

## Action compatibility

`config-push-fastly` retains the old `key` input only as a deprecated guard. A
non-empty value fails during input validation before release extraction or any
provider mutation. The diagnostic directs callers to the canonical selector
contract and Fastly's deterministic target keys.

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

## Verification requirements

Tests must prove:

- no runtime lookup, provisioning, descriptor read/write, or newly created link
  references `edgezero_runtime_env`; the sole permitted deployment behavior
  narrowly removes the exact undeclared legacy Config Store link described
  above and preserves same-named non-legacy identities;
- no runtime or deployment path constructs a service/version `EDGEZERO__*`
  key;
- production and staging deploy identical package bytes;
- the same physical Config Store uses base and `_staging` keys;
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
  deactivates the exact version;
- deprecated `key` input fails before mutation;
- the immediate final barrier re-reads exact links, provider-visible package
  identity, source state, and draft state before stage or activation.
