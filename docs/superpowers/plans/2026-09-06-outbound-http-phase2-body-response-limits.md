# Outbound HTTP Phase 2: Typed Bodies, Decoding, and Response Limits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the portable response pipeline: exact typed stream errors, response-limit errors, header/body normalization, encoded and decoded caps, Brotli preflight, gzip/Brotli completion, optional rechunking, and deadline-aware drains.

**Architecture:** Keep all transport-independent byte accounting and decoding in core. Adapters retain timers, abort handles, trailer/completion protocols, and native body ownership. The pipeline order is fixed: visible header limits, normalization, body disposition, encoded limit, Brotli-window gate, decoder/native EOF, rechunker, outer deadline/cancellation wrapper, then decoded limit or consumer.

**Tech Stack:** Rust 1.95, `bytes`, `futures`, exact manifest-pinned `async-compression = 0.4.43`, `http`, `serde`, `serde_json`.

---

## Preconditions

- [ ] Phase 1b is complete and `cargo test --offline --locked -p edgezero-core --lib` passes.
- [ ] Re-read spec §§3.1.4, 3.3.3-3.4.5, §5.1, and the core file summary in §7.
- [ ] Preserve `Body::into_bytes_bounded(usize)` as the inbound 400 helper. Outbound response drains are separate `u64`/502 APIs.
- [ ] Header-limit ties resolve `HeaderCount` before `HeaderBytes`, matching the specified per-entry algorithm. A malformed or conflicting payload-bearing `Content-Length` is `BadGatewayReason::Protocol`; HEAD/304 representation metadata is preserved without body interpretation.

## Task Protocol

For every task: add only the named tests; run the exact focused filter and require failure from the missing behavior; implement the listed API/algorithm; rerun that filter and the full core library suite; run `git diff --check`; then stage only the task's files and make the stated commit. For stream tests, use poll-counting scripted sources so early-stop, order, and EOF claims are executable assertions rather than comments.

**Required exact test names:** `response_too_large_preserves_every_reason_without_serializing_it`, `response_too_large_constructors_preserve_unspecified_and_specific_reason`, `body_from_stream_preserves_edge_error`, `body_from_external_stream_maps_to_internal`, `body_into_bytes_bounded_preserves_edge_error`, `body_stream_accepts_infallible_bytes`, `body_bounded_checks_before_append`, `response_normalization_precedence_table`, `response_resource_header_limit_count_wins_tie`, `content_encoding_classifier_covers_every_visible_shape`, `payload_content_length_rejects_before_body_poll`, `payload_content_length_explicit_identity_rejects_before_body_poll`, `payload_content_length_skips_decoded_cap_for_compressed_body`, `outbound_response_into_response_reapplies_normalization`, `encoded_limit_stops_before_decoder`, `rechunk_stream_is_lazy_and_ordered`, `outbound_response_until_deadline_wins_ready_result`, `outbound_response_json_error_classification`, `decoder_carrier_restores_exact_edge_error`, `decode_gzip_drains_every_member_to_native_eof`, `decode_brotli_rejects_trailing_data`, and `brotli_window_rejects_before_decoder_allocation`.

**Expected red:** each task either fails to resolve its new type/helper or observes the old behavior: 500/untyped limit, erased stream error, retained unsafe header, over-limit poll/append, cap winning an expired deadline, decoder stopping at its first end marker, or decoder construction before WBITS rejection. Do not accept a panic as the intended red result.

**Portable pipeline skeleton:** adapters compose the helpers in this exact nesting; an implementation with a different nesting fails the order tests.

```rust
let encoding = classify_content_encoding(&headers);
enforce_payload_content_length(
    &headers,
    encoding == ContentEncoding::Identity,
    max_decoded_response_bytes,
    max_encoded_response_bytes,
)?;
let raw = limit_encoded_stream(native_body, max_encoded_response_bytes);
let decoded = match encoding {
    ContentEncoding::Brotli => decode_brotli_stream(gate_brotli_window(
        raw,
        max_brotli_window_bits,
    )),
    ContentEncoding::Gzip => decode_gzip_stream(raw),
    ContentEncoding::Identity => raw,
    ContentEncoding::Passthrough => raw,
};
if matches!(encoding, ContentEncoding::Brotli | ContentEncoding::Gzip) {
    headers.remove(CONTENT_ENCODING);
    headers.remove(CONTENT_LENGTH);
}
let shaped = rechunk_stream(decoded, max_chunk_bytes);
let timed = adapter_deadline_wrapper(shaped, budget, native_completion);
let body = drain_or_stream_with_decoded_cap(timed, response_mode);
```

`adapter_deadline_wrapper` and `native_completion` above are adapter-owned placeholders, not core APIs; they show why rechunking must remain inside the outer timer/cancellation wrapper.

Every transport-independent stream helper accepts and returns the same concrete erased type;
none returns a branch-specific opaque `impl Stream`:

```rust
pub type BodyStream = LocalBoxStream<'static, Result<Bytes, EdgeError>>;

pub fn decode_brotli_stream(stream: BodyStream) -> BodyStream;
pub fn decode_gzip_stream(stream: BodyStream) -> BodyStream;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentEncoding { Brotli, Gzip, Identity, Passthrough }
pub fn classify_content_encoding(headers: &HeaderMap) -> ContentEncoding;
pub fn enforce_response_header_limits(
    headers: &HeaderMap,
    max_bytes: Option<u64>,
    max_count: Option<u64>,
) -> Result<(), EdgeError>;
pub fn enforce_payload_content_length(
    headers: &HeaderMap,
    is_identity: bool,
    max_decoded_bytes: Option<u64>,
    max_encoded_bytes: Option<u64>,
) -> Result<(), EdgeError>;
pub fn gate_brotli_window(stream: BodyStream, max_bits: u8) -> BodyStream;
pub fn limit_encoded_stream(stream: BodyStream, max: Option<u64>) -> BodyStream;
pub fn rechunk_stream(stream: BodyStream, max: Option<NonZeroU64>) -> BodyStream;
```

Ownership is exact: `BodyStream` lives at `edgezero_core::body::BodyStream`;
`ContentEncoding`, `classify_content_encoding`, `decode_brotli_stream`,
`decode_gzip_stream`, and `gate_brotli_window` live in `edgezero_core::compression`;
`enforce_payload_content_length`, `enforce_response_header_limits`,
`limit_encoded_stream`, and `rechunk_stream` live in `edgezero_core::outbound`. Core re-exports
every one at the `edgezero_core` root, and adapter crates use that single stable import
surface. Core owns typed source restoration and codec/accounting errors; adapters own the
outer deadline, cancellation guard, and native-completion protocol.

### Task 1: Add typed response-limit errors

**Files:**
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [ ] Add failing `response_too_large_*` tests for all reasons: `BrotliWindow`, `DecodedBody`, `EncodedBody`, `HeaderBytes`, `HeaderCount`, and `Unspecified`.
- [ ] Assert status 502, kind `response_too_large`, no `reason`, `field_path`, or `Retry-After` in the wire response, and preservation of the Rust-side reason.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib response_too_large_`; expect failure.
- [ ] Add `ResponseLimitReason` and `EdgeError::ResponseTooLarge`; update every exhaustive match in production and tests in alphabetical order.
- [ ] Add and test both required constructors: `response_too_large(message)` stores `Unspecified`; `response_too_large_with_reason(message, reason)` stores the supplied reason.
- [ ] Rerun the focused test and core suite; expect success.
- [ ] Commit: `feat(core): add typed outbound response limits`.

### Task 2: Make body stream errors exact

**Files:**
- Modify: `crates/edgezero-core/src/body.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/proxy.rs`
- Modify: `crates/edgezero-adapter-axum/src/request.rs`
- Modify: `crates/edgezero-adapter-axum/src/response.rs`
- Modify: `crates/edgezero-adapter-axum/src/proxy.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/proxy.rs`
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify: `crates/edgezero-adapter-fastly/src/proxy.rs`
- Modify: `crates/edgezero-adapter-spin/src/response.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

**Contract:**

```rust
pub type BodyStream = LocalBoxStream<'static, Result<Bytes, EdgeError>>;

pub enum Body {
    Once(Bytes),
    Stream(BodyStream),
}

pub fn from_external_stream<S, E>(stream: S) -> Self; // E -> Internal
pub fn from_stream<S>(stream: S) -> Self;          // exact EdgeError
pub async fn into_bytes_bounded(self, max_size: usize) -> Result<Bytes, EdgeError>;
pub fn into_stream(self) -> Option<BodyStream>;
pub fn stream<S>(stream: S) -> Self;                // infallible Bytes
```

- [ ] Add failing tests for typed error identity, external error conversion, infallible input, `From<Bytes>`, exact-limit success, first-byte-over-limit failure, checked overflow, and no poll after failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib body::tests::`; expect failure.
- [ ] Implement `BodyStream`, all three constructors, `into_stream`, `From<Bytes>`, and pre-append checked accounting. `into_bytes_bounded` must propagate an existing `EdgeError` unchanged. Add one typed-source test for every public error family and a foreign sentinel proving `from_external_stream` sanitizes before collection. Use `from_external_stream` only at actual platform/foreign-error inputs; a platform inbound producer either uses that compatibility boundary or deliberately maps to a public `EdgeError` before `from_stream`. Remove redundant `map_err(EdgeError::internal)` where it would erase an existing `EdgeError`.
- [ ] Run `rg -n 'Body::from_stream|Body::from_external_stream|Body::Stream|Self::Stream|\.into_stream\(' crates examples/app-demo --glob '*.rs' --glob '*.hbs'`. Classify construction/consumption sites, ignoring definitions, comments, and introspection-only matches. Add error-identity regressions at EdgeZero-owned error-producing boundaries; inbound platform streams receive only compile-required `from_external_stream` migration that preserves their existing Internal mapping.
- [ ] Run the core suite and `cargo test --workspace --all-targets`; expect success.
- [ ] Commit: `refactor(core): preserve typed body stream errors`.

### Task 3: Normalize response metadata and body disposition

**Files:**
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `response_normalization_*` tests for repeated and malformed `Connection`, nominated `content-encoding`/`content-length`, UTF-8 filtering, standard hop-by-hop fields, and idempotence.
- [ ] Add body-disposition cases for HEAD, every 1xx class, 204, positive/zero/absent 205, 304, and ordinary payload-bearing responses.
- [ ] Add explicit malformed, duplicate-conflicting, and comma-list `Content-Length` cases. Payload-bearing/205 ambiguity must fail as protocol; HEAD/304 metadata is retained without parsing it as a body promise.
- [ ] Add `outbound_response_into_response_reapplies_normalization`: mutate headers through `headers_mut`, then prove `into_response` strips hop-by-hop and nominated fields, preserves repeated end-to-end fields, keeps the body lazy, and returns typed protocol/internal errors rather than an infallible or generic conversion.
- [ ] Run both focused commands; each must execute a nonzero matching test count and fail for missing behavior:
  - `cargo test --offline --locked -p edgezero-core --lib response_normalization_`
  - `cargo test --offline --locked -p edgezero-core --lib outbound_response_into_response_`
- [ ] Implement `normalize_response_headers` returning `ResponseBodyDisposition`, a private checked `parse_content_length`, plus `OutboundResponse::into_response`. Preserve HEAD/304 representation metadata; strip prohibited 1xx/204 framing; validate payload/205 length syntax; rewrite successful 205 to `Content-Length: 0` only after capturing `declared_body`. `into_response` must reapply the same helper idempotently after any `headers_mut` use.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): normalize outbound response metadata`.

### Task 4: Implement response header, encoded-byte, and chunk wrappers

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`

**Pre-poll length contract:** adapters call this only after response normalization and only
for `ResponseBodyDisposition::Payload`:

```rust
pub fn enforce_payload_content_length(
    headers: &HeaderMap,
    is_identity: bool,
    max_decoded_bytes: Option<u64>,
    max_encoded_bytes: Option<u64>,
) -> Result<(), EdgeError>;
```

- [ ] Add failing `response_resource_*` tests for exact and over-limit header count/bytes, repeated values, checked overflow, and count-before-bytes precedence.
- [ ] Add classifier tables for absent and exactly one bare `identity`, `gzip`, or `br`; ASCII case and surrounding space/tab; repeated fields; comma-stacked, parameterized, empty, malformed/non-UTF-8, and unknown values. The alphabetically declared results are `Brotli`, `Gzip`, `Identity`, and `Passthrough`; every adapter consumes this helper rather than inspecting the header itself.
- [ ] Add payload-length tables for absent/zero/exact/over values; malformed, comma-list, and conflicting values; effective identity (absent or one bare `identity`) versus gzip/br/passthrough coding; encoded-only, decoded-only, and combined caps. Encoded applies to every coding; decoded rejects early only for effective identity. Include an explicit-identity overage that rejects before the scripted body is polled. HEAD/1xx/204/304 never call this helper; 205 uses its disposition protocol.
- [ ] Add encoded-limit tests for exact/over limit, empty chunks counting as zero but remaining observable, cumulative gzip-member input, early stop, and typed source-error preservation.
- [ ] Add `rechunk_*` tests for lazy pulls, order preservation, exact maximum item size, empty items, source error ordering, early drop, and no claim about backing allocation/RSS.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib response_resource_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib content_encoding_classifier_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib payload_content_length_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib encoded_limit_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib rechunk_`; expect a nonzero failure.
- [ ] Implement `ContentEncoding` and `classify_content_encoding` in `compression.rs`;
  implement `enforce_payload_content_length`, `enforce_response_header_limits`,
  `limit_encoded_stream`, and `rechunk_stream` in `outbound.rs`, with the exact shared
  signatures above. Re-export every new public item from `lib.rs`. The classifier uses
  `HeaderMap::get_all`; only one bare visible field can select Identity/Gzip/Brotli, and
  every repeated/stacked/parameterized/malformed/non-UTF-8/unknown shape is Passthrough. No
  helper may poll after a terminal error or cap result.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): enforce outbound response resources`.

### Task 5: Add deadline-aware response drains and JSON

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `outbound_response_drain_*` tests for empty and nonempty `Once` plus `Stream`; exact/over decoded limits; typed source failures; and pre-append accounting.
- [ ] Add `_until` tests for entry, pre-poll, post-ready chunk, source error, cap result, and EOF checks. If expired when a decision is ready, 504 wins with `BudgetSource::Unspecified`, because this API receives only `Deadline`. Separately prove an adapter-style wrapper preserves its supplied `DispatchBudget::cause`.
- [ ] Add `outbound_response_json_*` tests for valid/malformed JSON, buffered/streamed modes, and streamed `json` invalid-state mapping to protocol 502.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib outbound_response_drain_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib outbound_response_until_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib outbound_response_json_`; expect a nonzero failure.
- [ ] Implement synchronous `json<T>(&self)` for `Body::Once`, returning protocol 502 for `Body::Stream`; implement consuming `into_bytes_bounded`, `into_bytes_bounded_until`, `json_bounded`, and `json_bounded_until`. Malformed upstream JSON is decode 502. Do not delegate to the inbound body helper.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): add bounded outbound response drains`.

### Task 6: Replace decoder error plumbing and completion semantics

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/proxy.rs`
- Modify: `crates/edgezero-adapter-fastly/src/proxy.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.lock`

- [ ] Add failing `decoder_carrier_*` tests proving all `EdgeError` variants survive the `io::Error` bridge unchanged and decoder-originated errors become `BadGatewayReason::Decode` with diagnostics.
- [ ] Change the workspace requirement to exact `async-compression = "=0.4.43"`, then refresh both lockfiles with `cargo update --offline -p async-compression --precise 0.4.43` and `cargo update --offline --manifest-path examples/app-demo/Cargo.toml -p async-compression --precise 0.4.43`.
- [ ] Assert both graphs resolve exactly 0.4.43:
  - `cargo tree --offline --locked -p edgezero-core -e normal | rg 'async-compression v0\.4\.43'`
  - `cargo tree --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-core -e normal | rg 'async-compression v0\.4\.43'`
- [ ] Add `decode_gzip_*` tests for one member, concatenated/empty/split members, cumulative output, corrupt/truncated later members, trailing garbage, late source errors, empty chunks, and drain to native EOF.
- [ ] Add `decode_brotli_*` tests for one stream, buffered read-ahead recovery, trailing bytes, a second stream, late source errors, empty chunks, and native EOF.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib decoder_carrier_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib decode_gzip_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib decode_brotli_`; expect a nonzero failure.
- [ ] Implement the private exact carrier and the concrete `BodyStream -> BodyStream` decoder signatures; enable `GzipDecoder::multiple_members(true)`; recover and inspect Brotli unread input and continue to native EOF. Migrate the existing Cloudflare/Fastly shared-decoder call sites in this same task so the commit leaves the workspace buildable and does not erase `EdgeError`. Never drain after cap/error/timeout.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `fix(core): preserve decoder completion and errors`.

### Task 7: Gate Brotli allocation from the stream prefix

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `brotli_window_*` tests for all WBITS values 10 through 30, standard and two-byte large-window forms, every split prefix boundary, exact/over configured limits, malformed/truncated prefixes, replay fidelity, and proof that rejection occurs before decoder construction.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib brotli_window_`; expect failure.
- [ ] Implement a bounded prefix parser that returns replayable bytes plus WBITS. Over-limit is `BrotliWindow`; invalid syntax is decode 502.
- [ ] Compose only the **core-owned segment** in a private core test helper: classify -> payload-length checks -> encoded counter -> optional Brotli prefix gate -> decoder -> optional rechunker. Add table tests for absent/identity, bare case-insensitive gzip/br, unknown, parameterized, stacked, and repeated encodings. The helper mutates test headers to assert the contract that known decoded layers remove visible `content-encoding` and `content-length`, while Identity and Passthrough preserve them. Do not add a production response orchestrator, adapter deadline/cancellation wrapper, native-completion source, or platform response constructor in core. Phases 4-6 compose these public helpers at each native boundary and perform the specified header mutation before `OutboundResponse` construction.
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
