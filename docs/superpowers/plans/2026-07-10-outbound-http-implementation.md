# Outbound HTTP Implementation Index

> **Status:** Phase index, not an executable implementation plan.
>
> The authoritative contract is
> [`2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md).
> This file deliberately does not duplicate adapter algorithms, capability matrices, or
> migration details from that specification. Read the relevant spec sections against the
> current tree before authoring each phase plan.

## Current Baseline

- Rust 1.95, edition 2024, resolver 2.
- Four runtime adapters: Axum, Cloudflare, Fastly, and Spin SDK 6 / WASI HTTP 0.3.
- Adapter dispatch has both `execute(..)` and `execute_capture(..)` entry points.
- The action set includes staged deploy, version emission, healthcheck, and rollback in
  addition to build/serve/deploy/auth.
- The outbound design declares exactly seven **outbound** capabilities. The matrix and its
  footnotes in spec §3.5.2 are the only authority for those support levels; other specs may
  add non-outbound cells to the shared enum.

## Locked Scope

- The public `proxy` API becomes `outbound` without compatibility aliases. Templates,
  public docs, generated projects, and `examples/app-demo` migrate in the same implementation
  series.
- Core remains runtime-independent and WASM-compatible: no Tokio, reqwest, Fastly, worker,
  or Spin SDK dependency enters `edgezero-core`.
- `EdgeError` gains `BadGateway { reason: BadGatewayReason }`, attributed
  `GatewayTimeout`, and the distinct
  `ResponseTooLarge { reason: ResponseLimitReason }` outcome. Response overflow is not
  collapsed into `BadGateway`, and reason/provenance fields remain outside the JSON wire
  envelope.
- `DispatchBudget` and `dispatch_budget` land together after `OutboundRequest` exposes its
  private budget-input carrier. They are not part of Phase 1a.
- Capability types are owned by `edgezero-core::manifest`. Consequently,
  `edgezero-adapter` adds a direct dependency on `edgezero-core` for the registry trait's
  public capability signature.
- Outbound capability enforcement applies to construction/deployment of the current
  runtime: build, serve, deploy, staged deploy, and demo. Both `execute(..)` and
  `execute_capture(..)` gate exactly once before shell or registry dispatch. Auth, version
  emission, healthcheck, rollback, provision, and config commands are outside this gate.
- Spin request-component setters and request-option setters have different error types.
  `RequestOptionsError::NotSupported` retains the outer monotonic deadline race and logs a
  BestEffort degradation; `Immutable` and `Other(..)` are internal setup failures.
- The inbound `RequestContext` body-state migration is owned by
  [`2026-08-22-inbound-body-design.md`](../specs/2026-08-22-inbound-body-design.md). The
  outbound spec depends only on its `into_request()` contract. Ingress admission,
  request-start/read deadlines, and raw framing rejection are not prerequisites for Phase
  1a and must not be implemented opportunistically in an outbound phase.
- The downstream response-write lifetime starts after a core `Response` reaches an adapter
  converter. Adapter phases may implement the bounded fallback required by this outbound
  design, but must not claim a general write deadline, native abort, or exactly-once egress
  completion until a dedicated response-egress contract is reviewed.

## Phase Sequence

| Phase | Scope | Plan status |
| --- | --- | --- |
| 1a | Add `BadGatewayReason`, `BudgetSource`, typed `BadGateway`, attributed `GatewayTimeout`, `Deadline`, and the three deadline constants | Executable: [`2026-07-10-outbound-http-phase1a-error-time.md`](2026-07-10-outbound-http-phase1a-error-time.md) |
| 1b | Add the pinned direct `url` dependency, outbound request/response types, one-time canonical URI construction, resource-limit builders, `DispatchBudget`, and `dispatch_budget` in a buildable dependency order | Not yet authored |
| 2 | Typed body errors, typed response-limit errors, encoded/header/Brotli/chunk controls, bounded/deadline-aware drains, normalization, and shared decoder primitives | Not yet authored |
| 3 | Manifest capability declarations, adapter metadata, paired target/contract resolver, CLI gates, and selected-component Spin host-drift validation | Not yet authored |
| 4 | Axum and Cloudflare outbound implementations, response-converter scheduling, host-event fairness, and contract tests | Not yet authored |
| 5 | Spin hand-built WASI HTTP request/response state machines, completion-error mapping, and cooperative raw/decoded-input fairness | Not yet authored |
| 6 | Fastly dispatch/harvest engine, dynamic backends, timers, and test seams | Not yet authored |
| 7 | Templates, `app-demo`, public docs, generated-project checks, and remaining live-host characterization | Not yet authored |

The phase numbers after 1a are organizational guidance, not permission to split an
invariant across unbuildable commits. A phase plan may adjust boundaries when current code
requires it, but it must preserve the dependency order and acceptance contracts in the
specification.

Phases 4-6 include each adapter's direct dependencies, independent `test-utils` feature,
native contract tests, and explicit native/WASM CI activation from spec §5.5. These gates
land with their adapter implementation rather than waiting for phase 7. Cloudflare's raw
subrequest encoding bridge and host-observed cancellation/encoding tests belong to phase 4.
Phase 2 includes the shared decoder's gzip-member/native-completion contract, transport-side
encoded cap, opt-in rechunker, response-header caps, and pre-allocation Brotli-window check
(§3.4.1/§3.4.5).
Phase 4 includes Axum's single `block_in_place` + `Handle::block_on` response-conversion
boundary and Cloudflare's deployed-proven host-event yield quotas, frozen-clock regressions,
manual response-body encoding, and null-body 205 branch (§4.1/§4.2).
Phase 5 plan authoring must verify the Spin SDK-resource runner/harness compatibility gate
in spec §5.5; HTTP/Preview 3 registration flags alone are not execution evidence. It must
also route `client::send`, `request_done`, and response completion through the same typed
classifier and make continuously-ready raw/decoded input yield cooperatively. Phase 6
includes known-`SendErrorCause` boundary tests and the buffered-batch versus streamed-upload
cleanup distinction (§4.3). These requirements do not change Phase 1a.

## Plan-Authoring Gate

Before writing any later phase plan:

1. Re-read the relevant spec sections and inspect the current implementations and pinned
   SDK source.
2. Enumerate every touched file, including Cargo manifests and generated/template surfaces.
3. Keep platform behavior at adapter boundaries and portable value/arithmetic layers in
   core.
4. Define executable test seams, their Cargo features, target gates, and CI commands using
   spec §5.5. Confirm each command executes tests rather than reporting zero tests behind
   a whole-file platform gate, and validate SDK-resource runner/harness compatibility.
   Test constructible known SDK enum variants directly; do not fabricate nonexistent variants of
   `#[non_exhaustive]` enums with unsafe code or test-only public variants.
5. Separate no-runtime contract tests from live-host characterization. A mock must not be
   used to claim host cancellation or wire behavior.
6. Include the resource-limit precedence matrix, every typed reason value, multi-member
   gzip drain-to-native-EOF cases, and the platform's exact abort/completion owner. A
   rechunker test may claim bounded emitted item size, not bounded transport allocation or
   process RSS.
7. Run the repository gates required by `CLAUDE.md`, plus generated-project,
   `examples/app-demo`, adapter-target, and documentation checks for phases that touch those
   surfaces. Generated-project verification explicitly runs
   `cargo test -p scaffold-probe-core --lib`; a build-only command is insufficient.

## Readiness

Phase 1a is ready to execute independently. Later phases require their own reviewed plans;
this index must not be expanded into a second copy of the master design.
