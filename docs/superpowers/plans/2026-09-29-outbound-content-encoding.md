# Outbound Content-Encoding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode standards-compliant HTTP `deflate` responses and expose typed reasons for every raw content-encoding passthrough outcome.

**Architecture:** Keep classification and codec policy in `edgezero-core`; adapters continue to own native transport, deadline, and cancellation wrappers. Hard-migrate the public classifier to an owned typed result, generalize the existing decoder-memory control across Brotli and flate-family decoders, and preserve raw headers only for typed passthrough outcomes.

**Tech Stack:** Rust 1.95, `http`, `async-compression`, `compression-codecs`, `flate2`/`miniz_oxide`, futures streams, four EdgeZero runtime adapters.

---

### Task 1: Freeze the classifier and error contracts

**Files:**
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/response_egress_framing.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [x] Add failing classifier tests for bare `deflate`, valid unknown tokens, repeated fields, valid comma lists, invalid list elements, parameters, empty/OWS-only values, non-UTF-8 bytes, and case folding.
- [x] Run `cargo test -p edgezero-core compression::tests::content_encoding_classifier_covers_every_visible_shape`; verify compilation fails because `Deflate` and `PassthroughReason` do not exist.
- [x] Hard-cut `ContentEncoding` to add `Deflate` and `Passthrough(PassthroughReason)`; validate RFC token bytes and preserve lowercase unknown tokens. Borrow the now-non-`Copy` classification in `enforce_payload_content_length` and classify `Deflate` as compressed wire length.
- [x] Add failing `BadGatewayDecodeReason::Deflate` taxonomy/category tests, run them red, then add the variant and stable `bad_gateway_decode_deflate` egress category.
- [x] Re-export `PassthroughReason` and run the focused classifier/taxonomy tests green.
- [x] Run the full core compression suite after Task 2 supplies the new decoder API.

### Task 2: Add audited zlib-wrapped deflate decoding

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.lock`
- Modify: `crates/edgezero-core/Cargo.toml`
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Create: `docs/audits/2026-09-29-flate-decoder-memory-accounting.md`
- Create: `scripts/flate_dependency_contract.json`
- Create: `scripts/check_flate_dependency_contract.mjs`
- Modify: `.github/workflows/test.yml`
- Modify: `scripts/run_tests.sh`

- [x] Add failing stream tests for zlib-wrapped round-trip, raw RFC 1951 rejection, invalid/truncated input, trailing bytes, typed late source-error preservation, source release, and gzip/deflate decoder-memory refusal before body polling.
- [x] Run the focused deflate test and verify compilation fails because the shared decoder, fixed charge, and generalized gzip signature do not exist.
- [x] Enable the `zlib` codecs, exact-pin the audited flate dependency graph, and update both lockfiles.
- [x] Derive and document a conservative fixed flate decoder charge from pinned `async-compression`, `compression-codecs`, `flate2`, and `miniz_oxide`; add a compiled regression covering `InflateState`, codec wrappers, and the EdgeZero output buffer.
- [x] Hard-rename `DEFAULT_MAX_BROTLI_DECODER_BYTES`, the request field/parts field, and builder to `DEFAULT_MAX_DECODER_BYTES` / `max_decoder_bytes`; enforce it before constructing gzip/deflate decoders and against the existing Brotli charge.
- [x] Implement `decode_deflate_stream` with zlib framing, terminal resource release, read-ahead/trailing and concatenated-stream rejection, native-EOF validation, and typed `Decode(Deflate)` failures. Add a pending-after-codec-EOF test proving the decoder does not report terminal success early.
- [x] Add a separate flate dependency contract with exact package/version/source/checksum coverage and direct-normal-dependency assertions for both the root and excluded demo workspaces; wire it after both locked fetches in CI and the local test runner.
- [x] Run `cargo test -p edgezero-core compression`, `cargo test -p edgezero-core outbound::tests`, and `node scripts/check_flate_dependency_contract.mjs`; expect all to pass.

### Task 3: Integrate every adapter

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: in-module conversion tests in the Cloudflare, Fastly, and Spin outbound modules

- [x] Add adapter coverage for zlib-wrapped deflate decoding, `content-encoding`/`content-length` stripping, decoded-cap enforcement, and typed raw-preservation classification. Shared core tests cover codec edge cases; Axum covers the native conversion path, and each adapter compiles against the same branch.
- [x] Update each adapter to consume the borrowed non-`Copy` classification, enforce the generalized decoder cap, and route `Deflate` through the shared decoder.
- [x] Run the workspace adapter test suite and feature compilation gates; all existing adapter tests pass.

### Task 4: Migrate generated and example applications

**Files:**
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: associated generator and demo tests

- [x] Add generator/demo assertions for `max_decoder_bytes` and the removal of `max_brotli_decoder_bytes`.
- [x] Hard-migrate templates and demo code to the generalized control.
- [x] Run the workspace CLI/demo tests and generated-project compilation gates.

### Task 5: Align normative documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/superpowers/plans/2026-09-06-outbound-http-phase2-body-response-limits.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/guide/architecture.md`
- Modify: `docs/guide/adapters/overview.md`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/streaming.md`
- Modify: `CHANGELOG.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `scripts/check_outbound_legacy_api.sh`

- [x] Add docs-contract assertions for the new API/table/error/limit names and removal of stale `deflate`-passthrough wording.
- [x] Update the public API, portable encoding table, pipeline, limits, error taxonomy, test matrix, dependency evidence, public guides, and phase plans to include typed passthrough and zlib-wrapped deflate.
- [x] Record the hard migration and reject the removed bare variant/builder in legacy checks.
- [x] Run the docs and legacy contract scripts.

### Task 6: Full verification and delivery

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run the exact Cloudflare, Fastly, and Spin WASM clippy/check commands from `.github/workflows/format.yml`.
- [x] Run the excluded demo workspace tests and checks.
- [x] Review `git diff --check`, the complete diff, and generated documentation consistency.
- [ ] Commit, push PR 275, and verify hosted checks.
