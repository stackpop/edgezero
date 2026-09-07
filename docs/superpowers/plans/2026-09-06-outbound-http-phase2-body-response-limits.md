# Outbound HTTP Phase 2: Typed Bodies, Decoding, and Response Limits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the portable response pipeline: exact typed stream errors, response-limit errors, header/body normalization, encoded and decoded caps, Brotli preflight, gzip/Brotli completion, optional rechunking, and deadline-aware drains.

**Architecture:** Keep all transport-independent byte accounting and decoding in core. Adapters retain timers, abort handles, trailer/completion protocols, and native body ownership. The pipeline order is fixed: visible header limits, normalization, body disposition, encoded limit, Brotli-window gate, decoder/native EOF, rechunker, outer deadline/cancellation wrapper, then decoded limit or consumer.

**Tech Stack:** Rust 1.95, `bytes`, `futures`, `async-compression = 0.4.43` from the lockfile, `http`, `serde`, `serde_json`.

---

## Preconditions

- [ ] Phase 1b is complete and `cargo test --offline --locked -p edgezero-core --lib` passes.
- [ ] Re-read spec §§3.1.4, 3.3.3-3.4.5, §5.1, and the core file summary in §7.
- [ ] Preserve `Body::into_bytes_bounded(usize)` as the inbound 400 helper. Outbound response drains are separate `u64`/502 APIs.
- [ ] Header-limit ties resolve `HeaderCount` before `HeaderBytes`, matching the specified per-entry algorithm. A malformed or conflicting payload-bearing `Content-Length` is `BadGatewayReason::Protocol`; HEAD/304 representation metadata is preserved without body interpretation.

## Task Protocol

For every task: add only the named tests; run the exact focused filter and require failure from the missing behavior; implement the listed API/algorithm; rerun that filter and the full core library suite; run `git diff --check`; then stage only the task's files and make the stated commit. For stream tests, use poll-counting scripted sources so early-stop, order, and EOF claims are executable assertions rather than comments.

**Required exact test names:** `response_too_large_preserves_every_reason_without_serializing_it`, `body_from_stream_preserves_edge_error`, `body_from_external_stream_maps_to_internal`, `body_bounded_checks_before_append`, `response_normalization_precedence_table`, `response_header_limit_count_wins_tie`, `outbound_response_into_response_reapplies_normalization`, `encoded_limit_stops_before_decoder`, `rechunk_stream_is_lazy_and_ordered`, `outbound_response_until_deadline_wins_ready_result`, `outbound_response_json_error_classification`, `decoder_carrier_restores_exact_edge_error`, `gzip_drains_every_member_to_native_eof`, `brotli_rejects_trailing_data`, and `brotli_window_rejects_before_decoder_allocation`.

**Expected red:** each task either fails to resolve its new type/helper or observes the old behavior: 500/untyped limit, erased stream error, retained unsafe header, over-limit poll/append, cap winning an expired deadline, decoder stopping at its first end marker, or decoder construction before WBITS rejection. Do not accept a panic as the intended red result.

**Portable pipeline skeleton:** adapters compose the helpers in this exact nesting; an implementation with a different nesting fails the order tests.

```rust
let raw = limit_encoded_stream(native_body, max_encoded_response_bytes);
let decoded = match classify_content_encoding(&headers) {
    ContentEncoding::Brotli => decode_brotli_stream(gate_brotli_window(
        raw,
        max_brotli_window_bits,
    )),
    ContentEncoding::Gzip => decode_gzip_stream(raw),
    ContentEncoding::Passthrough => raw,
};
let shaped = rechunk_stream(decoded, max_chunk_bytes);
let timed = adapter_deadline_wrapper(shaped, budget, native_completion);
let body = drain_or_stream_with_decoded_cap(timed, response_mode);
```

`adapter_deadline_wrapper` and `native_completion` above are adapter-owned placeholders, not core APIs; they show why rechunking must remain inside the outer timer/cancellation wrapper.

### Task 1: Add typed response-limit errors

**Files:**
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [ ] Add failing `response_too_large_*` tests for all reasons: `BrotliWindow`, `DecodedBody`, `EncodedBody`, `HeaderBytes`, `HeaderCount`, and `Unspecified`.
- [ ] Assert status 502, kind `response_too_large`, no `reason`, `field_path`, or `Retry-After` in the wire response, and preservation of the Rust-side reason.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib response_too_large_`; expect failure.
- [ ] Add `ResponseLimitReason` and `EdgeError::ResponseTooLarge`; update every exhaustive match in production and tests in alphabetical order.
- [ ] Rerun the focused test and core suite; expect success.
- [ ] Commit: `feat(core): add typed outbound response limits`.

### Task 2: Make body stream errors exact

**Files:**
- Modify: `crates/edgezero-core/src/body.rs`
- Modify only where compilation requires constructor migration: `crates/edgezero-core/src/context.rs`, adapter `request.rs`/`response.rs`/legacy `proxy.rs`

**Contract:**

```rust
pub enum Body {
    Once(Bytes),
    Stream(LocalBoxStream<'static, Result<Bytes, EdgeError>>),
}

pub fn from_stream<S>(stream: S) -> Self;          // exact EdgeError
pub fn from_external_stream<S, E>(stream: S) -> Self; // E -> Internal
pub fn into_stream(self) -> Option<LocalBoxStream<'static, Result<Bytes, EdgeError>>>;
```

- [ ] Add failing tests for typed error identity, external error conversion, infallible input, `From<Bytes>`, exact-limit success, first-byte-over-limit failure, checked overflow, and no poll after failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib body::tests::`; expect failure.
- [ ] Implement the typed constructors and pre-append checked accounting. Use `from_external_stream` only at actual foreign-error boundaries; remove redundant `map_err(EdgeError::internal)` where it would erase an existing `EdgeError`.
- [ ] Run the core suite and `cargo test --workspace --all-targets`; expect success.
- [ ] Commit: `refactor(core): preserve typed body stream errors`.

### Task 3: Normalize response metadata and body disposition

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `response_normalization_*` tests for repeated and malformed `Connection`, nominated `content-encoding`/`content-length`, UTF-8 filtering, standard hop-by-hop fields, and idempotence.
- [ ] Add body-disposition cases for HEAD, every 1xx class, 204, positive/zero/absent 205, 304, and ordinary payload-bearing responses.
- [ ] Add explicit malformed, duplicate-conflicting, and comma-list `Content-Length` cases. Payload-bearing/205 ambiguity must fail as protocol; HEAD/304 metadata is retained without parsing it as a body promise.
- [ ] Add `outbound_response_into_response_reapplies_normalization`: mutate headers through `headers_mut`, then prove `into_response` strips hop-by-hop and nominated fields, preserves repeated end-to-end fields, keeps the body lazy, and returns typed protocol/internal errors rather than an infallible or generic conversion.
- [ ] Run both focused commands; each must execute a nonzero matching test count and fail for missing behavior:
  - `cargo test --offline --locked -p edgezero-core --lib response_normalization_`
  - `cargo test --offline --locked -p edgezero-core --lib outbound_response_into_response_`
- [ ] Implement `normalize_response_headers` returning `ResponseBodyDisposition`, plus `OutboundResponse::into_response`. Preserve HEAD/304 representation metadata; strip prohibited 1xx/204 framing; rewrite successful 205 to `Content-Length: 0` only after capturing `declared_body`. `into_response` must reapply the same helper idempotently after any `headers_mut` use.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): normalize outbound response metadata`.

### Task 4: Implement response header, encoded-byte, and chunk wrappers

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `response_resource_*` tests for exact and over-limit header count/bytes, repeated values, checked overflow, and count-before-bytes precedence.
- [ ] Add encoded-limit tests for exact/over limit, empty chunks counting as zero but remaining observable, cumulative gzip-member input, early stop, and typed source-error preservation.
- [ ] Add `rechunk_*` tests for lazy pulls, order preservation, exact maximum item size, empty items, source error ordering, early drop, and no claim about backing allocation/RSS.
- [ ] Run the three focused filters; expect failure.
- [ ] Implement `enforce_response_header_limits`, `limit_encoded_stream`, and `rechunk_stream`. No wrapper may poll after a terminal error or cap result.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): enforce outbound response resources`.

### Task 5: Add deadline-aware response drains and JSON

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `outbound_response_drain_*` tests for empty and nonempty `Once` plus `Stream`; exact/over decoded limits; typed source failures; and pre-append accounting.
- [ ] Add `_until` tests for entry, pre-poll, post-ready chunk, source error, cap result, and EOF checks. If expired when a decision is ready, 504 with the supplied cause wins.
- [ ] Add `outbound_response_json_*` tests for valid/malformed JSON, buffered/streamed modes, and streamed `json` invalid-state mapping to protocol 502.
- [ ] Run each focused filter; expect failure.
- [ ] Implement synchronous `json<T>(&self)` for `Body::Once`, returning protocol 502 for `Body::Stream`; implement consuming `into_bytes_bounded`, `into_bytes_bounded_until`, `json_bounded`, and `json_bounded_until`. Malformed upstream JSON is decode 502. Do not delegate to the inbound body helper.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): add bounded outbound response drains`.

### Task 6: Replace decoder error plumbing and completion semantics

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Inspect: `Cargo.toml`, `Cargo.lock`

- [ ] Add failing `decoder_carrier_*` tests proving all `EdgeError` variants survive the `io::Error` bridge unchanged and decoder-originated errors become `BadGatewayReason::Decode` with diagnostics.
- [ ] Verify `Cargo.lock` resolves `async-compression` 0.4.43. The spec's pin is lockfile-based; do not change the workspace's compatible manifest range in this task.
- [ ] Add `decode_gzip_*` tests for one member, concatenated/empty/split members, cumulative output, corrupt/truncated later members, trailing garbage, late source errors, empty chunks, and drain to native EOF.
- [ ] Add `decode_brotli_*` tests for one stream, buffered read-ahead recovery, trailing bytes, a second stream, late source errors, empty chunks, and native EOF.
- [ ] Run each filter; expect failure.
- [ ] Implement the private exact carrier; enable `GzipDecoder::multiple_members(true)`; recover and inspect Brotli unread input and continue to native EOF. Never drain after cap/error/timeout.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `fix(core): preserve decoder completion and errors`.

### Task 7: Gate Brotli allocation from the stream prefix

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `brotli_window_*` tests for all WBITS values 10 through 30, standard and two-byte large-window forms, every split prefix boundary, exact/over configured limits, malformed/truncated prefixes, replay fidelity, and proof that rejection occurs before decoder construction.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib brotli_window_`; expect failure.
- [ ] Implement a bounded prefix parser that returns replayable bytes plus WBITS. Over-limit is `BrotliWindow`; invalid syntax is decode 502.
- [ ] Integrate the full shared pipeline and add table tests for absent/identity, bare case-insensitive gzip/br, unknown, parameterized, stacked, and repeated encodings.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): gate brotli decoder allocation`.

## Phase Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-core --target wasm32-unknown-unknown`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `rg -n 'map_err\(EdgeError::internal\)' crates/edgezero-*` and inspect every remaining foreign-error boundary.
- [ ] `git diff --check`

Expected result: core exposes one typed, ordered response-processing pipeline. Platform timers and cancellation remain adapter work in Phases 4-6.
