# Outbound HTTP Round 6 Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the actionable round-6 outbound review findings without changing valid monotonic-cutoff semantics.

**Architecture:** Keep provider orchestration in each adapter, but move the byte-for-byte duplicated terminal-slot classifier into core as documented adapter support. Preserve the concurrent adapters' fail-closed cutoff rule because their shared monotonic completion samples are taken synchronously in poll order; fix Fastly response framing by carrying core's body-transmission disposition through the irreversible commit boundary. Promote the batch-driver construction protocol that generated applications already consume, and align all normative/public documentation with pre-normalization header accounting.

**Tech Stack:** Rust 2024, `futures`/`futures-util`, Fastly Compute ABI, Cargo workspace tests, Node.js documentation contract checker.

---

### Task 1: Centralize and harden batch-driver invariants

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`

- [x] **Step 1: Add red core tests for the shared terminal-slot classifier**

Add tests covering an on-time outcome, an observation at cutoff, an attributed `BatchCutoff` timeout, and a terminal sample before the batch start. The first three pin `Some` versus `None`; the backward sample pins zero elapsed plus the existing internal invariant outcome.

- [x] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test --offline --locked -p edgezero-core --lib finish_batch_item_
```

Expected: compilation fails because the shared core helper does not exist.

- [x] **Step 3: Move the duplicated helper into core and update all adapters**

Implement one documented `edgezero_core::outbound::finish_batch_item` adapter-support function using the existing behavior. Delete all four adapter-local copies and their now-unused imports. Do not add completion-group buffering or alter cutoff ordering.

- [x] **Step 4: Run affected core and adapter tests and verify GREEN**

Run:

```bash
cargo test --offline --locked -p edgezero-core --lib finish_batch_item_
cargo test --offline --locked -p edgezero-adapter-axum --lib
cargo test --offline --locked -p edgezero-adapter-fastly --lib
cargo check --offline --locked -p edgezero-adapter-cloudflare --tests --target wasm32-unknown-unknown --features cloudflare
cargo check --offline --locked -p edgezero-adapter-fastly --tests --target wasm32-wasip1 --features fastly
cargo check --offline --locked -p edgezero-adapter-spin --tests --target wasm32-wasip2 --features spin
```

Expected: all pass.

- [x] **Step 5: Strengthen batch invariant tests before changing API documentation**

Add exact-message assertions for premature EOF, duplicate index, and out-of-range index. Add a direct `OutboundBatch::cutoff` test and a test proving a second `next()` after an invariant failure returns the poisoned re-poll diagnostic.

- [x] **Step 6: Run the invariant characterization tests**

Run:

```bash
cargo test --offline --locked -p edgezero-core --lib batch_collection_
cargo test --offline --locked -p edgezero-core --lib batch_cutoff_
cargo test --offline --locked -p edgezero-core --lib batch_poisoned_
```

Expected: the exact diagnostics and poison/cutoff state behavior already pass. These tests pin
existing invariants before the public documentation/export change; they are characterization
coverage, not a behavior change.

- [x] **Step 7: Add a red public root-export consumer check**

Update the generated template and app-demo fixture to import
`edgezero_core::OutboundBatchDriverEvent` from the crate root, then run:

```bash
cargo test --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-core --no-run
scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds
```

Expected: unresolved import because the root export does not exist yet.

- [x] **Step 8: Promote the real batch-driver construction API**

Remove `#[doc(hidden)]` from `OutboundBatchDriverEvent`, `OutboundBatch::from_driver`, and `OutboundBatch::cutoff`; document event ordering, EOF, failure, cutoff, and index invariants. Re-export `OutboundBatchDriverEvent` from the core crate root and update generated/demo test clients to use the root export. Keep one API only; add no compatibility alias.

- [x] **Step 9: Run core, CLI, and demo checks**

Run:

```bash
cargo test --offline --locked -p edgezero-core --lib batch_
cargo test --offline --locked -p edgezero-cli
cargo test --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-core
scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds
```

Expected: all pass and generator assertions match the public API.

### Task 2: Prevent manual framing for Fastly body-suppressed responses

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`

- [x] **Step 1: Add red HEAD and 304 framing tests**

Extend the mock committer's prepared value to record `transmits_body`. Assert that HEAD and 304 responses retain valid representation `Content-Length`, transmit no body, and pass `false` through the committer boundary. Add a payload response assertion that passes `true`.

- [x] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test --offline --locked -p edgezero-adapter-fastly --lib body_suppressed_response_disables_manual_framing
cargo test --offline --locked -p edgezero-adapter-fastly --lib payload_response_allows_manual_framing
```

Expected: compilation/assertion failure because `EgressCommitter::prepare` does not receive the disposition.

- [x] **Step 3: Thread the body disposition through Fastly preparation**

Read `PreparedResponseEgress::transmits_body()` before consuming it, pass the boolean through `EgressCommitter::prepare`, and call `set_manual_framing` only when the response transmits a body and contains `Content-Length`. Update the raw-ABI happy-path test call site. Do not change core's HEAD/304 metadata preservation.

- [x] **Step 4: Run Fastly native and WASM tests**

Run:

```bash
cargo test --offline --locked -p edgezero-adapter-fastly --lib
(cd crates/edgezero-adapter-fastly && cargo test --offline --locked --target wasm32-wasip1 --features fastly --lib)
```

Expected: all pass.

### Task 3: Align the contract and documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/superpowers/plans/2026-09-06-outbound-http-phase2-body-response-limits.md`
- Modify: `docs/superpowers/plans/2026-09-06-outbound-http-phase4-axum-cloudflare.md`
- Modify: `docs/superpowers/plans/2026-09-06-outbound-http-phase5-spin.md`
- Modify: `docs/superpowers/plans/2026-09-06-outbound-http-phase6-fastly.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] **Step 1: Add red documentation-contract expectations**

Change the checker to require wording that response-header caps count adapter-visible upstream fields before normalization, including fields later stripped, plus EdgeZero's synthetic proxy field. Require the normative concurrent-driver statement that completion samples are taken synchronously in observation order from one monotonic clock.

- [x] **Step 2: Run the docs checker and verify RED**

Run:

```bash
node scripts/check_outbound_docs_contract.mjs
```

Expected: failure because the current guide/spec still say `guest-visible` and omit the monotonic ordering statement.

- [x] **Step 3: Update normative spec, public guides, and phase-plan terminology**

Replace misleading `guest-visible header` wording with the exact pre-normalization accounting boundary. State that stripped hop-by-hop fields consume budget and the synthetic `x-edgezero-proxy` field is charged afterward. Document why the concurrent adapters may terminate on their first cutoff observation without discarding an earlier terminal sample, while retaining Fastly's separate BestEffort limitation. Keep proxying's `None` meaning unchanged.

- [x] **Step 4: Run documentation validation**

Run:

```bash
node scripts/check_outbound_docs_contract.mjs
npm --prefix docs run lint
npm --prefix docs run format
```

Expected: all pass.

### Task 4: Verify the complete change and update the PR

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/fastly_response_abi.rs`
- Modify: `docs/superpowers/plans/2026-09-17-outbound-http-round6-hardening.md`

- [x] **Step 1: Audit the Fastly raw-ABI test boundary**

Confirm the existing mock suite covers post-commit abandon, write/source/deadline failures, and finish consumption. Add only target-valid status-result coverage if it exercises the real shared mapper. Do not add an ABI indirection layer solely to synthesize provider hostcall failures; record Viceroy fault-injection limits in the final review note.

Audit result: the portable mock suite covers those lifecycle branches, and the real shared status
mapper now has a `wasm32-wasip1`/Viceroy test for success and host failure. Viceroy does not expose
fault injection for each downstream response hostcall, so provider-hostcall failure coverage stops
at that mapper rather than adding a production ABI indirection solely for tests.

- [x] **Step 2: Mark implementation/documentation tasks complete in this plan**

Update completed checkboxes before final validation so the checked tree is the tree that will be
committed. Leave only the final gate/review/commit steps unchecked while commands are running.

- [x] **Step 3: Run every required repository gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
bash scripts/run_tests.sh
```

Expected: all gates pass.

- [x] **Step 4: Review the final diff**

Verify there are no adapter-local `finish_batch_item` definitions, no hidden public driver constructors used by scaffolds, no unconditional Fastly manual framing, and no stale `guest-visible` header-cap claims.

Independent runtime review found no code defect. Independent contract review found and closed four
documentation/API-hardening gaps: the normative spec now includes the public driver protocol and
shared helper, the historical termination plan no longer calls that protocol doc-hidden, Phase 4/5
mark their removed `send_all` APIs as superseded, and the public driver event is non-exhaustive.

- [x] **Step 5: Mark verification complete and validate the final documentation state**

Mark the gate/review checkboxes complete, then run:

```bash
git diff --check
node scripts/check_outbound_docs_contract.mjs
npm --prefix docs run format
```

Expected: all pass after the final plan-file edits.

- [x] **Step 6: Commit and push**

Commit the implementation and documentation together with a focused message, push `docs/outbound-http-spec`, and confirm PR 275 checks start against the pushed SHA.
