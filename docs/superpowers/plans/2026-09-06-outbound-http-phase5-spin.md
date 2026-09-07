# Outbound HTTP Phase 5: Spin WASI HTTP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Spin outbound HTTP with owned WASI resources, exact request/response completion, typed error classification, cooperative fairness, and a real executable SDK-resource test gate.

**Architecture:** A target-neutral state-machine driver is exercised natively; a thin Spin SDK layer supplies real WASI HTTP resources. Upload, send, request completion, response body, trailers, and caller-result handles remain owned until their specified terminal branch. One outer monotonic race covers the complete buffered exchange; streamed bodies retain the original deadline.

**Tech Stack:** Spin SDK 6.0.0 lock pin, WASI HTTP 0.3, `web-time`, `futures`, wasm32-wasip2, pinned Wasmtime runner.

---

## Preconditions

- [ ] Phases 1b-4 pass all gates.
- [ ] Re-read spec §4.4, Spin rows in §§5.2-5.5, and Spin file summary in §7.
- [ ] Keep capability support BestEffort for deadlines, flexible phase budgets, streamed-upload deadlines, and lazy response passthrough. No fake-resource test may promote these cells.

## Task Protocol

For each task: add only the named native or SDK-resource tests; run the exact native or pinned WASM command and require a nonzero failure from missing behavior; implement through the same production state machine; rerun focused and package tests; run `git diff --check`; then stage only listed files and make the stated commit. Resource ownership assertions use counters/defaulted completion writers and simultaneous-readiness scripts, never prose-only reasoning.

**Required exact test names:** `sdk_fields_preserve_duplicate_values`, `sdk_request_options_execute_all_setters`, `spin_error_code_table_is_exhaustive`, `send_all_preflight_precedence_and_indices`, `one_slot_send_all_matches_send`, `canonical_uri_wire_serialization_table`, `upload_failure_wins_simultaneous_send`, `request_done_error_wins_retained_send`, `reader_gone_never_polls_request_done`, `request_trailers_succeed_only_on_clean_eof_or_reader_gone`, `response_caller_result_succeeds_only_after_native_eof`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `raw_and_decoded_quotas_yield_before_item_65`, `streamed_tasks_consume_fast_body_before_slow_headers`, `streamed_body_races_each_pull_against_remaining_budget`, and `downstream_fallback_preserves_typed_error_envelope`.

**Expected red:** Task 0 fails at WASI import/resource execution until the runner is correct; later tests expose wrong poll priority, dropped/early-success completion handles, item 65 without Pending, unbounded streamed pull, or generic response errors. A trap caused by missing imports is a Task 0 failure, never a skipped/pass result.

**Exchange skeleton:** implement these states directly; do not replace them with an unbiased `select`.

```rust
enum ExchangeState {
    AwaitingRequestDone { request_done: RequestDone, send: StoredSend },
    ReaderGone { request_done: RequestDone, send: SendFuture },
    Uploading { pump: UploadPump, request_done: RequestDone, send: SendFuture },
}
```

`Uploading` polls one pump step first and polls send only after that step returns Pending. `AwaitingRequestDone` polls completion first while retaining ready send. `ReaderGone` never polls completion, waits for send, then drops completion before response conversion.

### Task 0: Prove the SDK-resource runner or STOP

**Files:**
- Create: `crates/edgezero-adapter-spin/tests/sdk_resources.rs`
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Modify after proof: `crates/edgezero-adapter-spin/.cargo/config.toml`, `.tool-versions`, `.github/workflows/test.yml`

- [ ] Add the independent `test-utils = []` feature without enabling `spin`; Task 1 adds the driver dependencies.
- [ ] Add nonzero WASM tests that construct real `Fields`, append duplicate fields, construct `RequestOptions`, call all three timeout setters, construct `Request`, and exercise request/response completion resources without invoking an external origin.
- [ ] Start with the pinned Wasmtime 44.0.1 candidate:

```sh
CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' \
  cargo test --offline --locked -p edgezero-adapter-spin --no-default-features \
  --features spin,test-utils --target wasm32-wasip2 --test sdk_resources
```

- [ ] Record the exact runtime version, flags, Rust target/component setup, test count, and passing output. Put the verified runner in the crate-local `.cargo/config.toml`; CI must use the same value. `-S http=y -S p3=y` is necessary evidence, not assumed sufficient evidence.
- [ ] If the pinned runtime cannot execute these resources, STOP. Select and review a compatible exact runtime pin before changing `.tool-versions` and CI together. Compilation, native fakes, and zero-test success do not pass this gate.
- [ ] Commit after proof: `test(spin): execute WASI HTTP SDK resources`.

### Task 1: Establish the native driver seam and error classifier

**Files:**
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Create: `crates/edgezero-adapter-spin/src/outbound.rs` beside temporary `src/proxy.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`
- Modify: `Cargo.lock`, `examples/app-demo/Cargo.lock`

- [ ] Add direct `web-time`; keep `test-utils` and `spin` independent.
- [ ] Remove the whole-file WASM gate from `tests/contract.rs`; create native and SDK modules. Confirm the native command executes a nonzero deliberately failing test.
- [ ] Add table tests for all 39 pinned `ErrorCode` variants. Timeout variants map to attributed 504; caller request policy/size to 400; local invariants to 500; transport/protocol to typed 502; host internal to unspecified 502.
- [ ] Test the same classifier through `client::send`, `request_done`, and response completion paths. Non-2xx HTTP statuses remain successful responses.
- [ ] Implement target-neutral traits/resources and the exhaustive classifier with no wildcard for the pinned exhaustive enum.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`; expect success.
- [ ] Commit: `feat(spin): add outbound driver and error classifier`.

### Task 2: Implement preflight, request conversion, and batch execution

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing tests for empty batch, complete preflight, streamed request/response slot rejection, stable indices, partial errors, one-slot `send_all`/`send` equivalence, one `batch_now`, concurrent eligible exchange progress, and sibling isolation. Include GET/HEAD body errors beating batch-only errors and no polls of rejected source streams.
- [ ] Add conversion tests for methods, repeated fields via append, request options order, nonzero nanosecond rounding, and all setter outcomes (`NotSupported`, `Immutable`, `Other`). Capture the final WASI scheme/authority/path-with-query and assert exact core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and queries.
- [ ] Run native contracts; expect failure.
- [ ] Implement `SpinOutboundClient::{send, send_all}` for contract tests. Retain method, mode, and all policy fields before consuming request parts; production injection switches only after Task 5 is green.
- [ ] Treat `RequestOptionsError::NotSupported` as logged BestEffort degradation while retaining the outer deadline race; `Immutable`/`Other` are internal setup failures.
- [ ] Use `join_all` for eligible complete batch slots after full preflight.
- [ ] Rerun native and SDK-resource tests; expect success.
- [ ] Commit: `feat(spin): dispatch outbound requests`.

### Task 3: Implement the biased upload/send state machine

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing transition tests for `Uploading`, `AwaitingRequestDone`, and `ReaderGone`; source/cap/deadline/write/flush failures; clean EOF; reader-gone; early response; and future/resource drop counts.
- [ ] Add simultaneous-readiness tests: upload failure beats send; request completion beats retained send; send is observed only after an upload step returns Pending.
- [ ] Prove request trailers write `Ok(None)` only for clean EOF or reader-gone and retain the default failure otherwise.
- [ ] Implement the state machine exactly as §4.4 specifies. Never poll `request_done` in `ReaderGone`; never lose an already-ready send result while awaiting request completion.
- [ ] Yield once after every accepted upload chunk and recheck the absolute deadline before accepting every terminal or data result.
- [ ] Rerun native contracts; expect success.
- [ ] Commit: `feat(spin): own outbound upload completion`.

### Task 4: Implement response completion, decoding, and fairness

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Delete: `crates/edgezero-adapter-spin/src/decompress.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing cases for visible header limits, repeated request fields, repeated response `Set-Cookie`, raw malformed response nomination/encoding, 3xx returned without a follow-up request, HEAD/1xx/204/304, every 205 branch, encoded/Brotli/decoded limits, gzip members, Brotli trailing data, typed source/completion errors, and timeout precedence.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Assert attributed 504, no early caller-result success, and preservation of a late typed source/completion error when it wins before expiry.
- [ ] Add the joined send-plus-immediate-body-consumption regression used by Axum/Cloudflare; a fast body must complete while sibling headers remain pending.
- [ ] Add caller-result tests proving the default is failure and `Ok(())` is written only after native EOF, trailers, decoder completion, caps, and deadlines succeed.
- [ ] Add continuously-ready raw and decoded streams. Freeze separate quotas at 64; empty items count; state survives polls/`next()`; item 65 is preceded by a cooperative Pending.
- [ ] Replace adapter-local buffered decompression with core's shared streaming pipeline. Remove production `brotli`/`flate2` only after no production callsite remains.
- [ ] For streamed mode, retain body/trailer/caller-result ownership in `Body::Stream`; late errors must surface as error items, never false EOF. Add pre-pull, timer-ready, item-ready, post-ready, and terminal EOF/error tests for the streamed-only monotonic race.
- [ ] Rerun native and SDK tests; expect success.
- [ ] Commit: `feat(spin): complete outbound response streams`.

### Task 5: Add the outer deadline race and downstream fallback

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Delete: `crates/edgezero-adapter-spin/src/proxy.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/response.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing tests for send/upload/buffered-drain timeout, simultaneous exchange/timer readiness, post-ready absolute checks, preservation of all four `BudgetSource` values, and the streamed continuation using only `budget.deadline.remaining()` after headers.
- [ ] Race the whole buffered exchange against `spin_sdk::time::sleep(remaining)`. Poll exchange first but check absolute expiry in both branches so expiry wins simultaneous results.
- [ ] For Streamed mode only, read remaining time after header exchange. If expired, drop native response resources and return attributed 504; otherwise install a fresh per-chunk sleep race bounded by that remaining duration while retaining the original absolute deadline for post-ready checks. Do not add a second race to Buffered mode.
- [ ] Rename/freeze `SPIN_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB`. Test exact/over limit and source failure; convert the original typed error envelope before platform headers are committed.
- [ ] Switch request extensions to `HttpClient`, delete the legacy module, and leave no module/type alias.
- [ ] Rerun native contracts and WASM checks; expect success.
- [ ] Commit: `feat(spin): enforce outbound exchange budgets`.

### Task 6: Land CI execution gates

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Modify if validated in Task 0: `.tool-versions`

- [ ] Add the native contract command.
- [ ] Add the exact validated SDK-resource and WASM contract runner commands, with `spin,test-utils`, and assert nonzero tests.
- [ ] Keep ordinary Spin WASM compile checks.
- [ ] Add table-driven capability tests and publish Spin's override only now: Native for outbound HTTP, header fidelity, and slot isolation; BestEffort for deadlines, flexible phase budget, streamed-upload deadlines, and lazy response passthrough; future capabilities -> Unsupported.
- [ ] Commit: `ci: execute Spin outbound contracts`.

## Phase Verification

- [ ] `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`
- [ ] `cargo check --offline --locked -p edgezero-adapter-spin --no-default-features --features spin --target wasm32-wasip2`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: Spin's deterministic contract and real SDK bindings execute in CI, and its conservative capability row lands with them. Host cancellation remains documented BestEffort until a separate live characterization proves a finite bound.
