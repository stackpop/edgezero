# Response Completion, Ingress Abort, and Fallible App Assembly Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Apply
> `superpowers:test-driven-development` to every behavior change and
> `superpowers:verification-before-completion` before claiming success.

**Goal:** Give every response-producing admission decision one non-clone completion owner, add a
portable non-response ingress abort outcome, and make application configuration fail explicitly
before an adapter begins serving a request.

**Architecture:** `AdmissionDecision` owns either a response completion, grant, and deadline or an
explicit abort. Core transfers the completion through `PreparedIngress`,
`ResponseEgressEnvelope`, and the sole `ResponseEgressAttempt`; the attempt stores terminal state,
invokes the response completion once, then invokes the global observer. Adapters map abort to their
strongest pre-response transport boundary and propagate fallible `Hooks::build_app()` before request
conversion, admission, or body polling. This is an atomic hard cut with no extension carrier,
infallible compatibility method, or response-extraction bypass.

**Source of truth:**
- [`2026-09-08-response-egress-design.md`](../specs/2026-09-08-response-egress-design.md)
- [`2026-08-22-inbound-body-design.md`](../specs/2026-08-22-inbound-body-design.md)
- [`2026-06-16-blob-app-config.md`](../specs/2026-06-16-blob-app-config.md)

**Tech Stack:** Rust 1.95, `http`, futures, Hyper/Axum/Tokio, Cloudflare Workers, Fastly Compute,
Spin SDK WASIp3, proc macros, Handlebars templates, VitePress documentation.

---

## File Structure

- `crates/edgezero-core/src/response_egress.rs`: owns the non-clone completion callback and invokes
  it from the exactly-once attempt state machine.
- `crates/edgezero-core/src/ingress.rs`: owns admission decision/outcome types and transfers the
  completion or explicit abort without body access.
- `crates/edgezero-core/src/app.rs`: retains completion across routing/error replacement and exposes
  a response-or-abort high-level dispatch result; hard-cuts `Hooks` to fallible construction.
- `crates/edgezero-core/src/manifest.rs` and `crates/edgezero-core/src/lib.rs`: expose the new public
  lifecycle types and `ingress-admission-abort` capability.
- `crates/edgezero-macros/src/app.rs`: emits the fallible `configure`/`build_app` contract without
  discarding callback results.
- `crates/edgezero-adapter-axum/src/{service,connection,dev_server}.rs`: maps abort to a Hyper
  service error/connection close and configuration failure to an error before listener bind.
- `crates/edgezero-adapter-{cloudflare,fastly,spin}/src/{lib,request}.rs`: maps abort to a fixed
  platform error and configuration failure to the documented pre-dispatch boundary.
- `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/cli.rs`: declares the new abort
  capability conservatively.
- `examples/app-demo/crates/app-demo-core/src/lib.rs` and
  `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`: demonstrate the new completion and
  fallible configuration signatures.
- `crates/edgezero-cli/src/generator.rs`, adapter templates, and demo adapter entrypoints: assert
  generated and shipped applications use the hard-cut APIs.
- `docs/guide/{handlers,capabilities}.md`, adapter guides, and
  `scripts/check_outbound_docs_contract.mjs`: publish and mechanically enforce the contract.

## Global Constraints

- `ResponseEgressCompletion` is non-clone, never stored in `http::Extensions`, and always present
  on response-producing admission decisions; no-resource callers use `empty()`.
- Terminal state is stored before callbacks. Completion runs once before the global observer; on
  unwind-capable targets each panic is contained independently with category-only logging.
- `ResponseEgressEnvelope::into_response()` is removed. A response can be transmitted only after
  `begin()` transfers completion ownership into an attempt.
- `Abort` creates no response, envelope, attempt, completion report, or global observer report and
  polls no request body.
- No adapter maps `Abort` to EdgeZero's normal JSON error response.
- `Hooks::configure` and `Hooks::build_app` return `Result`; no unchecked or infallible bridge is
  retained.
- After every production edit, run the narrowest affected package tests before proceeding.

### Task 1: Add the response-scoped completion state machine

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-macros/tests/app_macro.rs`
- Modify: `crates/edgezero-adapter-axum/src/{connection,response}.rs`
- Modify: `crates/edgezero-adapter-{cloudflare,fastly,spin}/src/response.rs`
- Test: colocated tests in `crates/edgezero-core/src/response_egress.rs`

- [x] **Step 1: Write red completion-order and exactly-once tests**

  Add tests proving normal completion, source/transport failure, fallback completion, guard drop,
  and competing terminal signals invoke the response completion exactly once before the global
  observer. Add an unwind test proving a completion panic does not suppress the observer and an
  observer panic does not re-enter completion.

  Use an ordered recorder whose expected sequence is:

  ```rust
  vec!["completion:completed", "observer:completed"]
  ```

- [x] **Step 2: Run the focused tests and verify the missing API fails**

  Run:

  ```bash
  cargo test -p edgezero-core --lib response_egress::tests::response_completion
  ```

  Expected: compile failure because `ResponseEgressCompletion` and the completion constructor
  parameter do not exist.

- [x] **Step 3: Implement direct completion ownership**

  Add the public non-clone owner:

  ```rust
  pub struct ResponseEgressCompletion {
      callback: Option<Box<dyn FnOnce(&ResponseEgressReport) + Send + 'static>>,
  }

  impl ResponseEgressCompletion {
      pub fn empty() -> Self;
      pub fn new<Complete>(complete: Complete) -> Self
      where
          Complete: FnOnce(&ResponseEgressReport) + Send + 'static;
  }
  ```

  Move it into `ResponseEgressEnvelope`, transfer it to `ResponseEgressAttempt` before policy or
  framing work, store terminal state first, invoke completion through its own unwind boundary, and
  then invoke the observer. In this same step, update every direct attempt/envelope constructor in
  core `app.rs` and all four adapter response/connection test helpers to pass an explicit empty
  completion. Do not defer those call sites to later tasks: Task 1 must leave the workspace
  compilable. Re-export the type from `edgezero_core`.

- [x] **Step 4: Remove the response extraction bypass**

  Delete `ResponseEgressEnvelope::into_response()`. Replace core test inspection with a test-only
  helper that calls `begin()`, begins writing, terminally settles the attempt, and only then obtains
  the response from `PreparedResponseEgress`. Migrate the macro integration test at the same time
  so the workspace-wide all-targets check remains compilable. Add `compile_fail` rustdoc examples
  proving the completion cannot be cloned or inserted into `http::Extensions`.

- [x] **Step 5: Run the core lifecycle suite**

  Run:

  ```bash
  cargo test -p edgezero-core --lib response_egress
  cargo test -p edgezero-core --doc
  cargo check --workspace --all-targets --features "fastly cloudflare spin"
  cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
  cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
  cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
  ```

  Expected: all response-egress unit tests and compile-fail doctests pass.

- [x] **Step 6: Commit the core completion primitive**

  ```bash
  git add crates/edgezero-core/src/response_egress.rs crates/edgezero-core/src/app.rs \
    crates/edgezero-core/src/lib.rs crates/edgezero-macros/tests/app_macro.rs \
    crates/edgezero-adapter-*/src/response.rs \
    crates/edgezero-adapter-axum/src/connection.rs
  git commit -m "feat(core): add response-scoped egress completion"
  ```

### Task 2: Carry completion through ingress and add explicit abort

**Files:**
- Modify: `crates/edgezero-core/src/ingress.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/router.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Test: colocated tests in those files

- [x] **Step 1: Write red admission ownership tests**

  Add table-driven tests for `Admit`, `ReadBodyBeforeFallback`, `Refuse`, and `Abort`. Prove the
  exact completion reaches normal handler success, handler error, canonical 404/405, exact-cap
  fallback, overflow, timeout, admitted conversion error, and detached refusal. Prove a dropped
  pre-begin envelope releases the captured resource without invoking its callback.

- [x] **Step 2: Write red abort tests at the core boundary**

  Assert `Abort` yields `IngressAdmissionOutcome::Aborted`, then
  `IngressBeginOutcome::Aborted`, and finally `IngressDispatchOutcome::Aborted`; body, middleware,
  handler, completion, observer, and response counters must remain zero.

- [x] **Step 3: Run focused tests and verify the variants fail to compile**

  Run:

  ```bash
  cargo test -p edgezero-core --lib ingress::tests
  cargo test -p edgezero-core --lib app::tests
  ```

  Expected: compile failures for the new completion fields and abort/result variants.

- [x] **Step 4: Hard-cut the admission API**

  Implement these shapes without compatibility constructors:

  ```rust
  pub enum AdmissionDecision {
      Admit {
          completion: ResponseEgressCompletion,
          grant: IngressGrant,
          read_deadline: Deadline,
      },
      ReadBodyBeforeFallback {
          completion: ResponseEgressCompletion,
          grant: IngressGrant,
          max_body_bytes: usize,
          read_deadline: Deadline,
          on_exceeded: BufferedIngressResponse,
          on_timeout: BufferedIngressResponse,
      },
      Refuse {
          completion: ResponseEgressCompletion,
          response: Response,
      },
      Abort,
  }
  ```

  Make `IngressAdmissionOutcome::Admitted` carry `AdmittedIngress` plus completion, make
  `PreparedIngress` own completion beside the router token/ingress proof. Define refusal and abort
  explicitly at the validated boundary:

  ```rust
  pub enum IngressAdmissionOutcome {
      Admitted {
          completion: ResponseEgressCompletion,
          ingress: AdmittedIngress,
      },
      Refused {
          completion: ResponseEgressCompletion,
          response: Response,
      },
      Aborted,
  }
  ```

  Add `Aborted` to the begin outcome. The default policy supplies
  `ResponseEgressCompletion::empty()`.

- [x] **Step 5: Retain completion across response replacement**

  Add `IngressDispatchOutcome::{Response, Aborted}`. Change `App::dispatch_ingress` to return it.
  In `dispatch_admitted`, split the completion from `PreparedIngress` before awaiting router
  dispatch, retain it while the router consumes `AdmittedIngress`, and move it into the resulting
  envelope regardless of the selected response. Make refusal egress use detached policy/no-op
  observer plus the refusal completion; pre-admission errors use an empty completion.

- [x] **Step 6: Migrate all core admission constructors and response-inspection tests**

  Update core/router tests to provide explicit empty completions where no resource is under test.
  Replace every envelope `into_response()` use with a begin-and-settle helper. Keep
  `PreparedResponseEgress::into_response()` because the attempt already owns completion at that
  point.

- [x] **Step 7: Run all core tests**

  Run:

  ```bash
  cargo test -p edgezero-core
  ```

  Expected: all core tests pass, including exact completion and abort counters.

- [x] **Step 8: Commit the ingress ownership cut**

  ```bash
  git add crates/edgezero-core/src/app.rs crates/edgezero-core/src/ingress.rs \
    crates/edgezero-core/src/router.rs crates/edgezero-core/src/lib.rs
  git commit -m "feat(core): carry egress completion through ingress"
  ```

### Task 3: Map abort to the Axum connection boundary

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/src/connection.rs`
- Test: colocated tests in both files
- Test: `crates/edgezero-adapter-axum/tests/contract.rs`

- [x] **Step 1: Write a red raw-socket abort test**

  Configure admission to return `Abort`, send a request with a body over a real loopback HTTP/1
  socket, and assert EOF/reset with zero response bytes. Counters must prove no body poll,
  middleware/handler call, completion, or observer invocation.

- [x] **Step 2: Run the focused Axum tests and verify the request currently becomes a response**

  Run:

  ```bash
  cargo test -p edgezero-adapter-axum admission_abort -- --nocapture
  ```

  Expected: failure because the service has no abort error path.

- [x] **Step 3: Implement a private Hyper service abort error**

  Change `dispatch_request` and `AxumConnectionService::call` to return
  `Result<Response<AxumEgressBody>, AxumIngressAbort>`. Map only
  `IngressBeginOutcome::Aborted` to this error; all ordinary EdgeZero failures remain bounded HTTP
  responses. Extend connection classification with `ConnectionExit::AdmissionAborted` by
  downcasting the Hyper error chain, then drop the connection without constructing response head
  or body.

- [x] **Step 4: Migrate Axum tests away from envelope extraction**

  Update service/contract fixtures for mandatory completion fields and begin-and-settle any
  response envelope they inspect.

- [x] **Step 5: Run the Axum suite**

  Run:

  ```bash
  cargo test -p edgezero-adapter-axum
  ```

  Expected: all Axum unit, contract, and raw-socket tests pass.

- [x] **Step 6: Commit the Axum abort boundary**

  ```bash
  git add crates/edgezero-adapter-axum/src/service.rs \
    crates/edgezero-adapter-axum/src/connection.rs \
    crates/edgezero-adapter-axum/tests/contract.rs
  git commit -m "feat(axum): close connections on ingress abort"
  ```

### Task 4: Map abort across Cloudflare, Fastly, and Spin

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Test: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Test: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Test: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] **Step 1: Write red adapter abort contract tests**

  Reuse each adapter's injected source/delivery seam. For an aborting app, assert source
  construction and delivery closures are never called, no envelope exists, and the adapter returns
  a fixed category-only platform error. Keep Cloudflare/Fastly/Spin transport reset claims out of
  local tests.

- [x] **Step 2: Run the three focused suites and verify they fail**

  Run:

  ```bash
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    cargo test -p edgezero-adapter-cloudflare --no-default-features \
    --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract admission_abort
  CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" \
    cargo test -p edgezero-adapter-fastly --no-default-features \
    --features fastly,test-utils --target wasm32-wasip1 --test contract admission_abort
  CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y -S http=y" \
    cargo test -p edgezero-adapter-spin --no-default-features \
    --features spin,test-utils --target wasm32-wasip2 --test contract admission_abort
  ```

  Expected: failures because unknown begin outcomes are currently rendered as detached internal
  responses.

- [x] **Step 3: Implement non-response abort propagation**

  Cloudflare and Spin return their fixed host error directly before `make_source` or `deliver`.
  Fastly adds an abort branch to its private `DispatchOutcome` and returns a fixed
  `fastly::Error`/top-level error before finalize or delivery. Remove wildcard branches that convert
  the known abort variant into an internal HTTP response; retain a future-unknown fail-closed arm
  only where `#[non_exhaustive]` requires it.

- [x] **Step 4: Migrate contract fixtures to completion ownership**

  Add explicit empty completions to ordinary decisions and replace every
  `ResponseEgressEnvelope::into_response()` call with begin-and-settle test helpers. Add one
  response-producing test per adapter proving a refusal completion reaches terminal egress once.

- [x] **Step 5: Run adapter host and target checks**

  Run:

  ```bash
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    scripts/run_test_nonzero.sh admission_abort cargo test -p edgezero-adapter-cloudflare \
    --no-default-features --features cloudflare,test-utils \
    --target wasm32-unknown-unknown --test contract
  CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" \
    scripts/run_test_nonzero.sh admission_abort cargo test -p edgezero-adapter-fastly \
    --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract
  CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y -S http=y" \
    scripts/run_test_nonzero.sh admission_abort cargo test -p edgezero-adapter-spin \
    --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract
  cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
  cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
  cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
  ```

  Expected: all supported host tests and WASM checks pass.

- [x] **Step 6: Commit provider abort propagation**

  ```bash
  git add crates/edgezero-adapter-cloudflare crates/edgezero-adapter-fastly \
    crates/edgezero-adapter-spin
  git commit -m "feat(adapters): propagate ingress abort without a response"
  ```

### Task 5: Hard-cut Hooks and macro expansion to fallible assembly

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-macros/src/app.rs`
- Test: `crates/edgezero-macros/tests/app_macro.rs`
- Modify: `crates/edgezero-adapter-axum/src/dev_server.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Test: adapter-local host-testable build seams plus target contract tests

- [x] **Step 1: Write red core and macro propagation tests**

  Add a failing configure callback returning a typed `EdgeError`. Assert both hand-written Hooks
  and macro-generated Hooks return the same error from `build_app()` and never expose a partially
  configured `App`.

- [x] **Step 2: Run core/macro tests and verify callback results are currently discarded**

  Run:

  ```bash
  cargo test -p edgezero-core --lib app::tests::build_app
  cargo test -p edgezero-macros --test app_macro
  ```

  Expected: compile failure because `configure` and `build_app` are infallible.

- [x] **Step 3: Implement the fallible trait and macro signatures**

  Hard-cut to:

  ```rust
  pub trait Hooks {
      fn build_app() -> Result<App, EdgeError> {
          let mut app = App::with_name(Self::routes(), Self::name());
          Self::configure(&mut app)?;
          Ok(app)
      }

      fn configure(_app: &mut App) -> Result<(), EdgeError> {
          Ok(())
      }
  }
  ```

  Make `app!` emit the same explicit methods and return the configured callback result. Do not add
  `build_app_unchecked`, callback coercion, or panic fallback.

- [x] **Step 4: Write red adapter-boundary tests**

  Add one small adapter-local `build_app_for_dispatch<A>()` seam used by the production entrypoint
  before any platform request work. Use a `FailingHooks` implementation and a dispatch/bind/receive
  closure counter to prove failure prevents the next boundary. Axum's seam is host-tested and
  proves no listener bind. Cloudflare, Fastly, and Spin exercise their platform error mapping in
  target contract tests. Cloudflare logs only `EdgeError::kind()` and returns exactly
  `worker::Error::RustError("application configuration failed")`.

- [x] **Step 5: Propagate build failure at each entrypoint**

  Axum and Spin retain the `EdgeError` source under fixed `anyhow` context. Cloudflare maps through
  a private helper that logs stable kind only and emits the fixed public worker error. Fastly
  changes its app runners and generated/demo main return type if necessary so the top-level error
  retains the `EdgeError` source under fixed `"application configuration failed"` context. Build
  before the boundary specified in the source specs.

- [x] **Step 6: Run core, macro, and adapter entrypoint tests**

  Run:

  ```bash
  cargo test -p edgezero-core
  cargo test -p edgezero-macros
  cargo test -p edgezero-adapter-axum failing_configuration
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    scripts/run_test_nonzero.sh failing_configuration cargo test \
    -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils \
    --target wasm32-unknown-unknown --test contract
  CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" \
    scripts/run_test_nonzero.sh failing_configuration cargo test \
    -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils \
    --target wasm32-wasip1 --test contract
  CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y -S http=y" \
    scripts/run_test_nonzero.sh failing_configuration cargo test \
    -p edgezero-adapter-spin --no-default-features --features spin,test-utils \
    --target wasm32-wasip2 --test contract
  ```

  Expected: all package tests pass with no infallible build call sites.

- [x] **Step 7: Commit fallible application assembly**

  ```bash
  git add crates/edgezero-core/src/app.rs crates/edgezero-macros \
    crates/edgezero-adapter-axum/src/dev_server.rs \
    crates/edgezero-adapter-cloudflare/src/lib.rs \
    crates/edgezero-adapter-fastly/src/lib.rs \
    crates/edgezero-adapter-spin/src/lib.rs
  git commit -m "feat(app): make application assembly fallible"
  ```

### Task 6: Align capabilities, demo, generator, templates, and guides

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/cli.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `examples/app-demo/crates/app-demo-adapter-fastly/src/main.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-adapter-fastly/src/templates/src/main.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `docs/guide/handlers.md`
- Modify: `docs/guide/capabilities.md`
- Modify: adapter guides as required by changed entrypoint errors
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `scripts/check_outbound_legacy_api.sh`
- Test: colocated manifest, CLI capability, generator, demo lifecycle, and docs-contract tests

- [x] **Step 1: Write red capability and generator assertions**

  Add `Capability::IngressAdmissionAbort` with the string `ingress-admission-abort`. Assert Axum is
  `Native`; Cloudflare, Fastly, and Spin are `BestEffort`. Add generator assertions for fallible
  `configure_app`, explicit empty completions, fallible `build_app`, and the Fastly return type.

- [x] **Step 2: Run the focused tests and verify stale artifacts fail**

  Run:

  ```bash
  cargo test -p edgezero-core manifest::tests
  cargo test -p edgezero-cli generator
  node scripts/check_outbound_docs_contract.mjs
  ```

  Expected: failures until capability matrices, generated code, and docs are aligned.

- [x] **Step 3: Migrate the demo and core template**

  Make `configure_app` return `Result<(), EdgeError>`, add `Ok(())`, and give every decision an
  explicit `ResponseEgressCompletion`. Demonstrate a response-owned permit/drop probe without a
  global response-id map. Update every demo/template `build_app()` call to handle `Result`.

- [x] **Step 4: Migrate adapter templates and generated fixtures**

  Update Fastly's template/demo main if its runner now returns `anyhow::Result<()>`; update other
  adapter templates only where signatures changed. Regenerate or update snapshots/assertions and
  prove a newly generated project contains no removed API.

- [x] **Step 5: Publish and enforce the capability contract**

  Add the abort capability row to the public guide and checker. Document that Axum's owned HTTP/1
  close is Native while the provider adapters remain BestEffort pending deployed evidence. Update
  handler/configuration examples for `Result` and direct completion ownership. Extend the legacy
  scan to reject infallible configure/build signatures, `ResponseEgressEnvelope::into_response`,
  and any completion-in-response-extension carrier.

- [x] **Step 6: Run demo, generator, docs, and capability suites**

  Run:

  ```bash
  cargo test -p edgezero-cli
  scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline \
    --locked -p edgezero-cli --test generated_project_builds
  cargo test --manifest-path examples/app-demo/Cargo.toml -p app-demo-core
  cargo check --manifest-path examples/app-demo/Cargo.toml --workspace
  npm --prefix docs run format
  npm --prefix docs run lint
  node scripts/check_outbound_docs_contract.mjs
  bash scripts/check_outbound_legacy_api.sh
  ```

  Expected: all generated/demo code and documentation checks pass.

- [x] **Step 7: Commit the consumer migration**

  ```bash
  git add crates/edgezero-core/src/manifest.rs crates/edgezero-adapter-*/src/cli.rs \
    crates/edgezero-cli examples/app-demo docs/guide scripts
  git commit -m "docs: align consumers with ingress lifecycle hard cut"
  ```

### Task 7: Comprehensive verification, self-review, and PR update

**Files:**
- Review: all files changed by Tasks 1-6
- Modify if needed: PR #275 description

- [ ] **Step 1: Audit for stale and bypassing APIs**

  Run:

  ```bash
  rg -n "fn configure\([^)]*&mut App[^)]*\) \{|fn build_app\(\) -> App|ResponseEgressEnvelope.*into_response|AdmissionDecision::(Admit|Refuse|ReadBodyBeforeFallback)" crates examples docs/guide
  rg -n "ResponseEgressCompletion" crates examples docs/guide
  ```

  Inspect every result. The only response-extension lifecycle value may be
  `ResponseEgressDeadline`; completion must follow direct ownership.

- [ ] **Step 2: Run formatting and full native CI gates**

  Run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-targets
  cargo check --workspace --all-targets --features "fastly cloudflare spin"
  ```

  Expected: zero warnings and zero failures.

- [ ] **Step 3: Run all WASM and excluded-workspace gates**

  Run:

  ```bash
  cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
  cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
  cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
  cargo test --manifest-path examples/app-demo/Cargo.toml --workspace
  cargo check --manifest-path examples/app-demo/Cargo.toml --workspace
  ```

  Expected: all target and demo gates pass.

- [ ] **Step 4: Re-run docs and repository contract gates**

  Run:

  ```bash
  npm --prefix docs run format
  npm --prefix docs run lint
  node scripts/check_outbound_docs_contract.mjs
  bash scripts/check_outbound_legacy_api.sh
  git diff --check
  ```

  Expected: all commands exit zero.

- [ ] **Step 5: Perform a final code-review pass**

  Review ownership/drop paths, callback panic ordering, abort zero-read behavior, fixed public error
  messages, adapter capability claims, generated code, and every `build_app()` call. Remove dead
  branches and imports introduced by the hard cut; do not retain compatibility aliases.

- [ ] **Step 6: Commit any verification fixes and push PR #275**

  ```bash
  git add -A
  git commit -m "test: close lifecycle hard-cut coverage"
  git push origin docs/outbound-http-spec
  ```

  Skip the final commit only if verification produced no changes. Update PR #275's description to
  call out direct response completion ownership, non-response admission abort, fallible app
  assembly, capability evidence, and the intentionally breaking migration.
