# Outbound HTTP Consumer Alignment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the portable outbound contract needed by completion-driven, deadline-bound applications while preserving EdgeZero ownership of provider transport and Fastly response egress.

**Architecture:** Replace the terminal-vector-only batch trait method with an adapter-owned `OutboundBatch` that yields indexed buffered completions until an absolute cutoff. Keep ordered collection as a core convenience over that one driver, add portable cache-bypass and validated wire-authority policy to `OutboundRequest`, and add a closed Fastly lifecycle runner whose hooks cannot take over response transmission.

**Tech Stack:** Rust 2024, `async-trait`, `futures-util`, `web-time`, Fastly SDK, Cloudflare `worker`, Spin WASI HTTP, Axum/reqwest, Cargo contract tests, Node documentation checks.

---

### Task 1: Lock the revised public contract in the specification

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/guide/capabilities.md`

- [x] **Step 1: Replace the terminal-vector-only batching decision**

Document this hard-cut API and remove normative statements that call `send_all` the only fan-out primitive:

```rust
pub struct OutboundBatchItem {
    pub index: usize,
    pub result: OutboundSlotResult,
}

pub struct OutboundBatchResults {
    pub slots: Vec<Option<OutboundSlotResult>>,
}

pub trait OutboundHttpClient: Send + Sync {
    async fn send(&self, request: OutboundRequest)
        -> Result<OutboundResponse, EdgeError>;

    fn start_batch_until(
        &self,
        requests: Vec<OutboundRequest>,
        cutoff: Deadline,
    ) -> OutboundBatch;
}
```

Specify that `OutboundBatch::next()` is cancellation-safe and yields each terminal slot at most once in observed completion order. The cutoff wins equality. Dropping or cancelling stops observation and applies the adapter's strongest available teardown without claiming that already-issued network side effects are undone.

- [x] **Step 2: Specify ordered collection as one-driver convenience**

Document `HttpClient::send_all_until` as `start_batch_until(...).collect().await`, with `slots.len() == requests.len()`, preflight failures as `Some(Err(_))`, and only unresolved-at-cutoff slots as `None`. Remove the old `send_all` API instead of retaining a compatibility alias.

- [x] **Step 3: Specify request transport policy**

Add `OutboundCachePolicy::{PlatformDefault, Bypass}` and a validated `host_authority_override`. State that the URI continues to own connection destination, SNI, and certificate identity; the override controls only the outgoing HTTP authority. Document Spin as `Unsupported` for authority separation and require preflight rejection rather than silent semantic drift.

- [x] **Step 4: Specify the closed Fastly lifecycle runner**

Define ordering: received request -> raw request mutation/extension injection -> EdgeZero conversion and ingress -> route -> response mutation -> egress policy/framing -> platform send -> return post-send state. Hooks borrow values and cannot send, stream, or consume the response themselves.

- [x] **Step 5: Run documentation checks**

Run: `node scripts/check_outbound_docs_contract.mjs`

Expected: PASS with no stale normative `send_all`-only or canonical-authority contradiction.

### Task 2: Add the core completion-driven batch API

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/time.rs`

- [x] **Step 1: Write failing core contract tests**

Add tests proving empty batches, reverse completion order, original indices, exact-once delivery, cancellation-safe `next()`, explicit cancellation, preflight result delivery, cutoff equality, ordered `None` collection, one shared start sample, and backwards-clock handling.

- [x] **Step 2: Run the focused tests and verify RED**

Run: `cargo test -p edgezero-core outbound::tests::batch -- --nocapture`

Expected: FAIL because `OutboundBatch`, `start_batch_until`, and `send_all_until` do not exist.

- [x] **Step 3: Implement the opaque batch driver**

Add `OutboundBatch`, `OutboundBatchItem`, and `OutboundBatchResults`. Keep adapter internals opaque through a hidden driver boundary. `next()` must not remove a slot until it returns the item, and `cancel()` must return unresolved indices synchronously while dropping driver-owned work.

- [x] **Step 4: Hard-cut the trait and handle**

Remove `OutboundHttpClient::send_all` and `HttpClient::send_all`. Add `start_batch_until` to both, plus `HttpClient::send_all_until` implemented only by collecting the returned batch.

- [x] **Step 5: Extend dispatch-budget selection**

Pass the batch cutoff into budget selection as a third candidate without mutating each request. Hard-cut `BudgetSource::BatchDeadline` to `RequestDeadline`, add `BatchCutoff`, and preserve exact attribution to the tightest source.

- [x] **Step 6: Run core tests and verify GREEN**

Run: `cargo test -p edgezero-core`

Expected: PASS.

### Task 3: Add cache and wire-authority request policy

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/error.rs`

- [x] **Step 1: Write failing policy tests**

Test default cache policy, bypass round-tripping through parts, valid DNS/IPv4/bracketed-IPv6 authority overrides, malformed/userinfo/path/control rejection, raw `Host` stripping, wire-authority selection, and continued URI-derived backend/SNI/certificate identity.

- [x] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p edgezero-core outbound::tests::outbound_request_policy -- --nocapture`

Expected: FAIL because the policy fields and builders do not exist.

- [x] **Step 3: Implement the typed fields and builders**

Add:

```rust
pub enum OutboundCachePolicy {
    PlatformDefault,
    Bypass,
}

pub fn cache_policy(self, policy: OutboundCachePolicy) -> Self;
pub fn host_authority_override(self, authority: &str) -> Result<Self, EdgeError>;
pub fn host_authority(&self) -> &str;
```

Store a parsed authority, not an unchecked string. Preserve it through `into_parts` and `from_parts`; keep `backend_target`, `sni_hostname`, and `cert_host` URI-derived.

- [x] **Step 4: Run core tests and verify GREEN**

Run: `cargo test -p edgezero-core`

Expected: PASS.

### Task 4: Implement Axum batch, cache, and authority behavior

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`

- [x] **Step 1: Write failing Axum contract tests**

Cover completion order, cutoff with recoverable completed slots, drop cancellation, Host override observed by a local server while TLS/connection target stays URI-derived, and cache bypass not synthesizing origin-visible cache headers.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p edgezero-adapter-axum outbound -- --nocapture`

Expected: FAIL against the removed trait method and missing policy conversion.

- [x] **Step 3: Implement a private `FuturesUnordered` batch driver**

Prepare every slot from one start sample, enqueue immediate preflight failures, drive complete buffered exchanges, race against the absolute cutoff, and drop unresolved reqwest futures on cancellation.

- [x] **Step 4: Apply wire authority and cache semantics**

Set the final `Host` from the validated authority field. Treat cache bypass as satisfied without adding `Cache-Control` because reqwest has no adapter-managed intermediary cache.

- [x] **Step 5: Run Axum tests and workspace tests**

Run: `cargo test -p edgezero-adapter-axum && cargo test --workspace --all-targets`

Expected: PASS.

### Task 5: Implement Cloudflare batch, cache, and authority behavior

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `examples/app-demo/wrangler.toml`
- Modify: `examples/app-demo/wrangler.ci.toml`

- [x] **Step 1: Write failing native and WASM contract tests**

Cover completion-order driving, cutoff cancellation through `AbortController`, buffered-only preflight, `CacheMode::NoStore`, duplicate headers, and validated Host override conversion. Keep deployed Host observation as the evidence gate before claiming Native authority separation.

- [x] **Step 2: Run available tests and verify RED**

Run: `cargo test -p edgezero-adapter-cloudflare --features test-utils`

Expected: FAIL until the new batch and request policy are mapped.

- [x] **Step 3: Implement the Cloudflare driver and mappings**

Use completion-driven futures with one abort guard per slot. Set `NoStore` only for `Bypass`; append the validated Host authority and continue manual redirect handling. Keep authority separation `BestEffort` until the host-observed probe proves the runtime retained the override.

- [x] **Step 4: Run Cloudflare checks**

Run: `cargo test -p edgezero-adapter-cloudflare --features test-utils`

Run: `cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare`

Expected: PASS.

### Task 6: Implement Spin batch and fail-closed authority behavior

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Write failing Spin contract tests**

Cover completion order, cutoff, owned exchange teardown, full-budget timers, cache bypass without synthetic headers, and explicit Host override rejection before body polling or provider dispatch.

- [x] **Step 2: Run native tests and verify RED**

Run: `cargo test -p edgezero-adapter-spin --features test-utils`

Expected: FAIL until the driver and authority preflight exist.

- [x] **Step 3: Implement the Spin driver and mappings**

Drive complete exchange futures under the batch cutoff. Keep cancellation `BestEffort`. Treat cache bypass as satisfied without request-header mutation. Reject authority override because WASI HTTP's authority is also the connection target.

- [x] **Step 4: Run Spin tests and WASM check**

Run: `cargo test -p edgezero-adapter-spin --features test-utils`

Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`

Expected: PASS.

### Task 7: Implement Fastly indexed completion and transport policy

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [x] **Step 1: Write failing target-neutral orchestration tests**

Cover dispatch-before-select, duplicate targets retaining unique indices, reverse completion order, preflight failures, cutoff equality, explicit cancellation, heterogeneous backend identities, Host override identity separation, and cache bypass on both single and batch paths.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p edgezero-adapter-fastly --features test-utils outbound -- --nocapture`

Expected: FAIL because the existing orchestrator returns only an ordered terminal vector.

- [x] **Step 3: Replace terminal-vector harvest with an indexed select driver**

Dispatch every eligible request before blocking. Use Fastly pending-handle selection to associate the selected handle with its original index. Preserve documented sequential dispatch, bounded selection-group, and synchronous selected-body-drain limitations. Stop observation at cutoff and classify cancellation/slot isolation as `BestEffort`.

- [x] **Step 4: Apply Fastly request policy**

Use the wire authority for `.override_host`, keep backend target/SNI/certificate URI-derived, include wire authority and relevant budget in backend identity, and call `set_pass(true)` for cache bypass.

- [x] **Step 5: Run Fastly tests and WASM check**

Run: `cargo test -p edgezero-adapter-fastly --features test-utils`

Run: `cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly`

Expected: PASS.

### Task 8: Add the closed Fastly request/response lifecycle runner

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `docs/guide/adapters/fastly.md`

- [x] **Step 1: Write failing lifecycle tests**

Prove request mutation precedes conversion, injected extensions reach middleware, response mutation precedes egress policy/framing, invalid framing fails closed, streaming stays adapter-owned, transport send happens exactly once, detached ingress failures skip the routed-response hook, and post-send state is returned only after terminal egress observation.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p edgezero-adapter-fastly request -- --nocapture`

Expected: FAIL because only the auto-receiving, request-extension-only runner exists.

- [x] **Step 3: Implement the lower-level closed runner**

Add a `FastlyService` method that accepts an already-received request, a borrow-only request preparation hook, and a borrow-only response finalization hook that may return post-send state. Keep conversion, ingress, response-egress begin/finish, stream ownership, and platform delivery private.

- [x] **Step 4: Build the top-level runner over it and remove the superseded API**

Keep `run_app` as the simple generated entrypoint. Replace `run_app_with_request_extensions` with one `run_app_with_hooks` helper built over the closed runner; do not keep the request-extension-only compatibility API.

- [x] **Step 5: Run Fastly tests**

Run: `cargo test -p edgezero-adapter-fastly`

Expected: PASS.

### Task 9: Update capabilities, documentation, demos, and generators

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/lib.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/adapters/overview.md`
- Modify: `docs/guide/adapters/{axum,cloudflare,fastly,spin}.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `scripts/check_outbound_legacy_api.sh`

- [x] **Step 1: Write failing capability and generator tests**

Add capabilities for completion-driven batching, pending cancellation, cache bypass, and wire-authority override. Assert each adapter's exact `Native`/`BestEffort`/`Unsupported` row. Assert generated/demo code uses the hard-cut API and no removed symbol remains.

Use these rows:

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `outbound-batch-completion-order` | Native | Native | BestEffort | Native |
| `outbound-batch-cancellation` | Native | BestEffort | BestEffort | BestEffort |
| `outbound-batch-slot-isolation` | Native | Native | BestEffort | Native |
| `outbound-cache-bypass` | Native | Native | Native | Native |
| `outbound-authority-override` | Native | BestEffort | Native | Unsupported |

Remove the old `send-all-slot-isolation` capability rather than retaining an alias.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p edgezero-core manifest -- --nocapture`

Run: `cargo test -p edgezero-cli generator -- --nocapture`

Expected: FAIL until capability metadata and generated source are migrated.

- [x] **Step 3: Implement metadata and migrate consumers**

Update adapter capability tables, manifest parsing/rendering, public guides, example handlers, and scaffold assertions. Extend the legacy checker to reject `send_all(` and direct Fastly response transmission in generated entrypoints.

- [x] **Step 4: Run focused verification**

Run: `cargo test -p edgezero-core && cargo test -p edgezero-cli`

Run: `node scripts/check_outbound_docs_contract.mjs`

Run: `bash scripts/check_outbound_legacy_api.sh`

Expected: PASS.

### Task 10: Complete whole-feature verification and cleanup

**Files:**
- Review: all files changed by Tasks 1-9

- [x] **Step 1: Remove superseded code**

Search for the old trait method, ordered Fastly batch helper, stale cache/Host behavior, and direct response-send bypasses. Remove helpers that no longer have callers rather than leaving compatibility code.

- [x] **Step 2: Run formatting and full native gates**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Run: `cargo test --workspace --all-targets`

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`

Expected: PASS with zero warnings.

- [x] **Step 3: Run target and demo gates**

Run: `cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly`

Run: `cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare`

Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`

Run: `cargo test --manifest-path examples/app-demo/Cargo.toml --locked`

Expected: PASS.

- [x] **Step 4: Run repository contract gates**

Run: `node scripts/check_outbound_docs_contract.mjs`

Run: `bash scripts/check_outbound_legacy_api.sh`

Run: `node scripts/check_brotli_dependency_contract.mjs`

Run: `bash scripts/check_no_legacy_typed_reads.sh`

Expected: PASS.

- [x] **Step 5: Review the final diff**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only intended files changed.
