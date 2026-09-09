# Response Egress Implementation Plan

> **Status:** Planned follow-up. Source of truth:
> [response-egress design](../specs/2026-09-08-response-egress-design.md).

**Goal:** Make client-response conversion and delivery observable and deadline-bounded where
platform APIs permit, with backpressure, native abort, and one terminal report.

**Architecture:** Core defines policy, metadata, terminal outcome/report, and a non-clone
completion guard. Adapter services retain request start/route metadata across router dispatch,
start one guarded egress attempt, and hand it to response converters. Streaming converters
poll under platform demand and one absolute timer. Buffered-only hosts enforce a converter
cap but remain Unsupported for delivery guarantees they cannot observe.

**Dependency:** Stable route metadata/request-start reuse depends on Tasks 1-3 of the
[inbound ingress plan](2026-09-08-inbound-ingress-admission.md). Response-egress work may be
developed earlier with `route: None`, but no adapter claims Native completion until the
service handoff preserves the original request metadata.

## Global Constraints

- This work does not alter outbound request/response deadlines.
- Do not add Tokio to core or shared adapter crates.
- No terminal path may call the observer twice.
- Do not reset the deadline per chunk, flush, or platform call.
- Do not rewrite status or append an error body after the adapter's response commit point.
- Do not claim client completion from response-object construction.
- Apply TDD per task; record the red failure before production edits.

## Task 1: Core policy, report, and completion guard

**Files:** new `crates/edgezero-core/src/response_egress.rs`, `lib.rs`, app configuration.

- [ ] Write red tests for the 30-second default, immediate expiry, checked
  `egress_started_at + DEADLINE_FAR_FUTURE` clamping, checked-add overflow failure, all
  `ResponseEgressOutcome` variants, report accessors, and observer object safety.
- [ ] Write red state tests for `Initial -> Writing -> Completed`, every failure from both
  nonterminal states, terminal-signal races, guard drop, disarm, and exactly one callback.
- [ ] Write red accounting tests for zero/exact bytes and checked `u64` overflow. Inject a
  backwards clock and assert zero elapsed plus `Unspecified`, one notification, and a log.
- [ ] Add `ResponseEgressPolicy`, body-blind immutable head accessors, outcome/report,
  observer handle,
  and crate-private completion guard. Keep observer payload bounded and body/header-free.
- [ ] Run `cargo test -p edgezero-core --lib response_egress`.

## Task 2: Preserve request metadata through dispatch

**Files:** core router/service result, adapter request services, context tests.

- [ ] Write red tests proving response policy sees the ingress-captured request start and
  canonical registered route pattern, never a dynamic path or a newly sampled start.
- [ ] Add an internal dispatch envelope carrying `Response`, request start, optional route
  metadata, and the app's policy/observer handles to the adapter converter boundary.
- [ ] Keep public handler return types unchanged; the envelope is service plumbing, not an
  application response type.
- [ ] Ensure 404/405 and converted `EdgeError` responses also create one egress attempt.
- [ ] Run core router/service tests.

## Task 3: Shared converter contract and capability cells

**Files:** `edgezero-adapter` registry/contracts, core manifest, CLI enforcement, docs.

- [ ] Write red parse/display/round-trip tests for `response-egress-abort`,
  `response-egress-backpressure`, `response-egress-completion`, and
  `response-write-deadlines`.
- [ ] Write red build/serve/deploy/demo tests that fail closed when an app requires Native
  and the selected target advertises BestEffort/Unsupported.
- [ ] Add the exact initial matrix from the spec. Keep cells separate so one target can
  expose pull backpressure without claiming observable finish.
- [ ] Define adapter contract fixtures for source, sink, timer, commit, abort, disconnect,
  and completion observation without introducing a runtime into core.
- [ ] Run manifest, registry, and CLI capability tests.

## Task 4: Axum streaming egress

**Files:** `edgezero-adapter-axum/src/response.rs`, service wiring, integration tests.

- [ ] Write red body-wrapper tests: no second source poll while one chunk is pending;
  independent timer wake; first-byte/inter-chunk/finish deadline; exact byte count; source
  error; sink error; body drop; normal EOF; and competing drop/deadline notification.
- [ ] Write red raw-socket tests for a client that does not read, disconnects before body,
  disconnects mid-body, and consumes normally. Assert reset/close and exactly one report.
- [ ] Replace `block_on` collection with a Hyper-compatible body wrapper. Retain the guard in
  the body until EOF/drop, and race the source against an adapter-owned Tokio sleep created
  from the same absolute deadline.
- [ ] Define and test the header commit point. Conversion/deadline failure before commit may
  use the minimal fallback response; after commit it emits a body error/resets instead.
- [ ] Mark Axum Native only after all local integration tests pass.
- [ ] Run `cargo test -p edgezero-adapter-axum --all-targets`.

## Task 5: Cloudflare stream wrapper and deployed probe

**Files:** Cloudflare response converter, WASM tests/harness, deployed probe script/docs.

- [ ] Write red wrapper tests for pull-driven source polling, cancel, source error, absolute
  deadline, exact byte count, and exactly-once finish/drop behavior.
- [ ] Add a frozen-clock fixture where the source continuously returns ready chunks. Assert
  the wrapper cooperatively yields and its independent Worker timer can win.
- [ ] Replace simple stream mapping with a wrapper owning source, timer, cancellation, and
  completion guard. Recheck deadline after every ready source/platform result.
- [ ] Add a deployed probe covering slow client/backpressure, first-byte and midstream
  timeout, explicit disconnect, normal finish, elapsed tolerance, and runtime version.
- [ ] Keep all four cells BestEffort until deployed evidence demonstrates the corresponding
  host behavior; a local WASM mock cannot upgrade them.
- [ ] Run Cloudflare unit/contract/WASM checks and archive the probe artifact.

## Task 6: Fastly and Spin bounded converter fallback

**Files:** Fastly/Spin response converters and tests, capability docs.

- [ ] Write red exact-cap and one-byte-over tests for both `Body::Once` and streamed bodies.
  The same finite collection cap applies before conversion; checked accounting prevents
  overflow.
- [ ] Write red tests for source errors and pre-return deadline checks. Assert reports use
  `ConversionError`, `SourceError`, or `DeadlineExceeded` as appropriate and fire once.
- [ ] Refactor collection into shared per-adapter helpers with one absolute converter
  deadline. Drop partial buffers/source on failure.
- [ ] Keep response delivery/abort/backpressure/completion cells Unsupported. Document that
  successful host response construction emits `HostHandoff` exactly once, never `Completed`;
  this preserves observer cardinality without claiming an unobservable client finish.
- [ ] Run Fastly and Spin unit/contract suites plus Spin's WASM build.

## Task 7: Cross-adapter lifecycle tests and documentation

- [ ] Run the shared matrix for empty, buffered, streaming, source failure, transport failure,
  deadline, disconnect, converter failure, and terminal races on every supporting target.
- [ ] Verify reports never contain body bytes, header values, dynamic paths, or raw source
  error strings.
- [ ] Update adapter guides with commit points, byte-count boundary, abort primitive,
  provider-buffer exclusions, and capability/evidence links.
- [ ] Audit all response converters with `rg` for `block_on`, unbounded `Vec` growth, stream
  mapping without cancel/drop observation, deadline resets, and variant-only completion.

## Task 8: Final verification

- [ ] Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

- [ ] Run Axum raw-socket and Cloudflare deployed timing probes separately; record versions
  and tolerances.
- [ ] Re-read the source spec and account for every normative statement before marking the
  plan complete.
