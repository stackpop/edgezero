# Outbound HTTP Phase 5: Spin WASI HTTP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Spin outbound HTTP with owned WASI resources, exact request/response completion, typed error classification, cooperative fairness, and a real executable SDK-resource test gate.

**Architecture:** A target-neutral state-machine driver is exercised natively; a thin Spin SDK layer supplies real WASI HTTP resources. Upload, send, request completion, response body, trailers, and caller-result handles remain owned until their specified terminal branch. One outer monotonic race covers the complete buffered exchange; streamed bodies retain the original deadline.

**Tech Stack:** exact manifest-pinned Spin SDK 6.0.0, WASI HTTP 0.3, `web-time`, `futures`, wasm32-wasip2, validated Wasmtime runner.

> **Readiness:** Only Task 0 is executable. Tasks 1-6 are blocked until Task 0 records a nonzero passing real SDK-resource run and freezes the exact runner in the crate-local Cargo configuration and CI.

---

## Preconditions

- [ ] Phases 1b-4 pass all gates.
- [ ] Re-read spec §4.4, Spin rows in §§5.2-5.5, and Spin file summary in §7.
- [ ] Keep capability support BestEffort for deadlines, flexible phase budgets, streamed-upload deadlines, and lazy response passthrough. No fake-resource test may promote these cells.

## Task Protocol

For each task: add only the named native or SDK-resource tests; run the exact native or pinned WASM command and require a nonzero failure from missing behavior; implement through the same production state machine; rerun focused and package tests; run `git diff --check`; then stage only listed files and make the stated commit. Resource ownership assertions use counters/defaulted completion writers and simultaneous-readiness scripts, never prose-only reasoning.

After Task 0, "run native contracts" means:
`scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`.
"Run SDK resources" and "run WASM contracts" mean invoking Cargo from
`crates/edgezero-adapter-spin` so its validated `.cargo/config.toml` is loaded:
`../../scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`
and
`../../scripts/run_test_nonzero.sh response_caller_result_succeeds_only_after_native_eof cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract`.
Every command must execute a nonzero test count; a target/linker error or zero tests is failure.

**Required exact test names:** `sdk_fields_preserve_duplicate_values`, `sdk_request_options_execute_all_setters`, `spin_error_code_table_is_exhaustive`, `spin_timeout_provenance_before_and_after_deadline`, `send_all_preflight_precedence_and_indices`, `one_slot_send_all_matches_send`, `request_preparation_consumes_entry_budget`, `adapter_final_dispatch_reapplies_request_normalization`, `canonical_uri_wire_serialization_table`, `upload_failure_wins_simultaneous_send`, `request_done_error_wins_retained_send`, `reader_gone_never_polls_request_done`, `request_trailers_succeed_only_on_clean_eof_or_reader_gone`, `response_caller_result_succeeds_only_after_native_eof`, `response_content_length_rejects_before_body_poll`, `response_content_length_explicit_identity_rejects_before_body_poll`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `raw_and_decoded_quotas_yield_before_item_65`, `streamed_tasks_consume_fast_body_before_slow_headers`, `streamed_body_races_each_pull_against_remaining_budget`, `downstream_fallback_preserves_typed_error_envelope`, and `adapter_capability_matrix_matches_outbound_spec`.

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
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.toml`
- Modify: `examples/app-demo/Cargo.lock`
- Create: `crates/edgezero-adapter-spin/tests/sdk_resources.rs`
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Modify after proof: `crates/edgezero-adapter-spin/.cargo/config.toml`, `.tool-versions`, `.github/workflows/test.yml`

- [ ] Pin root and app-demo `spin-sdk` requirements to exact `=6.0.0`. Add root workspace `wasip3 = "=0.6.0"` and make it an optional direct dependency of `edgezero-adapter-spin` enabled by `spin`, so that path dependency constrains the enum re-export in both the root and excluded app-demo graphs rather than relying on Spin SDK's compatible range.
- [ ] Refresh both independent locks with the exact manifest requirements using `cargo check --offline -p edgezero-adapter-spin --features spin --target wasm32-wasip2` and `cargo check --offline --manifest-path examples/app-demo/Cargo.toml -p app-demo-adapter-spin --features spin --target wasm32-wasip2`; these dependency-refresh commands are intentionally unlocked. Assert both locked trees report exactly `spin-sdk v6.0.0` and `wasip3 v0.6.0+wasi-0.3.0-rc-2026-03-15`.
- [ ] Add the independent `test-utils = []` feature without enabling `spin`; Task 1 adds the driver dependencies.
- [ ] Add nonzero WASM tests that construct real `Fields`, append duplicate fields, construct `RequestOptions`, call all three timeout setters, construct `Request`, and exercise request/response completion resources without invoking an external origin.
- [ ] Start with the pinned Wasmtime 44.0.1 candidate:

```sh
CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' \
  scripts/run_test_nonzero.sh sdk_fields_preserve_duplicate_values \
  cargo test --offline --locked -p edgezero-adapter-spin --no-default-features \
  --features spin,test-utils --target wasm32-wasip2 --test sdk_resources
```

- [ ] Record the exact runtime version, flags, Rust target/component setup, test count, and passing output. Put the verified runner in the crate-local `.cargo/config.toml`; CI must use the same value. `-S http=y -S p3=y` is necessary evidence, not assumed sufficient evidence.
- [ ] From `crates/edgezero-adapter-spin`, rerun without a runner environment override: `../../scripts/run_test_nonzero.sh sdk_fields_preserve_duplicate_values cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`; require the same nonzero pass. This proves the committed crate-local Cargo configuration is the configuration later tasks and CI consume.
- [ ] If the pinned runtime cannot execute these resources, STOP. Select and review a compatible exact runtime pin before changing `.tool-versions` and CI together. Compilation, native fakes, and zero-test success do not pass this gate.
- [ ] Commit after proof: `test(spin): execute WASI HTTP SDK resources`.

### Task 1: Establish the native driver seam and error classifier

**Files:**
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Create: `crates/edgezero-adapter-spin/src/outbound.rs` beside temporary `src/proxy.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/tests/sdk_resources.rs`

- [ ] Add direct `web-time`; keep `test-utils` and `spin` independent.
- [ ] Remove the whole-file WASM gate from `tests/contract.rs`; create native and SDK modules. Confirm the native command executes a nonzero deliberately failing test.
- [ ] In the native contract, test only the target-neutral classifier policy and driver seam; do not claim that a local mirror proves the SDK enum boundary.
- [ ] In `sdk_resources.rs`, add the exhaustive table over all 39 real pinned `spin_sdk::wasip3::http::types::ErrorCode` variants with no wildcard. At or after the absolute deadline, every simultaneous SDK error maps to an attributed 504. Before expiry, all five provider timeout variants map to 504 with `BudgetSource::Unspecified`; caller request policy/size maps to 400, local invariants to 500, transport/protocol to typed 502, and host internal to unspecified 502. Exercise the entire error table before and after deadline expiry. Invoke all three real request-option setters and record/assert the actual pinned Wasmtime outcome; this suite must not claim it can force host-selected `NotSupported` results.
- [ ] Test the real SDK conversion through `client::send`, `request_done`, and response completion paths under `spin,test-utils`. Non-2xx HTTP statuses remain successful responses.
- [ ] Run native contracts and require the deliberate assertion failure. From `crates/edgezero-adapter-spin`, run `cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources spin_error_code_table_is_exhaustive`; expect a nonzero compile/assertion failure from the missing classifier.
- [ ] Implement target-neutral traits/resources and `map_spin_send_err(err, deadline, cause)`. It checks absolute expiry before classifying the SDK variant and has no wildcard for the pinned exhaustive enum; any dependency update must revise the mapping and tests in the same change.
- [ ] Run native contracts; expect success, then run SDK resources filtered to `spin_error_code_table_is_exhaustive`; expect a nonzero pass against the real SDK type.
- [ ] Commit: `feat(spin): add outbound driver and error classifier`.

### Task 2: Implement preflight, request conversion, and batch execution

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing tests for empty batch, complete preflight, streamed request/response slot rejection, stable indices, partial errors, one-slot `send_all`/`send` equivalence, one method-entry `batch_now`, concurrent eligible exchange progress, and sibling isolation. Advance the clock during normalization/preflight/builder preparation and prove that time consumes the original `send`/`send_all` budget. Include GET/HEAD body errors beating batch-only errors and no polls of rejected source streams.
- [ ] In the target-neutral injected conversion suite, add tests for methods, repeated fields via append, request options order, nonzero nanosecond rounding, and every synthetic setter outcome (`NotSupported`, `Immutable`, `Other`). The real SDK-resource suite only executes the setters and asserts the pinned host's actual result; a deployed-host probe is required before claiming a different host accepts or rejects any setter. Capture the final WASI scheme/authority/path-with-query and assert exact core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and queries.
- [ ] Add `adapter_final_dispatch_reapplies_request_normalization`: after `headers_mut` introduces `Connection` nominations plus stale `Host`, `Content-Length`, and `Transfer-Encoding`, inspect the final WASI `Fields` and prove normalization ran immediately before SDK request construction. Only canonical host and adapter-owned framing may remain.
- [ ] Run native contracts; expect failure.
- [ ] Implement `SpinOutboundClient::{send, send_all}` for contract tests. Capture the monotonic snapshot as the first operation in each public method, then run `validate_for_dispatch` exactly once per request before batch-only mode checks, budget selection, body polling, normalization, or SDK construction; never re-anchor. Retain method, mode, and all policy fields before consuming request parts; production injection switches only after Task 5 is green.
- [ ] Treat `RequestOptionsError::NotSupported` as logged BestEffort degradation while retaining the outer deadline race; `Immutable`/`Other` are internal setup failures.
- [ ] Use `join_all` for eligible complete batch slots after full preflight.
- [ ] Rerun native and SDK-resource tests; expect success.
- [ ] Commit: `feat(spin): dispatch outbound requests`.

### Task 3: Implement the biased upload/send state machine

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [ ] Add failing transition tests for `Uploading`, `AwaitingRequestDone`, and `ReaderGone`; source/cap/deadline failures, partial writes, cancellation during `write_all`, clean EOF, reader-gone, early response, writer/reader drop, trailer-completion failure, and future/resource drop counts. A successful request-trailers `FutureWriter::write(Ok(None))` is the only path from clean EOF to `AwaitingRequestDone`; `FutureWriteError` means the host dropped its reader, transitions to `ReaderGone`, and never polls `request_done`. WASI HTTP 0.3's component-stream writer has no flush operation, so no test or implementation may invent one.
- [ ] Add simultaneous-readiness tests: upload failure beats send; request completion beats retained send; send is observed only after an upload step returns Pending.
- [ ] Prove request trailers attempt `Ok(None)` only for clean EOF or reader-gone and retain the default failure otherwise. Reader-gone ignores a rejected redundant completion write; clean EOF must inspect the write result and follow the transition above.
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
- [ ] Add identity/compressed `Content-Length` cases proving malformed/conflicting values and encoded/effective-identity decoded overages reject before reading the WASI response body; include exactly one bare `Content-Encoding: identity`. Compressed and unknown/stacked passthrough lengths are never compared with the decoded cap.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Assert attributed 504, no early caller-result success, and preservation of a late typed source/completion error when it wins before expiry.
- [ ] Add the joined send-plus-immediate-body-consumption regression used by Axum/Cloudflare; a fast body must complete while sibling headers remain pending.
- [ ] Add caller-result tests proving the default is failure and `Ok(())` is written only after native EOF, trailers, decoder completion, caps, and deadlines succeed. Separate the actual shapes: the body is a component `stream<u8>`; only the response trailers/completion future carries an `ErrorCode` into `map_spin_send_err`; caller-result `write(Ok(()))` returns `FutureWriteError` when the host dropped its reader. Assert protocol 502 for that failed completion handshake while a consumer waits and cleanup-only behavior during wrapper drop.
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
- [ ] Add the exact validated SDK-resource and WASM contract runner commands, with `spin,test-utils`, through `scripts/run_test_nonzero.sh` using exact sentinels; a successful zero-test command is failure.
- [ ] Keep ordinary Spin WASM compile checks.
- [ ] Add the generated Spin adapter target check:
  `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-spin --no-default-features --features spin --target wasm32-wasip2)`.
- [ ] Add and execute `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`; clippy compilation alone does not execute the feature-gated `cli.rs` table.
- [ ] Add table-driven capability tests and publish Spin's override only now: Native for outbound HTTP, header fidelity, and slot isolation; BestEffort for deadlines, flexible phase budget, streamed-upload deadlines, and lazy response passthrough; future capabilities -> Unsupported.
- [ ] Commit: `ci: execute Spin outbound contracts`.

## Phase Verification

- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`
- [ ] `(cd crates/edgezero-adapter-spin && ../../scripts/run_test_nonzero.sh response_caller_result_succeeds_only_after_native_eof cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract)` using the validated crate-local runner.
- [ ] `(cd crates/edgezero-adapter-spin && ../../scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources)` using the validated crate-local runner.
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `cargo check --offline --locked -p edgezero-adapter-spin --no-default-features --features spin --target wasm32-wasip2`
- [ ] `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-spin --no-default-features --features spin --target wasm32-wasip2)`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: Spin's deterministic contract and real SDK bindings execute in CI, and its conservative capability row lands with them. Host cancellation remains documented BestEffort until a separate live characterization proves a finite bound.
