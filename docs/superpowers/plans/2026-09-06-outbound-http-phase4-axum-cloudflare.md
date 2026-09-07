# Outbound HTTP Phase 4: Axum and Cloudflare Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the full outbound contract for Axum and Cloudflare, including batch behavior, typed cleanup, adapter scheduling, and executable native/WASM/host contracts.

**Architecture:** Both clients consume the same core request and response pipeline. Axum uses Reqwest and bounded conversion at one Tokio blocking boundary. Cloudflare uses a target-neutral orchestration driver plus a small Worker WASM bridge that owns abort, timer, native body, and host-event yield resources through terminal completion.

**Tech Stack:** Reqwest 0.13.4, Axum/Tokio, Worker 0.8.3, Web APIs, `web-time`, `futures`, wasm-bindgen-test, workerd/deployed probes.

---

## Preconditions and Owned Files

- [ ] Phases 1b-3 pass all gates.
- [ ] Re-read spec §§3.1-3.5, 4.1, 4.2, 5.2-5.5, and the Axum/Cloudflare rows in §7.
- [ ] Change the root and `examples/app-demo` workspace Worker requirements to exact `=0.8.3`, then refresh both lockfiles. The raw bridge is audited against 0.8.3; do not silently upgrade SDK source during this phase.
- [ ] Add `web-time` and independent `test-utils = []` to both adapter manifests. `test-utils` must not enable a runtime feature.
- [ ] Modify only Axum/Cloudflare adapters, root `Cargo.toml`/`Cargo.lock`, `examples/app-demo/Cargo.toml`/`Cargo.lock`, their host fixtures, and `.github/workflows/test.yml`. No core semantics or Phase 7 templates/docs.

## Task Protocol

For each adapter task: add only the named contract cases; run its exact Task 1 command and require nonzero failing tests; implement through the production driver; rerun to zero failures; run that adapter's target check and `git diff --check`; then stage only listed files and make the stated commit. Mock transport proves orchestration only. Any cancellation, raw-wire, or host-yield claim marked host-observed requires the workerd/deployed fixture before its capability cell is published.

**Required exact test names:** `send_all_preflight_precedence_and_indices`, `one_slot_send_all_matches_send`, `send_all_starts_every_eligible_exchange`, `canonical_uri_wire_serialization_table`, `redirect_response_is_not_followed`, `streamed_tasks_consume_fast_body_before_slow_headers`, `request_deadline_checks_before_and_after_source_ready`, `response_pipeline_preserves_typed_limits`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `axum_response_conversion_keeps_reactor_live`, `cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect`, `cloudflare_frozen_clock_forces_host_yield`, `cloudflare_205_reads_at_most_once`, `cloudflare_non_ascii_request_header_returns_error_not_panic`, and `cloudflare_abort_guard_fires_exactly_once`.

**Expected red:** contract smoke tests first fail their sentinel assertion; behavior tests then expose serial starts, followed redirects, timeout/cap misclassification, reactor starvation, transformed fetch bytes, absent abort, extra 205 reads, or panic. Host-yield proof fails by timer starvation, not by a fabricated clock advance.

### Task 1: Establish executable contract seams

**Files:**
- Modify: `crates/edgezero-adapter-{axum,cloudflare}/Cargo.toml`
- Create: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify/Create: both adapters' `src/test_utils.rs`
- Modify: `.github/workflows/test.yml`

- [ ] Add one deliberately failing smoke test to each native contract module.
- [ ] Run:
  - `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
  - `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] Confirm each command executes a nonzero test count and fails at the assertion, not at linking/imports.
- [ ] Replace smoke failures with feature-gated seams for clocks, timers, transport/body handles, abort events, host yields, and stage delays. Production and fake transports must call the same driver.
- [ ] Commit: `test(adapters): add outbound contract seams`.

### Task 2: Implement Axum dispatch and batch behavior

**Files:**
- Create: `crates/edgezero-adapter-axum/src/outbound.rs` beside the temporary legacy `src/proxy.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`

- [ ] Add failing tests for complete batch preflight, empty batch, index alignment, partial failure, one shared `batch_now`, all valid slots started before completion, duplicate request headers, methods, canonical target use, and a 302 response returned without following its `Location`. A valid one-slot buffered `send_all` must match `send` preflight and result semantics. Include GET/HEAD body invalidity beating batch-only stream-mode errors and prove a rejected body source is never polled.
- [ ] In the production Reqwest request captured by the seam, assert exact core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and query preservation. The adapter must never rebuild these values from URL components.
- [ ] Add upload tests for exact/over request cap, source error, stalled pull, timeout before/after readiness, and no further polling after failure.
- [ ] Run the Axum contract command; expect failures.
- [ ] Implement `AxumOutboundClient::{send, send_all}`. Configure Reqwest with `redirect(Policy::none())`, disable automatic content decoding, and remove the client-wide 30-second timeout.
- [ ] Apply `RequestBuilder::timeout(remaining)` immediately before `send`. Race every streamed upload pull against the same absolute budget and buffer only within `max_request_body_bytes`.
- [ ] Use `join_all` after preflight, preserving slot order and independent results.
- [ ] Compile/export the new module for contract tests, but leave production request injection on the legacy client until Task 3 is green.
- [ ] Rerun the contract command; expect success.
- [ ] Commit: `feat(axum): implement outbound dispatch`.

### Task 3: Implement Axum response pipeline and conversion schedule

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Delete: `crates/edgezero-adapter-axum/src/proxy.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/request.rs`
- Modify: `crates/edgezero-adapter-axum/src/response.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`

- [ ] Add loopback-origin tests for repeated headers including multiple `Set-Cookie` values, raw malformed nomination/encoding lines, all content-encoding policy rows, header/encoded/decoded/Brotli limits, gzip members/native EOF, bodyless/205 cases, timeout races, cancellation on drop/error/cap, and non-2xx success.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Every case must return the attributed 504, retain typed late source/completion errors, and keep cleanup armed until native EOF.
- [ ] Add the streamed fan-out regression: join tasks that each perform `send` and immediately consume the body; the fast body must finish before a sibling's delayed headers. A control that joins header-only sends then delays consumption must fail.
- [ ] Add exact and one-byte-over `AXUM_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` conversion cases, preserving the original `EdgeError` status/kind.
- [ ] Add a reactor-progress regression proving no nested runtime/blocking deadlock.
- [ ] Run the Axum contract command; expect failures.
- [ ] Feed Reqwest `Response::chunk()` lazily through the core pipeline; retain method/mode/all policy fields before consuming request parts.
- [ ] Make response conversion async. Await routing and conversion inside exactly one `block_in_place(|| Handle::block_on(async { ... }))` boundary.
- [ ] On conversion failure, emit `EdgeError::into_response()`; never replace typed 502/504 errors with a generic 500.
- [ ] Switch request extensions to `HttpClient`, remove the adapter's legacy module, and do not leave a module/type alias. Do not change inbound buffering except compile-required typed stream conversion.
- [ ] Rerun Axum contract and crate tests; expect success.
- [ ] Commit: `feat(axum): enforce outbound response contracts`.

### Task 4: Prove a Cloudflare host-event yield primitive

**Files:**
- Create/Modify: `crates/edgezero-adapter-cloudflare/tests/host/outbound.mjs`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/wrangler.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/src/lib.rs`
- Modify: adapter `package.json` and lockfile

- [ ] Build a deployed timing probe with a frozen guest clock and a continuously-ready stream. Compare candidate host-event yields; microtask/self-wake/immediately-ready futures are invalid.
- [ ] Select and document one primitive that demonstrably lets a real timer/abort event run, and freeze one quota in `1..=64`.
- [ ] If no candidate passes on the pinned Worker runtime, STOP Phase 4 and downgrade/re-review the Cloudflare deadline capability before implementation.
- [ ] Commit: `test(cloudflare): characterize outbound host yielding`.

### Task 5: Implement the Cloudflare target-neutral driver

**Files:**
- Create: `crates/edgezero-adapter-cloudflare/src/outbound.rs` beside the temporary legacy `src/proxy.rs`
- Create: `crates/edgezero-adapter-cloudflare/src/outbound/worker.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [ ] Add failing native tests for full batch preflight/order/isolation, one-slot `send_all`/`send` equivalence, shared `batch_now`, upload caps/errors/timeouts, send/body timeout races, encoded/decoded fairness, frozen-clock terminal decisions, abort ownership, every early-return/drop path, and 3xx visibility without redirect dispatch. Include GET/HEAD body errors beating batch-only errors and no rejected-source polls.
- [ ] Capture the final raw fetch URL and assert exact core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and queries; no bridge-side URL reconstruction is allowed.
- [ ] Run the Cloudflare native contract command; expect failure.
- [ ] Implement one driver parameterized by `Clock`, `Timer`, `HostYield`, `RawFetch`, `NativeBody`, and `AbortHandle`. Count ready items including empty chunks; preserve quota state across polls and reset only after the selected host event completes.
- [ ] Keep the owning abort guard armed through decoder completion and native EOF. Disarm only on full success.
- [ ] Implement concurrent eligible `send_all` exchanges with complete preflight and stable indices.
- [ ] Compile/export the new module for contracts, but leave production request injection on the legacy client until Task 6 is green.
- [ ] Rerun native contract tests; expect success.
- [ ] Commit: `feat(cloudflare): add outbound exchange driver`.

### Task 6: Add Worker raw fetch and response bridges

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound/worker.rs`
- Delete: `crates/edgezero-adapter-cloudflare/src/proxy.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [ ] Add WASM tests that inspect final fetch options for `signal`, `encodeResponseBody: "manual"`, and `RequestRedirect::Manual`; preserve request header list semantics and exercise SDK response conversion.
- [ ] Add response tests for duplicate visible headers including repeated `Set-Cookie`, encoded passthrough with downstream `EncodeBody::Manual`, decode modes, every limit, and null/non-null 205 branches.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Assert attributed 504, no 502/500 degradation, late typed source/completion preservation, and exactly-once abort ownership.
- [ ] Add the same joined send-plus-immediate-body-consumption regression as Axum. For headers, exercise request list semantics, non-ASCII request rejection instead of panic, and Cloudflare's visible normalized response-string baseline without claiming unavailable raw octets.
- [ ] For non-null 205, prove exactly one read: EOF succeeds; any empty/nonempty item aborts without a second read. Positive visible length aborts immediately.
- [ ] Run the WASM contract command; expect failure before implementation and nonzero success afterward.
- [ ] Implement lazy `Body::Stream` output and the raw JS/Web fetch bridge. Do not add direct Web dependencies unless the pinned Worker re-exports prove insufficient.
- [ ] Switch request extensions to `HttpClient`, delete the legacy module, and leave no module/type alias.
- [ ] Remove production `brotli`/`flate2` only after no production callsite remains.
- [ ] Commit: `feat(cloudflare): bridge raw outbound fetches`.

### Task 7: Land host evidence and CI gates

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: Cloudflare host fixtures/package scripts

- [ ] Run workerd and deployed-origin tests for abort on send/body timeout, early consumer drop, cap/decode failure, raw encoding, 205, and selected host-yield timing.
- [ ] Record runtime/tool versions and observed bounds in fixture output or adjacent README comments. Mocks do not satisfy this gate.
- [ ] Add native Axum/Cloudflare commands and WASM `test-utils` activation to CI. Ensure no command reports zero tests.
- [ ] Add table-driven capability tests and publish the overrides only now. Axum: Native for HTTP, header fidelity, deadlines, flexible phase budget, slot isolation, and upload deadlines; BestEffort for lazy response passthrough. Cloudflare: Native for HTTP, deadlines, flexible phase budget, slot isolation, upload deadlines, and lazy response passthrough; BestEffort for header fidelity. Both wildcard future capabilities to Unsupported.
- [ ] Commit: `ci: enforce axum and cloudflare outbound contracts`.

## Phase Verification

- [ ] `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
- [ ] `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract`
- [ ] `cargo check --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare --target wasm32-unknown-unknown`
- [ ] `npm ci --prefix crates/edgezero-adapter-cloudflare`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run test:workerd`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run test:deployed-timing`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: Axum and Cloudflare no longer expose adapter `proxy` modules, publish their reviewed capability rows atomically with passing contracts, and Cloudflare's Native timing claim is backed by host-observed evidence.
