# Platform Resource Metadata Design

**Date:** 2026-09-28
**Status:** Approved for implementation

## Purpose

Applications can currently read a target memory ceiling and whether that ceiling is per execution
or shared by one runtime instance. That is insufficient for fail-closed startup validation: a shared
ceiling also needs the maximum number of live inbound request executions charged to it, and every
ceiling needs an explicit statement about whether provider-owned ingress framing memory is charged
against it. Absence must remain distinguishable from a known value.

This design publishes those facts without moving application admission policy into EdgeZero.

## Platform facts

`edgezero-core` owns a generic fact wrapper:

```rust
pub enum PlatformFact<Value> {
    Known {
        value: Value,
        source: PlatformResourceSource,
    },
    Unknown {
        reason: PlatformUnknownReason,
    },
}
```

`PlatformResourceSource` identifies a published platform limit, a hosted default, or a runtime
configuration. `PlatformUnknownReason` distinguishes operator configuration, runtime configuration,
provider-unpublished facts, and unspecified metadata. Unknown is data, not an absent field and not
permission to substitute an unrelated provider limit.

`PlatformMetadata` carries three facts:

- `PlatformFact<MemoryCeiling>`;
- `PlatformFact<InboundRequestPopulationBound>`, whose non-zero value is the maximum number of live
  inbound request executions charged to the same memory ceiling;
- `PlatformFact<HostIngressMemoryAccounting>`, whose known values are `CountsTowardCeiling` and
  `OutsideCeiling`.

`MemoryCeiling` retains exact primary bytes, optional separately published stack bytes, and
`MemoryCeilingScope`. Provenance moves to the fact wrapper so every resource fact uses one source
model.

## Canonical target facts

| Target | Memory ceiling | Live requests per memory domain | Host ingress/framing charge |
| --- | --- | --- | --- |
| Axum | Unknown: operator configured | Unknown: operator configured | Unknown: operator configured |
| Cloudflare Workers | Known: 128,000,000 bytes per instance | Unknown: provider unpublished | Unknown: provider unpublished |
| Fastly Compute | Known: 128,000,000-byte heap plus separate 1,000,000-byte stack per execution | Known: 1 per execution | Unknown: provider unpublished |
| Spin (generic) | Unknown: runtime configured | Unknown: runtime configured | Unknown: runtime configured |
| Akamai Functions (Spin) | Known hosted default: 134,217,728 bytes per execution | Known hosted default: 1 per execution | Unknown: provider unpublished |

The Cloudflare six-simultaneous-connection limit is not an inbound population bound. It applies to
outbound connections waiting for headers within one invocation and must not populate this metadata.
All adapters currently receive host-parsed requests, so EdgeZero cannot infer host parser or HPACK
accounting from guest-visible allocations.

## Validation

Applications describe the arithmetic they intend to validate with a scope-tagged `MemoryEnvelope`:

```rust
pub struct MemoryEnvelope {
    scope: MemoryCeilingScope,
    fixed_bytes: u64,
    per_live_request_bytes: u64,
    separate_stack_bytes: Option<u64>,
}
```

`PlatformMetadata::validate_memory_envelope` uses checked arithmetic and returns:

- `Fits` only when the ceiling, request-population bound, and host-ingress accounting are all known,
  the envelope scope matches the ceiling scope, primary required bytes do not exceed the primary
  ceiling, and any separately published stack allowance has a supplied requirement within its
  limit;
- `Exceeds` when the known minimum primary or separate-stack requirement already exceeds its
  ceiling, even if another fact is unknown;
- `Indeterminate` with typed reasons whenever an unknown fact prevents a complete result.

For `PerExecution`, required bytes are `fixed + per_live_request`; the population bound must be one
when known. For `PerInstance`, required bytes are `fixed + per_live_request * max_live_requests`.
When the per-instance population is unknown, the validator uses one as the checked minimum only to
decide whether `Exceeds` is already certain; otherwise the result remains `Indeterminate`.

A separately published stack ceiling is checked independently. Omitting the envelope's separate
stack requirement when the platform publishes one makes the result `Indeterminate`; supplying a
separate stack requirement when the platform publishes no separate allowance is an error because
the primary envelope must then include that memory. An envelope whose scope differs from the
published ceiling is an error, as is arithmetic overflow or a non-one population bound attached to
a per-execution ceiling. These failures use a public typed validation error rather than strings.

`per_live_request_bytes` must include every charge the application claims is covered. If host
ingress memory is known to count toward the ceiling, the caller must include its bounded charge. An
unknown accounting fact prevents `Fits`; callers cannot override it through this API.

The validation result describes the complete published memory domain. Applications may separately
validate narrower, explicitly named managed budgets, but must not reinterpret `Indeterminate` as a
complete fit.

## Adapter API

The CLI adapter trait hard-cuts `memory_ceiling()` to `platform_metadata()`. Runtime entrypoints and
CLI adapters return the same canonical `PlatformMetadata` constant, preventing each new resource
fact from creating another parallel trait method. The default returns fully unknown metadata.

No compatibility alias remains.

## Batch observation cutoff

The batch method names remain unchanged. Public parameter names, examples, and documentation use
`observation_cutoff`. The contract states that this instant stops observation regardless of a
slot's remaining request budget. Passing an earlier instant intentionally produces unresolved slots
at cutoff.

No debug assertion requires the observation cutoff to cover every request deadline: early cutoffs
are a supported partial-result operation. `BudgetSource::BatchCutoff` continues to identify the
selected effective slot budget and does not, by itself, signal batch termination.

## Migration record

The repository gains an unreleased changelog with an exact migration table:

| Removed or changed API | Replacement |
| --- | --- |
| `MemoryCeilingSource` | `PlatformResourceSource` inside `PlatformFact::Known` |
| `MemoryCeiling::new(total, scope, stack, source)` | `MemoryCeiling::new(total, scope, stack)` wrapped in `PlatformFact::known(value, source)` |
| `MemoryCeiling::source()` | Match or inspect the enclosing `PlatformFact` |
| `PlatformMetadata::new(Option<MemoryCeiling>)` | `PlatformMetadata::new(memory, inbound_population, host_ingress_accounting)` |
| `PlatformMetadata::memory_ceiling() -> Option<_>` | `PlatformMetadata::memory_ceiling() -> PlatformFact<_>` |
| `Adapter::memory_ceiling()` | `Adapter::platform_metadata()` |
| overridable `Hooks::build_app()` and infallible `configure` | `App::build::<A>(platform)` plus fallible `Hooks::configure` |
| four-argument `OutboundResponse::new` | five-argument constructor requiring the application `MonotonicClock` |

The demo application and generated project use the same current APIs; no stale compatibility path
is retained.

## Verification

Tests cover fact construction, each canonical adapter profile, every validation outcome, scope and
arithmetic failures, per-execution population invariants, documentation drift, generated projects,
the demo workspace, native feature matrices, and all three WASM targets.
