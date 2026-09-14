# Owned Response Egress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to
> implement this plan task by task. Apply `superpowers:test-driven-development` to every
> behavioral change and `superpowers:verification-before-completion` before claiming success.

**Goal:** Replace response-object handoff and bounded buffering with adapter-owned response
delivery lifetimes across Axum, Cloudflare, Fastly, and Spin, while preserving one application
clock, one absolute deadline, exact accepted-byte accounting, and exactly one terminal report.

**Architecture:** Core owns the portable lifecycle state machine, policy evaluation, fallback
lifecycle, and shared HTTP framing normalization. Each adapter owns a runtime-specific
coordinator and writer pump through its strongest available transport boundary. There is no
cross-adapter sink abstraction. The migration is a hard cut: obsolete response-returning APIs,
buffering constants, compatibility aliases, tests, docs, and generated entrypoints are removed.

**Source of truth:**
[`2026-09-08-response-egress-design.md`](../specs/2026-09-08-response-egress-design.md).

**Tech stack:** Rust 1.95, `http`/`http-body`, Hyper/Axum/Tokio, Cloudflare Workers JS streams,
Fastly Compute body handles, Spin SDK WASIp3, Handlebars generator templates.

**Implementation status:** The hard-cut API migration and runtime-specific send-owning paths are
complete. Remaining unchecked items are evidence hardening that requires a deployed provider or
additional raw-socket fixtures; they do not justify a `Native` capability claim and are not hidden
as implementation success.

## Global Constraints

- Preserve the injected `MonotonicClock` from ingress through every egress sample.
- Evaluate the application response-egress policy exactly once per response attempt.
- Use one absolute write deadline. Never reset it per source poll, write, flush, or finish.
- Account only payload bytes accepted at the adapter's documented boundary, including an
  accepted partial prefix before a terminal error.
- One adapter-local coordinator owns `ResponseEgressAttempt`; helpers report events to it and
  never terminalize independently.
- Precommit fallback uses the same attempt, records fallback body metadata, preserves the
  original terminal cause, and is bounded by the internal fallback safety deadline.
- After commit, never replace or append an application error body.
- Capability cells are `Native` only with direct target-level evidence, `BestEffort` when a
  meaningful approximation is tested, and `Unsupported` otherwise.
- One explicitly transitional `#[doc(hidden)]` lifecycle method may coexist with the old method
  from Task 1 through Task 8 so the workspace stays buildable while adapters migrate. It is not a
  compatibility promise: each migrated adapter must eliminate its old call sites, the legacy scan
  must show a monotonically decreasing inventory, and the final tree contains neither method pair
  nor any old public API.
- After every production code edit, run the narrowest affected `cargo test -p ...` command.

## Task 0: Prove the risky target boundaries before the API cut

**Files:**
- Create: `crates/edgezero-adapter-axum/src/connection.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Test: colocated tests in `connection.rs`
- Test: `crates/edgezero-adapter-fastly/src/response.rs`
- Test: `crates/edgezero-adapter-spin/src/response.rs`
- Test: `crates/edgezero-adapter-cloudflare/src/response.rs`

- [x] Add an executable Axum/Hyper HTTP/1 prototype that proves a private non-`Send` response
  body can run on `LocalSet`, an independent timer can close a connection while Hyper is blocked
  below `poll_frame`, and ordered response IDs can attribute a pipeline-closing deadline without
  sharing the attempt between tasks.
- [x] Add a raw-socket proof that distinguishes frame production from the strongest observable
  OS-acceptance boundary. If that boundary cannot be associated with one response, plan Axum
  success as `HostHandoff`/`BestEffort` rather than `Completed`/`Native`.
- [x] Compile minimal target-gated prototypes for Fastly `StreamingBodyHandle`, Spin raw WASIp3
  response/result writers, and Cloudflare fixed-length/transform writers plus request signal.
  These prototypes become the final private adapter primitives; no throwaway public API remains.
- [x] Run the focused Axum raw-socket tests and all three target checks before changing core's
  public lifecycle surface. Stop and revise the design if any required primitive is unavailable.

## Task 1: Harden the core lifecycle and fallback contract

**Files:**
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Test: colocated tests in the files above

- [x] Add red tests for `RequestCancelled`, application versus fallback body kind, fallback
  `Completed` versus `Aborted`, accepted partial-prefix accounting, terminal races, policy panic,
  deadline normalization failure, fallback timeout, fallback write failure, backward/frozen
  clocks, observer unwind containment, and category-only EdgeZero diagnostic logging. Treat the
  application-owned Rust panic hook as an explicit trust boundary; do not replace it from a library.
- [x] Add the owned begin contract under an explicitly transitional `#[doc(hidden)]` method that
  returns a live attempt and structured precommit cause when policy evaluation or deadline
  normalization fails. Core must not pre-terminalize. Keep the old method only long enough for
  unmigrated adapters to compile, and forbid new call sites.
- [x] Add `ResponseEgressBodyKind::{Application, Fallback}` and
  `ResponseEgressFallbackDisposition::{Completed, Aborted}` to bounded terminal reports.
- [x] Add `ResponseEgressOutcome::RequestCancelled`. Defer removal of `ResponseReturned` and the
  old begin method until every adapter uses the owned contract in Task 8; run a workspace compile
  after each lifecycle signature transition.
- [x] Make fallback lifecycle mutation explicit on `ResponseEgressAttempt`; reject invalid
  application-to-fallback transitions and preserve exactly-once observer delivery.
- [x] Ensure routing errors, admission refusals, 404, and 405 responses reach adapters as
  `ResponseEgressEnvelope` values. Keep only the explicitly documented low-level core dispatch
  escape hatch outside adapter serving paths.
- [x] Run `cargo test -p edgezero-core --lib response_egress` and
  `cargo test -p edgezero-core --lib app`, then `cargo check --workspace --all-targets`.

## Task 2: Centralize HTTP response framing preparation

**Files:**
- Create: `crates/edgezero-core/src/response_egress_framing.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/response_egress.rs`
- Test: `crates/edgezero-core/src/response_egress_framing.rs`

- [x] Add table-driven red tests for all `Content-Length` field lines and comma members,
  whitespace, invalid decimal forms, overflow, identical duplicates, conflicting duplicates,
  application `Transfer-Encoding`, `Trailer`, final 1xx, 101, successful CONNECT, HEAD, 204, 205,
  and 304 precedence.
- [x] Add red tests proving every `Connection` field is parsed as comma-separated tokens and
  removes both nominated fields and the standard hop-by-hop set before adapter conversion.
- [x] Add red body-length tests: exact `Body::Once`, incremental streamed equality, early EOF,
  one-byte-over, accepted partial native writes, body-suppressed responses, immediate source
  release without polling, and no application trailers.
- [x] Implement an adapter-facing prepared-response type containing normalized status, headers,
  body transmission mode, request method, and optional declared length. Wrap the body with one
  core length-enforcing stream so all adapters share incremental overrun and early-EOF behavior;
  the wrapper must not contain runtime writer logic.
- [x] Expose the helper only as hidden adapter plumbing; application-facing response APIs remain
  unchanged.
- [x] Run `cargo test -p edgezero-core --lib response_egress_framing` and then
  `cargo test -p edgezero-core`.

## Task 3: Replace Fastly response return with owned transmission

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `crates/edgezero-adapter-fastly/Cargo.toml`
- Modify: `crates/edgezero-adapter-fastly/src/templates/src/main.rs.hbs`
- Modify: `examples/app-demo/crates/app-demo-adapter-fastly/src/main.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Test: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Test: colocated tests in `response.rs` and `lib.rs`

- [x] Add red adapter-local pump tests for empty, once, multi-chunk, short write, zero-progress
  write, source error, write error, finish error, abandon failure, deadline equality at every
  observable boundary, and exactly-once finish/abandon/report ordering.
- [x] Add red precommit fallback tests for policy panic, normalization/conversion failure,
  already-expired deadline, HEAD suppression, fallback success, partial write, fallback safety
  timeout, finish failure, original-cause preservation, and fallback metadata.
- [x] Prototype the low-level path under the real Fastly feature: initialize once, receive the
  client request, create an empty `BodyHandle`, commit with
  `ResponseHandle::stream_to_client`, and consume through `StreamingBodyHandle`.
- [x] Carry the original request method into framing preparation and prove HEAD and status-based
  suppression drops the application source without polling it.
- [x] Implement exact short-write looping and account each accepted prefix before checking the
  deadline or processing the next result. On postcommit failure, abandon the body handle and
  retain the original cause.
- [x] Hard-cut `run_app` and extension/config variants to no-request, send-owning signatures.
  Remove all response-returning service methods used by normal dispatch and remove
  `#[fastly::main]` support.
- [x] In the same edit, migrate the Fastly scaffold, demo entrypoint, and generator assertions to
  the no-request send-owning runner. Run generated-project and demo Fastly target checks before
  considering the old signature removed.
- [x] Delete `FASTLY_RESPONSE_STREAM_BUFFER_BYTES`, buffered response conversion, and imports or
  dependencies used only by that path.
- [x] Keep response-write deadline support `BestEffort`: synchronous hostcalls get boundary
  checks, not a claim of preemptive cancellation. Define successful completion as Fastly host
  acceptance of body-handle close.
- [x] Run host tests with `--features cli`, compile every native and WASM feature combination,
  and execute the `fastly,test-utils` contract and library suites under Viceroy. A host
  `--all-features` test link is not a valid Fastly gate because provider ABI imports are supplied
  only by the WASM runtime.

## Task 4: Replace Spin full-response conversion with raw WASIp3 delivery

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/response.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Modify: `crates/edgezero-adapter-spin/Cargo.toml`
- Modify: `crates/edgezero-adapter-spin/src/templates/src/lib.rs.hbs`
- Modify: `examples/app-demo/crates/app-demo-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Test: `crates/edgezero-adapter-spin/tests/contract.rs`
- Test: colocated tests in `response.rs`

- [x] Add red adapter-local coordinator tests for empty, once, streamed, partial write, pending
  write versus deadline equality, cancellation acknowledgement, source failure, body-writer
  failure, result-writer failure, response-result success/error/pending, destruction during every
  state, immediate source release for suppressed bodies, and exactly one terminal report.
- [x] Add the complete precommit fallback matrix: policy/conversion failure, expired deadline,
  HEAD suppression, success, partial write, safety timeout, finish/result failure, original-cause
  preservation, fallback metadata, and source release.
- [x] Prototype returning a raw `spin_sdk::wasip3` response from the existing
  `#[http_service]` entrypoint and owning its `BodyWriter` in a spawned coordinator.
- [x] Carry the original request method into framing preparation and prove HEAD and status-based
  suppression drops the application source without polling it.
- [x] Implement cancellable write operations so a losing write remains owned until cancellation
  is acknowledged or the response is destroyed. No losing future may retain attempt access.
- [x] Account each accepted prefix, own both the body writer and response-result writer through
  terminal observation, close on clean EOF, and use `HostHandoff` for successful completion until
  target evidence proves network completion.
- [x] Remove `SpinFullResponse`, `FullBody`, `http_into_wasi_response`,
  `SPIN_RESPONSE_STREAM_BUFFER_BYTES`, and all response collection paths. Remove the direct
  `wasip3` dependency if the final code uses only the SDK re-export.
- [x] In the same edit, migrate the Spin scaffold, demo adapter, and generator assertions to the
  raw-response runner, then compile the generated and demo Spin applications.
- [x] Run `cargo test -p edgezero-adapter-spin --all-features` and
  `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.

## Task 5: Give Cloudflare one owned writer and cancellation lifetime

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/context.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/Cargo.toml`
- Modify: `crates/edgezero-adapter-cloudflare/src/templates/src/lib.rs.hbs`
- Modify: `crates/edgezero-adapter-cloudflare/src/templates/wrangler.toml.hbs`
- Modify: `examples/app-demo/crates/app-demo-adapter-cloudflare/src/lib.rs`
- Modify: `examples/app-demo/crates/app-demo-adapter-cloudflare/wrangler.toml`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Create: `scripts/probe_response_egress.mjs`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/response-egress-worker/Cargo.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/response-egress-worker/wrangler.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/response-egress-worker/src/lib.rs`
- Test: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Test: colocated WASM/helper tests in `response.rs`

- [x] Add red state-machine tests for one staged chunk, writer backpressure, declared-length
  stream success/mismatch, unknown-length transform stream, enqueue/write rejection, request
  abort, independent deadline equality, frozen application clock, cooperative yield, suppressed
  body release, and terminal races.
- [x] Add the complete precommit fallback matrix: policy/conversion failure, expired deadline,
  fixed-length HEAD suppression, success, partial writer acceptance, safety timeout, close failure,
  original-cause preservation, and fallback metadata.
- [x] Capture `Request.signal` before request conversion and retain it through response delivery.
  Enable request signals in generated/runtime configuration.
- [x] Carry the original request method into framing preparation and prove HEAD and status-based
  suppression drops the application source without polling it.
- [x] For normalized declared lengths, create a fixed-length Workers stream; for unknown length,
  create a transform stream. Register the sole writer/coordinator promise with Workers
  `Context` so it survives handler return.
- [x] Account bytes only after the writer promise accepts them. On signal abort report
  `RequestCancelled`; do not claim `ClientDisconnected` without deployed proof.
- [x] Complete clean close as `HostHandoff`, and keep abort/backpressure/completion/deadline
  capabilities `BestEffort` unless the target-level probe proves the stronger boundary.
- [x] Remove the old Rust stream lifecycle wrapper, premature enqueue accounting, and
  `ResponseReturned` path.
- [x] In the same edit, migrate the Cloudflare scaffold, demo adapter, and generator assertions;
  enable request signals in both Wrangler configurations and compile both applications.
- [ ] **Deferred provider evidence:** add a dedicated Worker fixture with
  `/_probe/ready`, `/_probe/slow`, `/_probe/no-demand`, `/_probe/partial`, and
  `/_probe/complete` routes plus machine-readable observer log records. In local mode,
  `scripts/probe_response_egress.mjs` must spawn
  `wrangler dev --config <fixture>/wrangler.toml --port <free-port>`, wait for the ready route,
  execute assertions, capture logs, and terminate the exact child process in `finally` on success,
  failure, or signal. Run it as `node scripts/probe_response_egress.mjs --mode local`. The script
  must assert slow-reader backpressure, no-demand expiry, cancel/disconnect behavior, partial-byte
  accounting, clean close, and one terminal report. Run the same HTTP assertions without process
  ownership as `node scripts/probe_response_egress.mjs --mode deployed --base-url <url>` and
  archive runtime/version/tolerance output. No `Native` promotion may rely on compilation, mocks,
  or local Wrangler alone.
- [x] Run deterministic host coordinator tests,
  `cargo test -p edgezero-adapter-cloudflare --all-features`, and
  `cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare`.

## Task 6: Move Axum serving to a connection-owned Hyper boundary

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/response.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/src/dev_server.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-axum/Cargo.toml`
- Modify: root `Cargo.toml`
- Modify: `crates/edgezero-adapter-axum/src/templates/src/main.rs.hbs`
- Modify: `examples/app-demo/crates/app-demo-adapter-axum/src/main.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Test: `crates/edgezero-adapter-axum/tests/contract.rs`
- Test: new raw-socket cases colocated with `service.rs` or in adapter integration tests

- [x] Add a compile-time red prototype for a direct Hyper HTTP/1 connection running on a Tokio
  `LocalSet` with a private non-`Send` response body and no unbounded channel.
- [x] Add red body/coordinator tests for source laziness, one staged frame, accepted-byte
  accounting, source error, write deadline waking while Hyper is not polling, response drop,
  transport error, deadline equality after accepted progress, suppressed-body source release, and
  one terminal report.
- [ ] Add raw-socket precommit fallback tests for policy/conversion failure, expired deadline,
  HEAD suppression, complete fallback, partial write, safety timeout, connection failure,
  original-cause preservation, and fallback metadata.
- [ ] Add raw-socket tests for a normal reader, zero-read client, disconnect before body,
  disconnect mid-body, deadline while blocked, HTTP/1 pipelining, collateral connection close,
  exact `Content-Length`, and postcommit failure without replacement payload.
- [x] Replace `axum::serve` with an owned Hyper HTTP/1 accept loop. One response coordinator owns
  the attempt; the connection supervisor only sends ordered connection events and closes the
  HTTP/1 connection when deadline enforcement requires it.
- [x] Carry the original request method into framing preparation and prove HEAD and status-based
  suppression drops the application source without polling it.
- [x] Define collateral pipelined outcomes: the response whose deadline wins keeps
  `DeadlineExceeded`; other unsettled responses on that connection settle as `TransportError`
  unless an earlier cause already won.
- [x] Remove public `EdgeZeroAxumService`, buffered `into_axum_response`,
  `AXUM_RESPONSE_STREAM_BUFFER_BYTES`, and any transport-blind normal serving path.
- [x] In the same edit, migrate the Axum scaffold, demo server, and generator assertions to the
  connection-owning server API, then compile and smoke-test both generated and demo servers.
- [x] Promote individual Axum capability cells to `Native` only for boundaries established by
  the raw-socket tests; leave any unproved cell `BestEffort` or `Unsupported`.
- [x] Run `cargo test -p edgezero-adapter-axum --all-features`.

## Task 7: Validate all migrated templates and demo apps together

**Files:**
- Verify: all adapter templates and demo entrypoints migrated in Tasks 3-6
- Modify: `crates/edgezero-cli/src/generator.rs`
- Test: generator tests in `crates/edgezero-cli/src/generator.rs`

- [x] Consolidate cross-adapter generator assertions that reject `#[fastly::main]`,
  request-in/response-out Fastly runners, `SpinFullResponse`, old Axum service construction, and
  missing Cloudflare request-signal configuration.
- [x] Verify every scaffold and demo adapter uses the send-owning entrypoint established in its
  adapter task. Do not retain alternate legacy examples.
- [x] Generate one project per adapter into temporary directories and verify the rendered source
  contains only current APIs.
- [x] Run `cargo test -p edgezero-cli generator` and
  `cargo check --manifest-path examples/app-demo/Cargo.toml --workspace`.
- [x] Run generated target checks for native, `wasm32-wasip1`, `wasm32-unknown-unknown`, and
  `wasm32-wasip2` where the installed toolchain supports them.

## Task 8: Remove old APIs, dependencies, and accidental response bypasses

**Files:**
- Modify: all adapter `lib.rs`, `request.rs`, `response.rs`, manifests, and contract tests above
- Modify: `scripts/check_outbound_legacy_api.sh`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `Cargo.lock`
- Delete: `docs/superpowers/plans/2026-09-12-fastly-manual-response-egress.md`

- [x] Derive the forbidden inventory from design §6 and extend the legacy checker to scan
  production code, templates, demos, generated fixtures, current guides, manifests, and feature
  definitions. Fail on `ResponseReturned`, the old `begin` signature, all response buffer
  constants, `SpinFullResponse`, `FullBody`, `EdgeZeroAxumService`, public converters such as
  `from_core_response`, alternate response-returning dispatch/runners, `#[fastly::main]`, obsolete
  feature flags, adapter use of `collect_response_stream*`, and buffered-fallback guide language.
- [x] After all adapters use the owned contract, remove the transitional begin method, rename the
  owned method to the final `begin`, remove `ResponseReturned` and its special case, update every
  call site, and run `cargo check --workspace --all-targets` immediately.
- [x] Audit every adapter return and `?` path after the request-egress context exists. Route it
  through the owned coordinator, including errors from handlers, middleware, routing, fallback
  drain, response normalization, and policy evaluation.
- [x] Audit dependencies and feature gates with `cargo tree`/`rg`; `cargo machete` was not
  installed. Remove dependencies and gates that existed only for buffered conversion or obsolete
  entrypoints.
- [x] Keep outbound buffered-fetch helpers in core: they are live outbound APIs and are not part
  of downstream response buffering cleanup.
- [x] Run cross-adapter conformance cases for streamed declared-length early EOF, one-byte overrun,
  partial native writes, HEAD/status suppression, deadline equality, fallback accounting, and
  original-cause preservation.
- [x] Run the legacy script, docs contract script, adapter tests, and `git diff --check`.

## Task 9: Align capability evidence and public documentation

**Files:**
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/guide/architecture.md`
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/adapters/overview.md`
- Modify: `docs/guide/adapters/axum.md`
- Modify: `docs/guide/adapters/cloudflare.md`
- Modify: `docs/guide/adapters/fastly.md`
- Modify: `docs/guide/adapters/spin.md`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: adapter capability declarations in each `src/cli.rs`

- [x] Add tests that compare adapter capability declarations with the public matrix and reject a
  `Native` row lacking a named evidence fixture.
- [x] Document each adapter's commit point, byte-acceptance boundary, terminal success meaning,
  cancellation primitive, deadline limitation, provider-owned allocation exclusion, and probe
  status.
- [x] Remove language describing the old buffering fallback or response-object handoff as current
  behavior. Keep historical implementation plans historical and clearly non-normative.
- [x] Record target/runtime versions and tolerances for any deployed probes that were actually
  run. No deployed probe was run in this implementation session, and no capability was promoted
  based only on compilation, host mocks, or local runtime behavior.
- [x] Run `node scripts/check_outbound_docs_contract.mjs`, docs lint, and docs build.

## Task 10: Broad verification and adversarial self-review

- [x] Run focused adapter and core tests after the final cleanup.
- [x] Run all repository gates:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

- [x] Run demo and generated-project checks, legacy/API checks, docs checks, `git diff --check`,
  and inspect `git status --short` for unintended files.
- [x] Perform a direct dead-code, duplication, cfg-boundary, and build-matrix review. Separate
  review agents were unavailable in this session; do not claim independent-agent evidence.
- [x] Re-read every normative statement in the response-egress spec and record evidence or an
  explicit capability limitation for each one.
- [x] Report the final decision log: investigation agents, competing proposals, chosen
  runtime-specific-pump design, proof obtained per adapter, remaining provider limitations, and
  commands actually run.

## Decision And Evidence Record

- **Architecture:** Retained one core lifecycle/framing contract and four private runtime-specific
  pumps. Rejected a shared sink abstraction and every response-object or bounded-buffer fallback.
- **API migration:** Removed the old Fastly response-returning entrypoint, Spin full-response
  conversion, public Axum transport-blind service, converter-only terminal outcome, and response
  buffer constants. Demo and generated applications compile only against the send-owning APIs.
- **Capability policy:** The four response-egress guarantees remain `BestEffort` on every adapter.
  Lazy passthrough is `Native` only on Axum and Cloudflare. No local mock or compile check was used
  to promote provider behavior.
- **Target evidence:** Fastly contract and library suites passed locally under Viceroy `0.19.0`;
  Spin contract and SDK-resource suites passed under Wasmtime `45.0.0`. Those versions are newer
  than the repository pins, so CI remains the authoritative pinned-runtime run. Cloudflare WASM
  tests compile locally, but no `wasm-bindgen-test-runner` or Wrangler deployment was available.
- **Open evidence:** The deployed Workers probe and the expanded Axum socket matrix remain open.
  Their absence is reflected by the unchecked tasks and conservative capability declarations.
- **Review:** Separate review agents were unavailable. Direct review covered framing precedence,
  clock/deadline races, accepted-prefix accounting, cancellation ownership, provider diagnostics,
  cfg/feature combinations, unsafe boundaries, dependency reachability, retired APIs, demos, and
  generated projects.
