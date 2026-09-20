# Response Egress Composition and Detached Deadline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow multiple application-owned response resources to share one terminal event and require every detached normalized-error response to carry an absolute application deadline.

**Architecture:** `ResponseEgressCompletion::join` composes two non-clone completion owners while preserving deterministic callback order and independent panic isolation on unwind-capable targets; panic-abort targets may terminate before the second callback. `DetachedResponseEgressDecision::Send` becomes a named-field variant containing both the completion and a mandatory `Deadline`; `App` inserts that deadline into the existing `ResponseEgressDeadline` extension path before constructing detached egress. The change is a hard cut across core, adapters, the reference app, generated projects, specifications, and contract guards.

**Tech Stack:** Rust 2024, `edgezero-core`, Axum, Cloudflare Workers, Fastly Compute, Fermyon Spin, Cargo test/Clippy, shell and Node.js contract checks, VitePress documentation.

---

### Task 1: Pin the application-neutral contracts

**Files:**
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`
- Modify: `docs/guide/handlers.md`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`

- [x] **Step 1: Specify completion composition**

  Add `ResponseEgressCompletion::join(self, other) -> Self`. Define left-to-right callback order, the same borrowed terminal report for both callbacks, independent panic isolation on unwind-capable targets, at-most-once execution, and release of both captured resources when the joined completion is abandoned. State explicitly that panic-abort targets may terminate before the second callback runs.

- [x] **Step 2: Specify the hard-cut detached decision**

  Replace the tuple variant with:

  ```rust
  #[non_exhaustive]
  pub enum DetachedResponseEgressDecision {
      Abort,
      Send {
          completion: ResponseEgressCompletion,
          deadline: Deadline,
      },
  }
  ```

  State that the deadline is absolute in the request's injected monotonic-clock domain, is mandatory for every normalized-error `Send` decision, and cannot be extended by the default response-egress policy. Admission-selected detached responses retain their existing response-owned deadline path and are outside this factory contract. `Abort` continues to create no response, attempt, completion callback, observer call, handler call, middleware call, or body poll.

- [x] **Step 3: Specify relative deadline construction and default behavior**

  Add `DetachedResponseEgressHead::write_deadline_after(Duration) -> Deadline`, clamped to `DEADLINE_FAR_FUTURE` and failing closed to `request_start` on arithmetic overflow. The default decision sends with an empty completion and `DEFAULT_RESPONSE_WRITE_BUDGET` measured from captured request start.

- [x] **Step 4: Correct public staging terminology**

  Replace the two `deploy-staged` phrases in the outbound specification with `deploy --staging`. Retain “staged version” only where it describes Fastly's provider lifecycle; the public EdgeZero action and flag remain `DeployStaging` and `--staging`.

- [x] **Step 5: Run documentation formatting**

  Run: `cd docs && npm run format -- --check`

  Expected: PASS.

### Task 2: Add completion composition test-first

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`

- [x] **Step 1: Write failing composition tests**

  Add focused tests proving:

  - joined callbacks both receive the same report and run once in left-to-right order;
  - on unwind-capable test targets, a panic in the left callback does not suppress the right callback;
  - dropping a joined completion before terminal egress releases both captured resources exactly once;
  - calling the enclosing attempt's terminal path more than once cannot rerun either callback.

- [x] **Step 2: Run the focused tests and observe RED**

  Run: `cargo test -p edgezero-core response_egress_completion_join`

  Expected: FAIL because `ResponseEgressCompletion::join` does not exist.

- [x] **Step 3: Implement the minimal composition primitive**

  Add complete public rustdoc covering deterministic order, same-report delivery, abandonment, and the unwind-versus-panic-abort qualification. Implement an `#[inline]` consuming method equivalent to:

  ```rust
  #[must_use]
  #[inline]
  pub fn join(mut self, mut other: Self) -> Self {
      Self::new(move |report| {
          self.complete(report);
          other.complete(report);
      })
  }
  ```

  Reuse `complete` for each child so each callback retains its own panic boundary on unwind-capable targets. Do not add cloneability, shared mutable callback registries, or a compatibility wrapper.

- [x] **Step 4: Run focused and core tests**

  Run: `cargo test -p edgezero-core response_egress_completion_join`

  Run: `cargo test -p edgezero-core`

  Expected: PASS.

### Task 3: Require detached response deadlines test-first

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/lib.rs` only if public re-exports require adjustment

- [x] **Step 1: Write failing head/deadline tests**

  Add tests proving `write_deadline_after` is anchored to `request_start`, clamps durations to `DEADLINE_FAR_FUTURE`, and fails closed to `request_start` on overflow.

- [x] **Step 2: Write failing detached-send tests**

  Add tests proving:

  - `Send { completion, deadline }` exposes the selected deadline to `ResponseEgressEnvelope::begin`;
  - the resulting write policy uses the earlier of the detached application deadline and the default response policy;
  - an already-expired detached deadline remains expired;
  - `Abort` still creates no response and invokes no completion;
  - the default factory selects a finite deadline from captured request start.

- [x] **Step 3: Run focused tests and observe RED**

  Run: `cargo test -p edgezero-core detached_ingress_error`

  Run: `cargo test -p edgezero-core detached_response_egress_head`

  Expected: FAIL on the missing named fields/helper/deadline propagation.

- [x] **Step 4: Implement the hard-cut decision and helper**

  Change `Send(ResponseEgressCompletion)` to the named-field variant. Implement `write_deadline_after` with `#[must_use]`, `#[inline]`, and public rustdoc covering request-start anchoring, `DEADLINE_FAR_FUTURE` clamping, and fail-closed overflow, using the same bounded arithmetic pattern as `IngressHead::read_deadline_after`.

- [x] **Step 5: Thread the deadline through detached egress**

  In `App::detached_ingress_error_egress`, render the bounded response, insert `ResponseEgressDeadline::new(deadline)` into its extensions, and build the existing detached envelope with the supplied completion. Keep the default no-op observer and default policy; `ResponseEgressEnvelope::begin` already clamps the policy to the application deadline.

- [x] **Step 6: Update the default decision factory**

  Return `Send { completion: ResponseEgressCompletion::empty(), deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET) }`.

- [x] **Step 7: Run focused and core tests**

  Run: `cargo test -p edgezero-core detached_ingress_error`

  Run: `cargo test -p edgezero-core response_egress`

  Run: `cargo test -p edgezero-core`

  Expected: PASS.

### Task 4: Migrate adapters and prove detached behavior

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Migrate all factories to named fields**

  Every `Send`-producing test factory returns `Send { completion, deadline: head.write_deadline_after(...) }`. Retain explicit `Abort` coverage. Keep adapter production dispatch unchanged because it already receives the fully prepared `ResponseEgressEnvelope`.

- [x] **Step 2: Extend the native deadline proof**

  Add an Axum contract assertion showing an expired detached deadline reaches the response-egress conversion boundary as expired and still preserves exactly-once completion. Keep the existing raw-socket zero-byte abort proof.

- [x] **Step 3: Run focused adapter tests**

  Run:

  ```bash
  scripts/run_test_nonzero.sh normalized_ingress_error_returns_typed_response_without_application_observation cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils
  scripts/run_test_nonzero.sh admission_policy_error_decision_send_uses_detached_completion cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils
  cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract --no-run
  cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract --no-run
  cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract --no-run
  ```

  Expected: the two native tests execute and pass; each provider contract test target compiles. Provider execution occurs with the pinned runners in Task 6.

- [x] **Step 4: Run adapter crate tests**

  Run: `cargo test -p edgezero-adapter-axum --all-targets`

  Run: `cargo test -p edgezero-adapter-cloudflare --all-targets --features test-utils`

  Run: `cargo test -p edgezero-adapter-fastly --all-targets --features "cli test-utils"`

  Run: `cargo test -p edgezero-adapter-spin --all-targets --features test-utils`

  Expected: PASS.

### Task 5: Align demo, templates, generator, and guards

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `scripts/check_outbound_legacy_api.sh`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] **Step 1: Write failing generator, demo, and guard assertions**

  Require generated source to contain the named `completion` and `deadline` fields, the exact `head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET)` expression, and `.join(`. Add a demo lifecycle test proving both composed completion effects occur exactly once. Extend the legacy checker to reject `DetachedResponseEgressDecision::Send(`, `DeployStaged`, `deploy_staged`, `deploy-staged`, and `--staged` in active public/code surfaces while excluding historical plans and legitimate Fastly provider-state prose.

- [x] **Step 2: Run the focused checks and observe RED**

  Run: `cargo test -p edgezero-cli generate_new_scaffolds_workspace_layout`

  Run: `cd examples/app-demo && cargo test composed_response_completion`

  Run: `bash scripts/check_outbound_legacy_api.sh`

  Expected: FAIL until the template, demo, and stale specification text are migrated.

- [x] **Step 3: Migrate the reference app and template**

  Return `Send { completion, deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET) }`. Add a concise example composing the static completion with a late-bound completion using `join` without introducing application-specific terminology, and make the new demo lifecycle test pass.

- [x] **Step 4: Strengthen the documentation contract checker**

  Require the named detached decision fields, mandatory deadline semantics, composition semantics, unchanged abort behavior, and the existing provider capability caveats.

- [x] **Step 5: Run demo, generator, and contract checks**

  Run: `cargo test -p edgezero-cli generate_new_scaffolds_workspace_layout`

  Run: `cd examples/app-demo && cargo test --workspace --all-targets`

  Run: `bash scripts/check_outbound_legacy_api.sh`

  Run: `node scripts/check_outbound_docs_contract.mjs`

  Expected: PASS.

### Task 6: Complete cross-target verification and publish

**Files:**
- Modify only files required by failures caused by this hard cut.

- [x] **Step 1: Run native workspace gates**

  Run: `cargo fmt --all -- --check`

  Run: `cargo test --workspace --all-targets`

  Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

  Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`

  Run:

  ```bash
  bash scripts/check_adapter_feature_matrix.sh axum native
  bash scripts/check_adapter_feature_matrix.sh cloudflare native
  bash scripts/check_adapter_feature_matrix.sh fastly native
  bash scripts/check_adapter_feature_matrix.sh spin native
  ```

  Expected: PASS.

- [x] **Step 2: Run strict WASM Clippy and feature-matrix gates**

  Run:

  ```bash
  cargo clippy -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-cloudflare --features "cloudflare test-utils" --target wasm32-unknown-unknown --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly cli" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly test-utils" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly cli test-utils" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-spin --features spin --target wasm32-wasip2 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-spin --features "spin test-utils" --target wasm32-wasip2 --all-targets -- -D warnings
  bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown
  bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1
  bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2
  ```

  Expected: PASS.

- [ ] **Step 3: Run runner-backed WASM tests**

  With the pinned runners installed, run:

  ```bash
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract
  CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract
  CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' scripts/run_test_nonzero.sh closed_lifecycle_orders_hooks_and_returns_state_after_delivery cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract
  CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' scripts/run_test_nonzero.sh router_dispatches_get_and_returns_response cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract
  CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh streamed_upload_eof_yields_before_final_precedence cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --lib
  (cd crates/edgezero-adapter-fastly && CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' ../../scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked --no-default-features --features fastly,test-utils --lib)
  ```

  Expected: PASS. If a pinned runner is unavailable locally, the corresponding hosted CI job must pass on the exact pushed head before completion.

- [x] **Step 4: Run docs, demo, and generated-project gates**

  Run:

  ```bash
  RUSTDOCFLAGS='-D warnings' cargo doc --offline --locked --workspace --all-features --no-deps
  scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds
  bash scripts/check_outbound_legacy_api.sh
  node scripts/check_outbound_docs_contract.mjs
  (cd docs && npm run lint && npm run format && npm run build)
  (cd examples/app-demo && cargo fmt --all -- --check)
  (cd examples/app-demo && cargo test --locked --workspace --all-targets)
  (cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)
  (cd examples/app-demo && cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare)
  (cd examples/app-demo && cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly)
  (cd examples/app-demo && cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin)
  ```

  Expected: PASS.

- [x] **Step 5: Review the complete diff**

  Verify no tuple `Send` constructor, compatibility shim, legacy public staging identifier, application-specific terminology, unrelated refactor, or unbounded detached response remains.

- [ ] **Step 6: Commit and push**

  Commit with an application-neutral message, push to the existing PR #275 branch, and wait for the hosted check matrix on the exact pushed head.

  Expected: every required check passes; the PR remains mergeable, subject only to repository review policy.
