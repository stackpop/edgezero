# Detached Ingress Abort Decision Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an application atomically choose either a response-owned completion resource or a transport abort for normalized ingress errors before EdgeZero constructs a detached response.

**Architecture:** Replace the completion-only detached-response factory with a synchronous typed decision factory returning `DetachedResponseEgressDecision::Send(ResponseEgressCompletion)` or `Abort`. `App::detached_ingress_error_egress` converts that decision into the existing `IngressDispatchOutcome`, and every adapter maps `Aborted` through its existing non-response abort/error boundary. This is a hard cut: no compatibility aliases or legacy callback remain.

**Tech Stack:** Rust 2024, EdgeZero core ingress/response-egress APIs, Axum/Cloudflare/Fastly/Spin adapters, colocated and adapter contract tests, VitePress documentation.

---

### Task 1: Specify the detached error decision contract

**Files:**
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/guide/handlers.md`

- [x] **Step 1: Update the normative contract**

  Define `DetachedResponseEgressDecision::{Send(ResponseEgressCompletion), Abort}`. State that the callback executes exactly once after normalized metadata exists and before response construction. `Abort` creates no response, envelope, completion, observer report, middleware call, handler call, or body poll. The native adapter proves zero response bytes over a raw socket; provider adapters return through their existing fixed pre-response error boundaries without claiming a transport-observed reset.

- [x] **Step 2: Preserve capability boundaries**

  Keep raw parser failures outside the portable lifecycle and retain the existing `BestEffort` response-egress capability classifications. Update the public capability guide without promoting provider abort support.

- [x] **Step 3: Run documentation checks**

  Run: `node scripts/check_outbound_docs_contract.mjs`

  Expected: PASS with no stale completion-only factory contract.

### Task 2: Add failing decision tests across core and adapters

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Write failing core tests for `Send`, `Abort`, and the default**

  Add tests proving `Send` creates detached egress and settles its completion exactly once, while `Abort` returns `IngressDispatchOutcome::Aborted` without constructing or settling a completion. Prove an unset factory defaults to `Send(ResponseEgressCompletion::empty())`.

- [x] **Step 2: Run the focused tests and observe RED**

  Run: `cargo test -p edgezero-core detached_ingress_error`

  Expected: FAIL because the typed decision and outcome-returning API do not exist.

- [x] **Step 3: Write failing adapter tests before changing production call sites**

  Add two cases per adapter: normalized request-head validation failure and admission-policy failure. Each decision factory returns `Abort`; assert exactly one factory call and zero response delivery, body polling, middleware, handler, completion, and observer activity. Where a failing policy created an owned probe before returning its error, assert its drop count. For the native adapter, add a raw-socket proof of zero response bytes. For provider adapters, assert the existing fixed pre-response error boundary without claiming a transport reset.

- [x] **Step 4: Run the focused suites and observe RED**

  Run:

  `scripts/run_test_nonzero.sh normalized_ingress_error_decision_abort_closes_without_response cargo test -p edgezero-adapter-axum --no-default-features --features axum,test-utils --lib normalized_ingress_error_decision_abort_closes_without_response`

  `scripts/run_test_nonzero.sh normalized_ingress_error_decision_abort_uses_provider_error cargo test -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract normalized_ingress_error_decision_abort_uses_provider_error`

  `scripts/run_test_nonzero.sh normalized_ingress_error_decision_abort_uses_provider_error cargo test -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract normalized_ingress_error_decision_abort_uses_provider_error`

  `scripts/run_test_nonzero.sh normalized_ingress_error_decision_abort_uses_provider_error cargo test -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract normalized_ingress_error_decision_abort_uses_provider_error`

  Expected: FAIL because the decision enum/setter do not exist. The sentinels prove the intended suites are non-empty.

### Task 3: Implement the core decision and adapter mappings

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Implement the hard-cut core API**

  Add the public non-exhaustive decision enum and decision-factory alias. Replace `set_detached_response_egress_completion_factory` with `set_detached_response_egress_decision_factory`. Change `detached_ingress_error_egress` to return `IngressDispatchOutcome`, using `Response(Box<ResponseEgressEnvelope>)` for `Send` and `Aborted` for `Abort`. Default to `Send(ResponseEgressCompletion::empty())`.

- [x] **Step 2: Migrate existing `Send` fixtures and map every adapter boundary**

  Wrap every existing detached completion in `DetachedResponseEgressDecision::Send`. Match `IngressDispatchOutcome::Response`, `Aborted`, and a wildcard arm at every normalized-validation and admission-policy-error call site. Both `Aborted` and unknown future variants fail closed through the existing non-response abort boundary; do not add another transport error type.

- [x] **Step 3: Run core and focused adapter tests and observe GREEN**

  Run: `cargo test -p edgezero-core detached_ingress_error`

  Run the four `scripts/run_test_nonzero.sh` commands from Task 2, then run equivalent sentinel commands for each admission-policy-error abort test.

  Expected: PASS.

- [x] **Step 4: Run the core suite**

  Run: `cargo test -p edgezero-core`

  Expected: PASS.

### Task 4: Align the reference app and generator

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`

- [x] **Step 1: Add a failing generator assertion**

  Require generated applications to use the decision factory and `Send` variant. Retain one intentional negative assertion rejecting the removed completion-only setter.

- [x] **Step 2: Run the generator test and observe RED**

  Run: `cargo test -p edgezero-cli generate_new_scaffolds_workspace_layout`

  Expected: FAIL on the legacy setter or missing decision enum.

- [x] **Step 3: Update demo and templates**

  Wrap completion resources in `DetachedResponseEgressDecision::Send`. Keep the reference behavior unchanged; the API demonstrates where a bounded application may return `Abort`.

- [x] **Step 4: Run demo and generator tests**

  Run: `cargo test -p edgezero-cli generate_new_scaffolds_workspace_layout`

  Run from `examples/app-demo`: `cargo test --workspace --all-targets`

  Expected: PASS.

### Task 5: Complete cross-target verification

**Files:**
- Modify only files required by verification failures caused by this hard cut.
- Modify: `scripts/check_outbound_legacy_api.sh`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] **Step 1: Enforce the hard cut in repository checkers**

  Extend the legacy API checker to reject the old factory type/setter in active Rust/template surfaces. Extend the docs contract checker to require the new decision enum, `Send`/`Abort` semantics, and unchanged raw-parser/provider capability caveats.

  Run a scoped stale-API search over `crates`, `examples`, and active guide/spec documents, excluding the intentional generator negative assertion and historical plans.

  Expected: no production, template, test, or documentation matches.

- [x] **Step 2: Run formatting and native gates**

  Run: `cargo fmt --all -- --check`

  Run: `cargo test --workspace --all-targets`

  Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

  Expected: PASS.

- [x] **Step 3: Run feature and WASM gates**

  Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`

  Run the strict WASM Clippy matrix:

  ```bash
  cargo clippy -p edgezero-adapter-cloudflare --features "cloudflare" --target wasm32-unknown-unknown --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-cloudflare --features "cloudflare test-utils" --target wasm32-unknown-unknown --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly cli" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly test-utils" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-fastly --features "fastly cli test-utils" --target wasm32-wasip1 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-spin --features "spin" --target wasm32-wasip2 --all-targets -- -D warnings
  cargo clippy -p edgezero-adapter-spin --features "spin test-utils" --target wasm32-wasip2 --all-targets -- -D warnings
  ```

  Run the target-specific adapter contract commands from Task 2 with configured WASM runners, plus:

  ```bash
  bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown
  bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1
  bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2
  ```

  Expected: PASS.

- [x] **Step 4: Run rustdoc, docs, demo, and generated-project gates**

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

- [x] **Step 5: Review, commit, and push**

  Review the diff for accidental compatibility shims, stale terminology, and unrelated changes. Commit with an application-neutral message and push to the existing PR branch.
