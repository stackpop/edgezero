# Outbound Batch Driver Failure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve precise adapter-driver failures and all terminal batch slots observed before the failure.

**Architecture:** Extend the private adapter-to-core driver protocol with an explicit failure event. Incremental callers receive its `EdgeError` directly; ordered collection wraps the error with the index-aligned slots collected so far in a new public `OutboundBatchFailure`. Fastly emits this event when pending-handle selection or metadata reassociation violates its invariant, instead of silently ending its stream or misreporting a deadline cutoff.

**Tech Stack:** Rust 2024, `futures` streams, Fastly Compute SDK, Cargo native/WASM tests, VitePress documentation checks.

---

### Task 1: Preserve Partial Results on Driver Failure

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [x] **Step 1: Write failing core tests**

Add tests proving that:

```rust
let failure = block_on(batch.collect()).expect_err("driver failure");
assert!(matches!(failure.error, EdgeError::Internal { .. }));
assert!(failure.slots[completed_index].is_some());
assert!(failure.slots[unresolved_index].is_none());
```

Cover both an explicit driver failure carrying a distinctive diagnostic and premature EOF after one item.

- [x] **Step 2: Run the focused tests and verify RED**

Run: `cargo test -p edgezero-core outbound::tests::batch_collection -- --nocapture`

Expected: compilation fails because `OutboundBatchDriverEvent::Failed` and `OutboundBatchFailure` do not exist.

- [x] **Step 3: Implement the minimal core contract**

Add:

```rust
#[derive(Debug)]
#[non_exhaustive]
pub struct OutboundBatchFailure {
    pub error: EdgeError,
    pub slots: Vec<Option<OutboundSlotResult>>,
}
```

Add hidden `OutboundBatchDriverEvent::Failed(EdgeError)`. Make `next()` poison the batch and return that exact error. Make `collect()` convert any `next()` error into `OutboundBatchFailure` without discarding its local slot vector. Change `HttpClient::send_all_until` to return `Result<OutboundBatchResults, OutboundBatchFailure>`, export the type, and implement `From<OutboundBatchFailure> for EdgeError` so existing handlers can deliberately discard partials with `?`.

- [x] **Step 4: Run core tests and verify GREEN**

Run: `cargo test -p edgezero-core`

Expected: all core tests pass.

### Task 2: Propagate Fastly Selection Failures

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`

- [x] **Step 1: Write failing Fastly tests**

Add target-neutral tests for all three reassociation failures:

- invalid selected handle index;
- unknown or duplicate pending handle;
- omitted pending handle.

Assert their exact internal diagnostic and add a driver-level regression proving a previously emitted item remains available in `OutboundBatchFailure` when the Fastly failure event follows it.

- [x] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p edgezero-adapter-fastly --features test-utils selection`

Expected: the regression fails because the harvest path ends with EOF instead of emitting the precise error.

- [x] **Step 3: Emit the explicit failure event**

Replace the `let Ok(..) else { return; }` branch with a `match` that yields `OutboundBatchDriverEvent::Failed(error)` and returns. Do not emit `Cutoff`; provider selection failed while the cutoff remained live.

- [x] **Step 4: Run Fastly tests and verify GREEN**

Run: `cargo test -p edgezero-adapter-fastly --features test-utils`

Expected: all Fastly tests pass, including exact diagnostics and partial-result retention.

### Task 3: Align Consumers and Documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/superpowers/plans/2026-09-16-outbound-batch-termination.md`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `scripts/check_outbound_legacy_api.sh`

- [x] **Step 1: Update executable consumers**

Keep demo and generated handlers source-compatible through `From<OutboundBatchFailure> for EdgeError`, but make their fail-closed choice explicit by propagating `failure.error`. Demonstrate partial-slot retention in the public guide and update generator assertions to pin the current contract.

- [x] **Step 2: Update the normative and public contracts**

Document that explicit adapter failures and malformed EOF are not cutoff, incremental callers receive the precise `EdgeError`, and ordered collection returns `OutboundBatchFailure` containing the same error plus all slots collected before failure.

- [x] **Step 3: Harden documentation and legacy checkers**

Require `OutboundBatchFailure` and the new collector signature. Reject stale `Result<OutboundBatchResults, EdgeError>` declarations and claims that batch-level errors erase prior slot results.

- [x] **Step 4: Run documentation and generator checks**

Run: `node scripts/check_outbound_docs_contract.mjs`

Run: `bash scripts/check_outbound_legacy_api.sh`

Run: `cargo test -p edgezero-cli generator`

Expected: all checks pass.

### Task 4: Full Verification and Delivery

**Files:**
- Modify: `docs/superpowers/plans/2026-09-17-outbound-batch-driver-failure.md`

- [x] **Step 1: Run repository verification**

Run: `scripts/run_tests.sh`

Run any target-specific strict WASM Clippy commands required by CI if the integrated script does not cover them.

Expected: formatting, strict Clippy, native tests, all WASM targets, documentation, and contract checks pass.

- [x] **Step 2: Review the final diff**

Confirm no selection failure is represented as `Cutoff`, no old collector signature remains, demo/template output matches, and no unrelated lower-priority review item was included.

- [x] **Step 3: Mark this plan complete, commit, and push**

Commit the focused implementation and synchronized documentation, push `docs/outbound-http-spec`, and confirm PR #275 checks are green.
