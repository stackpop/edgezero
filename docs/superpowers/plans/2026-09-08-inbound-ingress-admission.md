# Inbound Ingress Admission Implementation Plan

> **Status:** Partially implemented in PR #275. Core route resolution, admission, grants,
> request timing, lazy bounded body state, adapter entry seams, and the opt-in bounded body
> drain before canonical 404/405 responses are implemented. The Axum raw-parser boundary and
> deployed cancellation probes remain open; raw framing/head-limit capabilities stay
> `Unsupported`, and every current adapter reports host-managed head accounting and framing.
> This plan is owned by the
> [inbound-body design](../specs/2026-08-22-inbound-body-design.md), not the outbound HTTP
> implementation phases.
>
> Checkboxes record implementation status. Mixed adapter/certification tasks remain unchecked
> until every assertion in that task has named evidence.

**Goal:** Add stable pre-dispatch route resolution, one app-owned ingress admission gate,
request-owned grants, absolute body-read deadlines, parser-level request-target/header
limits and raw HTTP/1 framing rejection on Axum, and lazy bounded inbound-body consumption
across all adapters.

**Architecture:** Each adapter stamps one monotonic request start and validates raw framing
where possible. Core resolves the route once without dispatching, admission sees that stable
resolution, and admitted requests dispatch through an opaque single-use token. The router
constructs `RequestContext` with the original start, route metadata, one-shot grant, and a
lazy deadline-bound body. Core owns the body cache/poison state machine; adapters own native
read cancellation and honest capability claims.

**Dependencies:** The outbound Phase 1a `Deadline`, `BudgetSource`, and `BadGatewayReason`
work, plus the Phase 2 `ResponseLimitReason` work, must be present before the total
`StoredError` match lands. No outbound adapter client phase is otherwise required.

## Global Constraints

- Do not add Tokio to core or shared adapter crates.
- Route resolution may run before admission; middleware, handlers, extractors, and body
  polling may not.
- A resolved token is consumed once and cannot be replayed against another router or a
  changed method/path.
- Every admitted body has one finite absolute deadline. Relative per-read timeout resets are
  forbidden.
- Raw head limits and framing validation must use a pre-normalization parser boundary. A
  normalized target or `HeaderMap` check cannot claim parser-allocation or
  request-smuggling protection.
- Preserve `RequestContext::new(Request, PathParams)` for low-level callers; it records no
  route or admission grant.
- Follow red-green-refactor for every behavioral task and run the focused test after each
  production edit.

## Task 1: Stable route identity and single-use resolved dispatch

**Files:** `crates/edgezero-core/src/router.rs`, route tests, public re-exports.

- [x] Write red tests for structural `RouteId(method, registered_pattern)`, dynamic path
  independence, optional manifest route class on matched/405 metadata, deterministic 405
  candidates, and `NotFound`.
- [x] Write red tests proving `resolve` executes no middleware/handler/body poll and captures
  exact path params.
- [x] Write red tests proving `dispatch_resolved` does not rematch, consumes its token, and
  rejects a foreign-router token or changed method/path as `Internal`.
- [x] Add `RouteId`, class-bearing `RouteMetadata`, `RouteResolution`, opaque
  `ResolvedDispatch`, `RouterService::resolve`, `RouterBuilder::route_with_class`, and
  `dispatch_resolved` exactly as specified. Route class must not enter `RouteId`.
- [x] Run `cargo test -p edgezero-core --lib router`.

## Task 2: App admission policy, parser limits, and normalized ingress head

**Files:** `crates/edgezero-core/src/app.rs`, app tests, macro/configure tests.

- [x] Write red tests for validated nonzero `IngressHeadLimits`, `IngressHeadAccounting`,
  `IngressFraming`, immutable/body-blind `IngressHead`, `AdmissionDecision`, and non-clone
  opaque `IngressGrant` downcast behavior.
- [x] Write red ordering tests: framing, resolution, admission, then dispatch; refusal skips
  middleware/handler/body polling and retains the chosen response.
- [x] Add the synchronous policy and immutable head-limit setters on `App`; preserve both
  through complete app-to-service construction rather than cloning only `app.router()`.
- [x] Add the cloneable app-owned `MonotonicClock`, its setter/snapshot accessors, and tests
  proving admission retains that exact handle rather than resnapshotting a global clock.
- [x] Add the default decision: empty grant plus
  `request_start + DEFAULT_INBOUND_READ_BUDGET` (30 seconds). Clamp every policy deadline
  to `request_start + DEADLINE_FAR_FUTURE` with checked arithmetic; reject overflow or an
  invalid policy result before body ownership moves.
- [x] Run `cargo test -p edgezero-core --lib app`.

## Task 3: RequestContext ingress metadata and grant lifetime

**Files:** `crates/edgezero-core/src/context.rs`, router/context tests.

- [x] Write red tests proving `IngressHead` and routed `RequestContext` expose the identical
  `MonotonicInstant`, paired `MonotonicClock`, and class-bearing `RouteMetadata` values.
- [x] Write red drop-count tests for taken, untaken, refused, 404, and 405 grants. A second
  `take_ingress_grant()` returns `None`; no grant type is clonable or serialized.
- [x] Preserve `RequestContext::new(Request, PathParams)`: snapshot start at construction,
  expose no route, return no grant, and make no admission claim.
- [x] Add a crate-private routed constructor consuming `AdmittedIngress`; initialize all
  metadata and the admitted clock before middleware starts.
- [x] Run `cargo test -p edgezero-core --lib context`.

## Task 4: Lazy body cell, bounded extractors, and absolute deadline

**Files:** `crates/edgezero-core/src/body.rs`, `context.rs`, `extractor.rs`, `error.rs`.

- [x] Write red state-machine tests for `Initial`, `Draining`, `Cached`, `Poisoned`, and
  `Taken`, including reentrancy and dropped-drain poison.
- [x] Write red deadline races for first byte, inter-chunk waits, EOF, source error, and
  simultaneous readiness using the admitted clock. Expiry wins and remains sticky as
  `RequestTimeout` (408).
- [x] Write red cap tests for exact limit, first byte over, checked accounting overflow, and
  stricter re-check of already cached bytes without poisoning the cache.
- [x] Implement private `BodyCell`/`StoredError`, no borrow across await, and fallible
  `take_body`/`into_request`. Keep the total error match wildcard-free.
- [x] Migrate JSON/form extractors to the documented defaults and explicit `Within` forms.
- [x] Run `cargo test -p edgezero-core`.

## Task 5: Future Axum raw request-head and framing certification

**Files:** pinned Hyper patch/upstream hook, workspace dependency patch, Axum connection
setup/request conversion, raw-socket integration tests.

Current Axum ingress uses `IngressHeadAccounting::HostManaged` and
`IngressFraming::HostManaged`. Every item in this task is future promotion evidence; none is a
claim about the current adapter.

- [ ] Add red raw-socket tests for exact/over-limit request-target bytes, raw header bytes,
  and field-line count; checked accounting overflow; CL+TE in both orders; duplicate
  equal/unequal and comma-list CL; signed/malformed/overflow CL;
  repeated/non-final/unsupported TE; HTTP/2 TE; and valid controls.
- [ ] Assert target overflow is 414, header byte/count overflow is 431, diagnostics echo no
  request data, and every head rejection happens before resolution/admission/body polling.
  Exact limits pass and expose the exact `IngressHeadAccounting::RawValidated` totals.
- [ ] Assert rejection is 400, closes HTTP/1 or resets only the multiplexed stream, invokes
  no admission policy, and polls no body.
- [ ] Characterize pinned Hyper 1.10.1 first: preserve source references and tests proving a
  `Content-Length` after `Transfer-Encoding` is skipped and equal duplicate lengths are
  accepted. These are the red cases the patch must change.
- [ ] Add the smallest audited parser patch (or consume an upstream equivalent) in Hyper's
  existing request-line/ordered `httparse` header path. Bound parser reads so the raw head is
  rejected rather than accumulated past the installed target/header policy; count duplicate
  field lines and exact syntax bytes. In the same boundary reject any CL+TE presence in
  either order; any second or comma-list CL; and malformed, repeated/non-final `chunked`, or
  unsupported transfer codings before normalization. Keep Hyper as the only HTTP/1 parser;
  do not add an independent socket pre-parser that could disagree on pipelined boundaries.
- [ ] Pin the exact patched source/revision and add an upgrade gate that fails when the
  patch no longer applies or its source assertions change. Document the upstream issue/PR
  and removal condition.
- [ ] Only after raw acceptance, derive the normalized `IngressFraming` summary from the
  surviving headers and HTTP version and attach the raw accounting totals. Do not claim the
  summaries themselves performed parser enforcement or smuggling validation.
- [ ] Convert accepted bodies lazily, wrap native cancellation under the admitted absolute
  deadline, and dispatch with the exact resolved token.
- [ ] Run Axum unit, integration, and raw-socket suites.

## Task 6: Cloudflare, Fastly, and Spin ingress implementations

**Files:** each adapter request/service entry point and contract tests.

Axum shares the current `HostManaged` baseline described in Task 5. This task records the
equivalent baseline and target-specific deadline behavior for the other three adapters.

- [ ] Write per-adapter ordering tests with an observable first body poll.
- [ ] Stamp `request_start` from `App::monotonic_now()` at earliest guest entry, pass
  `HostManaged` framing, attach
  `IngressHeadAccounting::HostManaged`, apply only documented post-materialization
  defense-in-depth limits, invoke policy once, and preserve the resolved token through
  dispatch.
- [ ] Implement pre-read/post-ready deadline checks against the admitted app clock and the
  strongest available drop/abort primitive. Do not claim preemption around synchronous
  host calls.
- [ ] Prove buffered provider values enter as `Body::Once` only after admission and within a
  documented platform bound; all other paths use lazy `Body::Stream`.
- [ ] Run each adapter's contract suite, including its WASM target where applicable.

## Task 7: Capabilities, generators, and deployed probes

**Files:** manifest schema/parser, adapter registry, CLI checks, scaffold templates, docs.

- [ ] Add separate `ingress-admission`, `inbound-read-deadlines`,
  `raw-ingress-head-limits`, and `raw-ingress-framing-validation` cells with the exact
  initial matrix in the spec.
- [ ] Test parse/display/round-trip, unknown value rejection, and fail-closed build/serve/
  deploy/demo behavior for Native requirements.
- [ ] Update manifest-generated and hand-written app construction so both retain admission
  policy; keep low-level `into_router()` explicit about its lack of adapter admission.
- [ ] Keep Cloudflare's pre-select cooperative zero-delay yield, document the frozen-clock
  caveat, add Cloudflare and Spin deployed cancellation probes, and retain Fastly
  cooperative timing evidence. Store machine-readable target/runtime/version/tolerance
  results.
- [ ] Update adapter capability docs without upgrading a cell from local mocks alone.

## Task 8: Final verification and integration boundary

- [ ] Audit for eager request-body collection, post-materialization parser-limit claims,
  normalized-header framing claims, rematching after admission, relative timeout resets,
  wildcard `EdgeError` matches, and leaked grants.
- [ ] Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

- [ ] Record deployed-probe results separately from deterministic local tests.
- [ ] Commit ingress work independently from outbound adapter phases so each contract can be
  reviewed and reverted without changing the other.
