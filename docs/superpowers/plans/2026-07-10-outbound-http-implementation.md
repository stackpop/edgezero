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
- The outbound design declares exactly seven capabilities. The matrix and its footnotes in
  spec §3.5.2 are the only authority for support levels.

## Locked Scope

- The public `proxy` API becomes `outbound` without compatibility aliases. Templates,
  public docs, generated projects, and `examples/app-demo` migrate in the same implementation
  series.
- Core remains runtime-independent and WASM-compatible: no Tokio, reqwest, Fastly, worker,
  or Spin SDK dependency enters `edgezero-core`.
- `EdgeError` gains `BadGateway`, attributed `GatewayTimeout`, and the distinct
  `ResponseTooLarge` outcome. Response overflow is not collapsed into `BadGateway`.
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
  outbound spec depends only on its `into_request()` contract.

## Phase Sequence

| Phase | Scope | Plan status |
| --- | --- | --- |
| 1a | Add `BudgetSource`, `BadGateway`, `GatewayTimeout`, `Deadline`, and the three deadline constants | Executable: [`2026-07-10-outbound-http-phase1a-error-time.md`](2026-07-10-outbound-http-phase1a-error-time.md) |
| 1b | Add outbound request/response types, canonical URI accessors, `DispatchBudget`, and `dispatch_budget` in a buildable dependency order | Not yet authored |
| 2 | Typed body errors, bounded/deadline-aware drains, normalization, and shared decoder primitives | Not yet authored |
| 3 | Manifest capability declarations, adapter metadata, resolver, CLI gates, and Spin host-drift validation | Not yet authored |
| 4 | Axum and Cloudflare outbound implementations and contract tests | Not yet authored |
| 5 | Spin hand-built WASI HTTP request/response state machines | Not yet authored |
| 6 | Fastly dispatch/harvest engine, dynamic backends, timers, and test seams | Not yet authored |
| 7 | Templates, `app-demo`, public docs, generated-project checks, and live-host characterization | Not yet authored |

The phase numbers after 1a are organizational guidance, not permission to split an
invariant across unbuildable commits. A phase plan may adjust boundaries when current code
requires it, but it must preserve the dependency order and acceptance contracts in the
specification.

## Plan-Authoring Gate

Before writing any later phase plan:

1. Re-read the relevant spec sections and inspect the current implementations and pinned
   SDK source.
2. Enumerate every touched file, including Cargo manifests and generated/template surfaces.
3. Keep platform behavior at adapter boundaries and portable value/arithmetic layers in
   core.
4. Define executable test seams. Do not fabricate nonexistent variants of
   `#[non_exhaustive]` enums with unsafe code or test-only public variants.
5. Separate no-runtime contract tests from live-host characterization. A mock must not be
   used to claim host cancellation or wire behavior.
6. Run the repository gates required by `CLAUDE.md`, plus generated-project,
   `examples/app-demo`, adapter-target, and documentation checks for phases that touch those
   surfaces.

## Readiness

Phase 1a is ready to execute independently. Later phases require their own reviewed plans;
this index must not be expanded into a second copy of the master design.
