# Outbound HTTP Phase 6: Fastly Dispatch and Harvest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Fastly outbound HTTP with dispatch-all-before-wait fan-out, deterministic dynamic backends, host timer derivation, typed errors, streamed upload ownership, and bounded downstream conversion.

**Architecture:** A target-neutral synchronous engine owns slots, backend identity/cache behavior, timing arithmetic, and cleanup. The SDK layer supplies `PendingRequest`, backend registration, and body handles. Batch dispatch is a distinct first phase; ordered harvest may opportunistically poll later slots but never loses a result.

**Tech Stack:** Fastly SDK 0.12.1, `web-time`, `sha2`, core typed streams/decoders, native `test-utils`, wasm32-wasip1/Viceroy.

---

## Preconditions

- [ ] Phases 1b-5 pass all gates.
- [ ] Re-read spec §4.3, Fastly rows in §§5.2-5.5, and the Fastly file summary in §7.
- [ ] Keep deadlines, flexible phase budgets, slot isolation, streamed-upload deadlines, and lazy response passthrough at the exact reviewed BestEffort levels.
- [ ] Confirm `OutboundRequest::host_name()` exists from Phase 1b; never reparse the URI to construct backend identity.

## Task Protocol

For each task: add only the named native or SDK-gated cases; run the exact native/WASM command and require a nonzero failure; implement through the shared production engine; rerun focused and package tests; run `git diff --check`; then stage only listed files and make the stated commit. Fake handles establish stage order and cleanup, not real host cancellation or a finite write bound.

**Required exact test names:** `backend_identity_uses_every_canonical_property`, `backend_creation_error_table_is_exhaustive`, `backend_builder_disables_cross_session_pooling`, `dispatch_guard_checks_expiry_before_slack`, `send_all_dispatches_every_slot_before_wait`, `one_slot_send_all_matches_send`, `canonical_uri_wire_serialization_table`, `send_all_reverse_completion_preserves_order`, `done_error_survives_poll_sweep`, `send_error_cause_table_preserves_timeout_source`, `response_read_checks_deadline_after_eof`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `streamed_upload_finishes_exactly_once`, `streamed_upload_failure_never_waits`, and `downstream_fallback_preserves_typed_error_envelope`.

**Expected red:** the old client waits during dispatch, backend names/settings collide, pooling remains enabled, error variants map to generic 500, an EOF read escapes deadline checks, or failed uploads finish/wait. SDK-gated tests must fail assertions, not report zero tests.

**Engine skeleton:**

```rust
enum Slot {
    Done(Result<OutboundResponse, EdgeError>),
    Pending(PendingSlot),
    Taken,
}

struct BackendIdentity {
    budget_ms: u32,
    host: String,
    port: u16,
    scheme: Scheme,
    tls_mode: TlsMode,
}
```

Phase one converts every valid slot to `Pending` or `Done`; no wait occurs. Phase two waits in input order, polls later pending slots opportunistically into `Done`, and replaces an unresolved `Taken` with internal error rather than panicking.

### Task 1: Establish the engine and executable seams

**Files:**
- Modify: `crates/edgezero-adapter-fastly/Cargo.toml`
- Create: `crates/edgezero-adapter-fastly/src/outbound.rs` beside temporary `src/proxy.rs`
- Create: `crates/edgezero-adapter-fastly/src/outbound/{backend,engine,sdk,test_utils}.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `Cargo.lock`, `examples/app-demo/Cargo.lock`

- [ ] Add direct `web-time` and independent `test-utils = []`; do not enable Fastly through test-utils.
- [ ] Move whole-file WASM gating to the SDK test module. Add a native deliberately failing smoke test and prove the native command executes it.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`; expect a nonzero test count and failure at the smoke assertion, not at linking/imports.
- [ ] Define injectable clock, dispatch-stage callback, backend registrar, synchronous pending/body handles, and hash function. Public retained callbacks must remain `Send + Sync`.
- [ ] Production and tests must call the same generic engine; test code may supply effects, not copy algorithms.
- [ ] Replace the sentinel with a passing seam-construction test and rerun the same native command; require a nonzero test count and success before committing.
- [ ] Commit: `test(fastly): add outbound engine seams`.

### Task 2: Implement backend identity and host timers

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/backend.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing tests for DNS/IP/IPv6/plain/TLS identities, default/explicit ports, exact ceil-to-ms budgets, SHA-256 first-128-bit names, dedup, forced hash collision, externally occupied `NameInUse`, disabled cross-session connection pooling, and concurrent lookup.
- [ ] Add timer tests: for total >= 4 ms, connect = total/4, first byte = remainder, between = total; below 4 ms, connect/first each equal total.
- [ ] Add an exhaustive no-wildcard `BackendCreationError` test table: `Disallowed` -> actionable unspecified 502; `NameInUse` -> cache/collision protocol; all three `*TimeoutTooLarge`, `NameTooLong`, and `EncodingError` -> internal 500; `HostError` -> unspecified 502.
- [ ] Add guard-order tests immediately before dispatch: absolute expiry -> attributed 504; live-deadline setup over `BATCH_DISPATCH_SLACK_MAX` -> internal 500; no deadline uses the default budget without the live-deadline slack failure.
- [ ] Run native contracts; expect failure.
- [ ] Implement `BackendIdentity(scheme, host_name, resolved_port, tls_mode, budget_ms)` and `ez_` names. Cache name -> `(identity, backend)` behind `Mutex`; never hold the lock across `finish()`. Call `.enable_pooling(false)` because the SDK defaults to cross-session pooling, which is distinct from session-scoped registration and would weaken request-budget isolation.
- [ ] A cache miss followed by `NameInUse` fails closed. Do not recover with string matching or `Backend::from_name`.
- [ ] Implement the complete creation-error mapping separately from `SendErrorCause`; DNS/TLS/connect failures belong only to the send-stage table.
- [ ] Configure SNI only from `sni_hostname`; certificate checking from `cert_host`, including HTTPS IP literals.
- [ ] Rerun native contracts; expect success.
- [ ] Commit: `feat(fastly): configure deterministic outbound backends`.

### Task 3: Implement two-phase send_all

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing cases for empty batch, complete preflight and index alignment, every valid dispatch before first wait, one-slot `send_all`/`send` equivalence, reverse completion with stable order, partial dispatch/wait/body errors, preservation of pre-existing `Done` errors during poll sweeps, shared `batch_now`, and unresolved-slot internal error. Include GET/HEAD body errors beating batch-only errors and no polls of rejected source streams.
- [ ] Capture the exact SDK request target and assert it matches core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and queries; Fastly may derive backend identity from accessors but must not reconstruct the request URI.
- [ ] Add serial-harvest cases showing a later slot can expire while waiting behind an earlier slot without cancellation or Native slot-isolation claims.
- [ ] Run native contracts; expect failure.
- [ ] Implement `Slot::{Done, Pending, Taken}`. Dispatch every valid slot sequentially; then wait in input order and opportunistically poll later pending slots.
- [ ] Preserve all policy/method/mode fields in `PendingSlot`. Resolve every output index without panic. Do not use SDK `select()`, which cannot preserve stable slot identity.
- [ ] Harvest every successfully dispatched buffered batch request even when siblings fail.
- [ ] Rerun native contracts; expect success.
- [ ] Commit: `feat(fastly): dispatch fanout before ordered harvest`.

### Task 4: Map SDK errors and process responses

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add SDK-gated construction tests for every known public `SendErrorCause` listed in §4.3, with a mandatory future `_` classifier arm. Test timeout variants under all four `BudgetSource` values.
- [ ] Add native response cases for visible headers, repeated request fields, repeated response `Set-Cookie`, raw malformed response nomination/encoding, 3xx returned without a follow-up request, bodyless/205, encoded/Brotli/decoded limits, encoding policy, gzip members/native EOF, errors, deadlines before/after each blocking read including EOF, and drop-on-early-termination.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Assert attributed 504 after the blocking read returns, no decoder-end success before native EOF, and late typed source/completion preservation when observed in budget.
- [ ] Run native and Fastly WASM library/contract tests; expect failures.
- [ ] Map timeout -> attributed 504; transport/protocol -> typed 502; local invariant/platform internal -> 500; unspecified known/future -> unspecified 502 as specified.
- [ ] Apply the core response pipeline in order. Drop the owned native body on cap/decode/deadline/consumer termination; do not drain after failure and do not claim finite origin cancellation.
- [ ] Rerun native/WASM tests; expect success.
- [ ] Commit: `feat(fastly): classify and harvest outbound responses`.

### Task 5: Implement streamed uploads

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing tests for source/cap/pre-read/post-read/write/flush failures, future drop, clean EOF, `finish` exactly once, finish failure, post-finish expiry, and no wait after failed finish.
- [ ] Retain writer and pending handles together. Every failure drops both without `finish`/`wait`; clean EOF finishes once and only then permits wait.
- [ ] Document that source pulls and host writes can stall beyond the cooperative check and that partial bytes may already be delivered.
- [ ] Rerun native/WASM tests; expect success.
- [ ] Commit: `feat(fastly): own streamed outbound uploads`.

### Task 6: Migrate request injection and downstream conversion

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Delete: `crates/edgezero-adapter-fastly/src/proxy.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Inject one `HttpClient::with_client(FastlyOutboundClient::new())` per Fastly session so the backend cache lifetime mirrors the SDK namespace.
- [ ] Delete the legacy module during this switch and leave no module/type alias.
- [ ] Add exact/over `FASTLY_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` tests and typed stream-error envelope tests.
- [ ] Implement bounded fallback before platform headers are committed. Preserve original status/kind on failure.
- [ ] Rerun adapter and workspace tests; expect success.
- [ ] Commit: `feat(fastly): wire outbound client and response fallback`.

### Task 7: Land CI gates

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`

- [ ] Add the native contract command and enable `test-utils` in Fastly WASM contract/library runs.
- [ ] Confirm each command executes nonzero tests; retain production target checks.
- [ ] Add table-driven capability tests and publish Fastly's override only now: Native for outbound HTTP and header fidelity; BestEffort for deadlines, flexible phase budget, slot isolation, streamed-upload deadlines, and lazy response passthrough; future capabilities -> Unsupported.
- [ ] Commit: `ci: execute Fastly outbound contracts`.

## Phase Verification

- [ ] `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --lib`
- [ ] `cargo check --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly --target wasm32-wasip1`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: Fastly dispatches all eligible fan-out slots before harvest, preserves typed per-slot outcomes, and documents rather than overclaims its unavoidable host-blocking gaps.
