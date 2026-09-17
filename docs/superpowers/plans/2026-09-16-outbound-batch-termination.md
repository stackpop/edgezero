# Outbound Batch Termination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make normal completion, method-cutoff termination, and malformed adapter-driver termination unambiguous in the public outbound batch API.

**Architecture:** Replace the stream-like `Option` terminal signal with an explicit `OutboundBatchNext::{Item, Finished}` result and add `OutboundBatchTermination::{Completed, Cutoff}` to ordered results. Adapter streams emit private-contract driver events for slots, cutoff, or failure; core validates index range, uniqueness, and premature EOF and returns `OutboundBatchFailure` with the precise `EdgeError::Internal` and previously collected slots instead of silently converting invariant violations into unresolved slots.

**Tech Stack:** Rust 2024, `futures`, `async-stream`, EdgeZero `EdgeError`, provider adapter contract tests, VitePress documentation checks.

---

### Task 1: Lock the corrected contract in core tests

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [x] **Step 1: Replace the premature-EOF expectation with failing invariant tests**

Add focused tests asserting:

- all slots resolve as `OutboundBatchTermination::Completed`;
- an explicit driver cutoff yields `OutboundBatchTermination::Cutoff` and only then permits `None` slots;
- premature driver EOF returns `EdgeError::Internal`;
- duplicate and out-of-range slot indices return `EdgeError::Internal`;
- an empty batch completes normally;
- dropping a pending `next()` future remains cancellation-safe.

- [x] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p edgezero-core outbound::tests::batch_ -- --nocapture`

Expected: compilation/test failure because the typed termination API and driver events do not exist.

- [x] **Step 3: Implement the minimal core state machine**

Add public non-exhaustive enums:

```rust
pub enum OutboundBatchTermination {
    Completed,
    Cutoff,
}

pub enum OutboundBatchNext {
    Item(OutboundBatchItem),
    Finished(OutboundBatchTermination),
}
```

Change `OutboundBatch::next` to return `Result<OutboundBatchNext, EdgeError>`, `collect` to return `Result<OutboundBatchResults, OutboundBatchFailure>`, and `HttpClient::send_all_until` to return `Result<OutboundBatchResults, OutboundBatchFailure>`. Add `termination` to `OutboundBatchResults`.

Use the public adapter driver event with `Item`, `Cutoff`, and `Failed` variants. Mark completion only after every index resolves. Return a fixed category-safe internal error for core-detected premature EOF, duplicate indices, out-of-range indices, or impossible terminal transitions; preserve adapter-supplied failures exactly. Ordered collection retains every slot observed before either class of failure. The event and constructors were promoted from doc-hidden adapter support by the round-6 hardening pass.

- [x] **Step 4: Run core tests and verify GREEN**

Run: `cargo test -p edgezero-core`

Expected: PASS.

### Task 2: Migrate all adapter drivers

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/tests/sdk_resources.rs`

- [x] **Step 1: Update adapter contract tests first**

Assert that ordinary batches finish as `Completed`, deadline/cutoff cases finish as `Cutoff`, and batch iteration handles `Item` then `Finished` through `Result` without treating errors as cutoff.

- [x] **Step 2: Run adapter contract tests and verify RED**

Run the four native contract suites through `scripts/run_test_nonzero.sh` using their existing batch sentinels.

Expected: FAIL until adapters emit explicit driver events.

- [x] **Step 3: Emit explicit item and cutoff events**

For Axum, Cloudflare, and Spin, map every completed exchange to an item event and every `finish_batch_item` cutoff decision to a cutoff event. For Fastly, also map initial expiry, dispatch-loop expiry, and selected-body cutoff to a cutoff event; map provider-selection invariant failure to a failure event carrying its precise error. Driver EOF is reserved for full completion only.

- [x] **Step 4: Run adapter tests and WASM checks**

Run:

```sh
cargo test -p edgezero-adapter-axum --features test-utils
cargo test -p edgezero-adapter-cloudflare --features test-utils
cargo test -p edgezero-adapter-fastly --features test-utils
cargo test -p edgezero-adapter-spin --features test-utils
cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Expected: PASS.

### Task 3: Migrate consumers and hard-cut guards

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: adapter request/service tests that call `send_all_until`
- Modify: `scripts/check_outbound_legacy_api.sh`

- [x] **Step 1: Add failing demo and generator assertions**

Require checked-in and generated handlers to choose explicitly whether to inspect `OutboundBatchFailure.slots` or discard them while propagating `failure.error`, inspect the typed termination reason, and avoid the removed direct-result API shape.

- [x] **Step 2: Run focused consumer tests and verify RED**

Run:

```sh
cargo test -p edgezero-cli generator -- --nocapture
cargo test --manifest-path examples/app-demo/Cargo.toml --locked
```

Expected: FAIL until consumers use the new result and termination types.

- [x] **Step 3: Migrate all callers and test clients**

Update demo/template handlers, mocks, and adapter tests. Remove the old `OutboundBatch::from_stream` constructor and reject stale `while let Some(item) = batch.next().await` and direct `.send_all_until(...).await.slots` shapes in the legacy checker.

- [x] **Step 4: Run focused consumer tests and verify GREEN**

Run the generator and app-demo commands from Step 2 plus `bash scripts/check_outbound_legacy_api.sh`.

Expected: PASS.

### Task 4: Update the normative contract and public guides

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] **Step 1: Update specification API and invariants**

Document explicit `Completed`/`Cutoff` termination, `OutboundBatchFailure`, the successful-result rule that `None` is legal only for `Cutoff`, retained partial slots on failure, and explicit adapter cutoff/failure signaling.

- [x] **Step 2: Update guides and examples**

Show `OutboundBatchNext::Item`/`Finished` iteration and explicit handling of `send_all_until(...).await` as `Result<OutboundBatchResults, OutboundBatchFailure>`. State that malformed drivers never degrade into timeout-like unresolved slots and that ordered collection preserves slots observed before failure.

- [x] **Step 3: Extend documentation regression checks**

Reject stale claims that `next()` returns `Option`, that `send_all_until` directly returns results, or that every missing slot means cutoff without consulting termination.

- [x] **Step 4: Run documentation checks**

Run:

```sh
node scripts/check_outbound_docs_contract.mjs
cd docs && npm run format && npm run lint && npm run build
```

Expected: PASS.

### Task 5: Whole-feature verification and delivery

**Files:**
- Review: all files changed by Tasks 1-4

- [x] **Step 1: Run formatting and strict native gates**

Run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
```

Expected: PASS with zero warnings.

- [x] **Step 2: Run the integrated provider and generated-project suite**

Run: `scripts/run_tests.sh`

Expected: PASS, including Fastly Viceroy, provider WASM builds, generated workspace, app-demo, and documentation contracts.

- [x] **Step 3: Review the final diff**

Run: `git diff --check`, search for removed API shapes, and confirm only intended files changed.

- [x] **Step 4: Commit and push PR 275**

Commit message: `fix(outbound): type batch termination`

Update the PR description and wait for hosted checks to complete.
