# Detached Egress and Config Deadline Closure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cover normalized pre-admission responses with application-owned completion resources and ensure asynchronous bounded config reads return at their absolute deadline without overstating provider cancellation.

**Architecture:** Core gains one default-empty detached-egress completion factory keyed by bounded failure metadata. Bounded store methods become required hard-cut trait methods; Cloudflare and Spin race asynchronous provider futures against native timers, while Axum and Fastly retain accurately documented synchronous limitations. Application permits remain application-defined, while a generic core late-bound owner centralizes the response-lifecycle synchronization needed to retain any `Send` resource until terminal egress or abandonment.

**Tech Stack:** Rust 2024, `async-trait`, `futures-util`, Tokio, Cloudflare Workers `Delay`, Spin SDK `sleep`, existing EdgeZero ingress/egress and capability APIs.

---

### Task 1: Add detached normalized-rejection completion ownership

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Test: `crates/edgezero-core/src/app.rs`

- [x] **Step 1: Write failing core lifecycle tests**

  Add tests proving a normalized rejection calls the configured factory once with method,
  application-clock request start, status, and stable error kind; invokes the returned completion
  exactly once at terminal egress; leaves policy/observer untouched; and drops a captured resource
  without a fabricated report when the envelope is abandoned before `begin()`.

- [x] **Step 2: Run the focused tests and verify RED**

  Run: `cargo test -p edgezero-core --lib app::tests::detached_ingress`

  Expected: compile failure because the detached head and setter do not exist.

- [x] **Step 3: Implement the factory**

  Add `DetachedResponseEgressHead<'_>` with read-only accessors for request method, request start,
  status, and `EdgeError::kind()`. Store an `Arc<dyn for<'head> Fn(&DetachedResponseEgressHead<'head>) -> ResponseEgressCompletion + Send + Sync>` in `App`, default it to an empty completion, expose a generic setter, and call it exactly once from
  `detached_ingress_error_egress` before consuming the error into its response.

- [x] **Step 4: Run core tests**

  Run: `cargo test -p edgezero-core --lib app::tests::detached_ingress`

  Expected: all detached lifecycle cases pass.

### Task 2: Prove detached completion in every adapter

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Strengthen existing normalized-ingress rejection tests**

  Configure a detached completion factory with a drop/completion counter, trigger normalized head
  rejection, and prove each adapter settles one completion after delivery while admission,
  middleware, handlers, policy, observer, and body polling remain untouched.

- [x] **Step 2: Run or compile each adapter contract under its supported target**

  Run the Axum `normalized_ingress_error` and admission-policy-error filters with a required
  nonzero test count. Cloudflare, Fastly, and Spin ingress contracts are target-only, so compile
  those tests under strict target Clippy; local runtime execution remains dependent on the pinned
  provider runners used by hosted CI.

### Task 3: Hard-cut bounded store implementations

**Files:**
- Modify: `crates/edgezero-core/src/config_store.rs`
- Modify: `crates/edgezero-core/src/secret_store.rs`
- Modify: core and adapter test fixtures implementing either trait

- [x] **Step 1: Remove both cooperative defaults**

  Make `ConfigStore::get_bounded` and `SecretStore::get_bytes_bounded` required. Keep unbounded
  convenience reads separate and explicitly outside extraction guarantees.

- [x] **Step 2: Run workspace check and record expected compile failures**

  Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`

  Expected: every fixture relying on the old default fails with a missing required method.

- [x] **Step 3: Migrate fixtures explicitly**

  Give deterministic in-memory fixtures finite pre/post checks and exact byte accounting. Do not
  restore a blanket compatibility helper that can hide a pending provider future.

- [x] **Step 4: Run core store and extractor tests**

  Run: `cargo test -p edgezero-core --lib config_store` and
  `cargo test -p edgezero-core --lib extractor`.

### Task 4: Race Cloudflare and Spin asynchronous reads

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/config_store.rs`
- Modify: `crates/edgezero-adapter-spin/src/config_store.rs`
- Modify: `crates/edgezero-adapter-spin/src/secret_store.rs`
- Test: colocated unit tests in those modules

- [x] **Step 1: Write never-ready and equality-precedence tests**

  Inject a pending provider future and a controlled timer. Prove a clock-confirmed timer completion
  returns the typed deadline error, drops the provider future, retains no partial value, and wins
  when both sides are ready at equality. Prove an early timer wake re-arms without discarding the
  provider result. Repeat for Spin secret reads because typed config extraction shares the same
  deadline.

- [x] **Step 2: Run focused tests and verify they hang/fail under the old await-only helper**

  Use injected immediately-ready timers so the red test itself remains bounded.

- [x] **Step 3: Implement native timer races**

  Compute remaining time from the injected application clock before provider polling. Cloudflare
  uses `worker::Delay`; Spin uses `spin_sdk::time::sleep`. Poll the timer first for equality
  precedence, drop the losing provider future, then retain the existing post-ready deadline and
  byte-cap checks. Native test-only backends use an injected timer helper rather than wall sleeps.

- [x] **Step 4: Run native and WASM checks**

  Run the affected native tests plus Cloudflare `wasm32-unknown-unknown` and Spin `wasm32-wasip2`
  max-feature Clippy commands.

### Task 5: Document late binding and align consumer surfaces

**Files:**
- Modify: `docs/guide/handlers.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`
- Modify: `docs/superpowers/specs/2026-06-16-blob-app-config.md`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Test: demo lifecycle tests and generated-project compile test

- [x] **Step 1: Add the late-bound shared one-shot example**

  Show one handle travelling through `IngressGrant` and one through
  `ResponseEgressCompletion`; the handler installs the acquired permit and terminal completion
  takes/releases it. State explicitly that handler-local capture releases too early and that
  install-after-terminal must fail closed.

- [x] **Step 2: Align capability language**

  State that Cloudflare/Spin timer races bound EdgeZero's return, not provider cancellation;
  Fastly remains non-preemptible; no adapter satisfies a Native cancellation requirement.

- [x] **Step 3: Keep demo and generator on the public hard-cut API**

  Configure the detached factory and add lifecycle assertions without pretending the illustrative
  lease is a production semaphore. Regenerate/compile the scaffold to prove template parity.

### Task 6: Replace application-owned permit slots with a reusable core primitive

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `docs/guide/handlers.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`

- [x] **Step 1: Write failing core state-machine tests**

  Specify `ResponseEgressCompletion::late_bound::<T>()` returning one non-clone
  `ResponseEgressResource<T>` installer plus its terminal completion. Prove one installation lives
  until terminal egress; install-before-terminal and terminal-before-install ordering; duplicate
  installation; completion abandonment before and after installation while the installer remains;
  and competing terminal attempts. Require duplicate and closed installations to drop the rejected
  resource outside the mutex and return distinct typed errors, terminal/abandonment to release an
  installed resource exactly once, and envelope abandonment to fabricate no completion or observer
  report. Add a compile-fail doctest proving the installer is non-clone and a type assertion proving
  it is `Send + Sync` for `T: Send` even when `T` is not `Sync`. Race installation against close,
  verify poisoned-lock recovery, and require the typed install failure to implement
  `std::error::Error`.

- [x] **Step 2: Run the focused tests and verify RED**

  Run: `cargo test -p edgezero-core --lib response_egress::tests::late_bound`

  Expected: compile failure because the reusable constructor, resource, and install error do not
  exist.

- [x] **Step 3: Implement the generic ownership primitive**

  Add `ResponseEgressResource<T>` backed by one private shared mutex state and
  `ResponseEgressResourceInstallError::{AlreadyInstalled, Closed}`. Its public
  `install(&self, resource: T)` method consumes the candidate and never returns it. Require
  `T: Send + 'static`. Capture a private completion-side owner in the callback; both callback
  execution and owner `Drop` close the slot and take the installed resource while the installer may
  remain alive. Always release the mutex before dropping application resources. Keep terminal
  callback execution in the existing `ResponseEgressCompletion` exactly-once boundary.

- [x] **Step 4: Run focused core tests and strict core Clippy**

  Run: `cargo test -p edgezero-core --lib response_egress::tests::late_bound` and
  `cargo test -p edgezero-core --doc response_egress` and
  `cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings`.

- [x] **Step 5: Make generator assertions reject the local state machine and verify RED**

  Require `ResponseEgressCompletion::late_bound` and `ResponseEgressResource` in generated core
  source, and reject `ResponsePermitSlot`, `ResponsePermitState`, and an application-owned mutex
  lifecycle implementation. Run generator tests and verify the existing template fails.

- [x] **Step 6: Hard-cut demo and generator to the core primitive**

  Delete their local mutex state machine. Keep only the illustrative application permit,
  admission lease metadata, and one `install(permit)` call. Retain one integration lifecycle test
  in each generated/demo surface; move duplicate/post-terminal/abandonment proofs to core.

- [x] **Step 7: Replace documentation examples**

  Document the public primitive and its typed failure semantics. Make generated-project assertions
  and the normative specification require the core late-bound owner instead of application-owned
  synchronization.

- [x] **Step 8: Run demo, generator, and generated-project gates**

  Run demo tests and strict Clippy, generator unit tests, and the ignored generated workspace gate.

### Task 7: Full verification

**Files:**
- Verify all modified files; do not alter unrelated worktree changes.

- [x] **Step 1: Run repository gates**

  Run workspace tests, strict Clippy, formatting, strict rustdoc, feature compilation, docs/legacy
  contract scripts, app-demo tests/Clippy, generated workspace compilation, and all supported WASM
  max-feature Clippy checks.

- [x] **Step 2: Review the final diff**

  Confirm normalized errors own one completion, pending async config reads return typed deadline
  errors, capability claims remain conservative, and the pre-existing rustdoc/CI edits are
  preserved.
