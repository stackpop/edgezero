# Outbound HTTP Phase 4: Axum and Cloudflare Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the full outbound contract for Axum and Cloudflare, including batch behavior, typed cleanup, adapter scheduling, and executable native/WASM/host contracts.

**Architecture:** Both clients consume the same core request and response pipeline. Axum uses Reqwest and bounded conversion at one Tokio blocking boundary. Cloudflare uses a target-neutral orchestration driver plus a small Worker WASM bridge that owns abort, timer, native body, and host-event yield resources through terminal completion.

**Tech Stack:** Reqwest 0.13.4, Axum/Tokio, Worker 0.8.3, Web APIs, `web-time`, `futures`, wasm-bindgen-test, workerd/deployed probes.

---

## Preconditions and Owned Files

- [ ] Phases 1b-3 pass all gates.
- [ ] The implementation index's inert workflow-bootstrap PR is merged to the default
  branch, so `.github/workflows/outbound-cloudflare-deployed.yml` can receive
  `workflow_dispatch` for an exact reviewed implementation SHA. Verify the default-branch
  workflow has only the protected-environment manual trigger; do not attempt to dispatch a
  workflow definition that exists only on this implementation branch.
- [ ] Phase 0 has also provisioned the protected `outbound-cloudflare-probe` environment,
  required reviewers, exact disposable secrets/account, fixed reviewed probe ref, and the
  authenticated observable-origin arm/case/observe/delete protocol. Task 4 cannot begin on
  workflow presence alone.
- [ ] Re-read spec §§3.1-3.5, 4.1, 4.2, 5.2-5.5, and the Axum/Cloudflare rows in §7.
- [ ] Add `web-time` and independent `test-utils = []` to both adapter manifests. `test-utils` must not enable a runtime feature.
- [ ] Modify only Axum/Cloudflare adapters, root `Cargo.toml`/`Cargo.lock`, `examples/app-demo/Cargo.toml`/`Cargo.lock`, their host fixtures, the shared nonzero-test gate, and `.github/workflows/test.yml`. The protected Cloudflare dispatcher remains the inert default-branch bootstrap workflow; no core semantics or Phase 7 templates/docs.

## Task Protocol

For each adapter task: add only the named contract cases; run its exact Task 1 command and require nonzero failing tests; implement through the production driver; rerun to zero failures; run that adapter's target check and `git diff --check`; then stage only listed files and make the stated commit. Mock transport proves orchestration only. Any cancellation, raw-wire, or host-yield claim marked host-observed requires the workerd/deployed fixture before its capability cell is published. Task 4 is the exception to the one-commit task protocol: its protected red/green proof requires two immutable commits with the exact messages named below. Keep both commits reachable and do not amend, rebase, or squash either one after recording its workflow evidence.

**Required exact test names:** `send_all_preflight_precedence_and_indices`, `one_slot_send_all_matches_send`, `send_all_starts_every_eligible_exchange`, `request_preparation_consumes_entry_budget`, `adapter_final_dispatch_reapplies_request_normalization`, `canonical_uri_wire_serialization_table`, `redirect_response_is_not_followed`, `streamed_tasks_consume_fast_body_before_slow_headers`, `request_deadline_checks_before_and_after_source_ready`, `response_pipeline_preserves_typed_limits`, `response_content_length_rejects_before_body_poll`, `response_content_length_explicit_identity_rejects_before_body_poll`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `axum_205_reads_at_most_once`, `axum_response_conversion_keeps_reactor_live`, `cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect`, `cloudflare_raw_fetch_rejects_failed_option_set_without_dispatch`, `cloudflare_origin_observes_canonical_host`, `cloudflare_stream_delivers_first_chunk_before_source_eof`, `cloudflare_frozen_clock_forces_host_yield`, `cloudflare_205_reads_at_most_once`, `cloudflare_invalid_utf8_request_header_returns_error_not_panic`, `cloudflare_valid_non_ascii_request_header_survives`, `cloudflare_abort_guard_fires_exactly_once`, and `adapter_capability_matrix_matches_outbound_spec`.

**Expected red:** contract smoke tests first fail their sentinel assertion; behavior tests then expose serial starts, followed redirects, timeout/cap misclassification, reactor starvation, transformed fetch bytes, absent abort, extra 205 reads, or panic. Host-yield proof fails by timer starvation, not by a fabricated clock advance.

### Task 1: Establish executable contract seams

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.toml`
- Modify: `examples/app-demo/Cargo.lock`
- Modify: `crates/edgezero-adapter-{axum,cloudflare}/Cargo.toml`
- Modify: `crates/edgezero-adapter-cloudflare/.cargo/config.toml`
- Create: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify/Create: both adapters' `src/test_utils.rs`
- Modify: `.github/workflows/test.yml`

- [ ] Pin Reqwest and Worker before compiling either bridge. Set root `reqwest = "=0.13.4"`. Set root and app-demo Worker requirements to exact `=0.8.3`, both with `default-features = false` and `features = ["http"]`; make the Cloudflare adapter inherit that workspace dependency while retaining `optional = true`.
- [ ] Refresh both independent lock graphs with `cargo update --offline -p reqwest --precise 0.13.4`, `cargo update --offline -p worker --precise 0.8.3`, `cargo update --offline --manifest-path examples/app-demo/Cargo.toml -p reqwest --precise 0.13.4`, and `cargo update --offline --manifest-path examples/app-demo/Cargo.toml -p worker --precise 0.8.3`.
- [ ] Assert both graphs before adding bridge code:
  - `cargo tree --offline --locked -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown | rg 'worker v0\.8\.3'`
  - `cargo tree --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown | rg 'worker v0\.8\.3'`
- [ ] Assert both Reqwest graphs resolve exactly 0.13.4:
  - `cargo tree --offline --locked -p edgezero-adapter-axum --features axum -e normal | rg 'reqwest v0\.13\.4'`
  - `cargo tree --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-adapter-axum -e normal | rg 'reqwest v0\.13\.4'`
- [ ] Add one deliberately failing smoke test to each native contract module.
- [ ] Remove Cloudflare's whole-file `cfg(all(feature = "cloudflare", target_arch = "wasm32"))`; place it only on the browser SDK module and add a native `test-utils` module, so the native command cannot succeed with zero tests.
- [ ] Replace the Cloudflare crate's stale Preview-1 Cargo config with `[build] target = "wasm32-unknown-unknown"` and an exact `wasm-bindgen-test-runner` for that target. CI installs the runner and executes one crate-local contract command without `--target` or `CARGO_TARGET_*_RUNNER`; a nonzero passing sentinel proves local target resolution consumes the committed config.
- [ ] Run:
  - `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
  - `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] Confirm each command executes a nonzero test count and fails at the assertion, not at linking/imports.
- [ ] Replace smoke failures with feature-gated seams for clocks, timers, transport/body handles, abort events, host yields, and stage delays. Production and fake transports must call the same driver.
- [ ] Commit: `test(adapters): add outbound contract seams`.

### Task 2: Implement Axum dispatch and batch behavior

**Files:**
- Create: `crates/edgezero-adapter-axum/src/outbound.rs` beside the temporary legacy `src/proxy.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`

- [ ] Add failing tests for complete batch preflight, empty batch, index alignment, partial failure, one shared `batch_now`, all valid slots started before completion, duplicate request headers, methods, canonical target use, and a 302 response returned without following its `Location`. Advance the clock during normalization/preflight/builder preparation and prove `send`/`send_all` budgets use the method-entry snapshot rather than a post-preflight re-anchor. A valid one-slot buffered `send_all` must match `send` preflight and result semantics. Include GET/HEAD body invalidity beating batch-only stream-mode errors and prove a rejected body source is never polled.
- [ ] In the production Reqwest request captured by the seam, assert exact core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and query preservation. The adapter must never rebuild these values from URL components.
- [ ] Add upload tests for exact/over request cap, source error, stalled pull, timeout before/after readiness, and no further polling after failure.
- [ ] Add `adapter_final_dispatch_reapplies_request_normalization`: mutate `Connection` nominations, `Host`, `Content-Length`, and `Transfer-Encoding` through `headers_mut` after construction, then inspect the production Reqwest request and prove the adapter called `normalize_for_dispatch` immediately before SDK construction. Only adapter-owned framing and the canonical host may remain.
- [ ] Run the Axum contract command; expect failures.
- [ ] Implement `AxumOutboundClient::{send, send_all}`. Capture the monotonic snapshot as the first operation in each public method, before normalization/preflight, and pass it through without re-anchoring. Configure Reqwest with `redirect(Policy::none())`, `no_gzip()`, `no_brotli()`, `no_deflate()`, and `no_zstd()`, and remove the client-wide 30-second timeout. Assert compressed loopback bytes reach only the shared EdgeZero decoder. Insert `Accept-Encoding: identity` only when the normalized request has no caller-supplied `Accept-Encoding`; preserve any caller value for the shared response decoder.
- [ ] Run `validate_for_dispatch` exactly once per request immediately after the method-entry snapshot and before batch-only mode checks, budget selection, body polling, normalization, or Reqwest construction. Batch survivors enter a private already-validated helper so they are not validated twice.
- [ ] Apply `RequestBuilder::timeout(remaining)` immediately before `send`. Race every streamed upload pull against the same absolute budget and buffer only within `max_request_body_bytes`.
- [ ] Use `join_all` after preflight, preserving slot order and independent results.
- [ ] Compile/export the new module for contract tests, but leave production request injection on the legacy client until Task 3 is green.
- [ ] Rerun the contract command; expect success.
- [ ] Commit: `feat(axum): implement outbound dispatch`.

### Task 3: Implement Axum response pipeline and conversion schedule

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Delete: `crates/edgezero-adapter-axum/src/proxy.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/request.rs`
- Modify: `crates/edgezero-adapter-axum/src/response.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`

- [ ] Add loopback-origin tests for repeated headers including multiple `Set-Cookie` values, raw malformed nomination/encoding lines, all content-encoding policy rows, header/encoded/decoded/Brotli limits, gzip members/native EOF, bodyless/205 cases, timeout races, future/source drop on cancellation/error/cap, and non-2xx success. Exercise HTTP/1 and HTTP/2 separately: HTTP/1 may close its connection, while HTTP/2 resets only the request stream and may retain the pooled connection. Do not assert a generic connection drop or bounded origin observation.
- [ ] Add identity/compressed `Content-Length` cases proving the shared pre-poll helper runs after normalization: encoded caps apply to every coding, decoded caps reject early for effective identity (absent or exactly one bare `identity`), and every rejection leaves the loopback body unread. Include an explicit `Content-Encoding: identity` overage.
- [ ] For Axum 205, prove positive visible length aborts without reading and absent/zero length performs at most one native read: EOF succeeds; an empty or nonempty item aborts without a second read.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Every case must return the attributed 504, retain typed late source/completion errors, and keep cleanup armed until native EOF.
- [ ] Add the streamed fan-out regression: join tasks that each perform `send` and immediately consume the body; the fast body must finish before a sibling's delayed headers. A control that joins header-only sends then delays consumption must fail.
- [ ] Add exact and one-byte-over `AXUM_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` conversion cases, preserving the original `EdgeError` status/kind.
- [ ] Add a reactor-progress regression proving no nested runtime/blocking deadlock.
- [ ] Add optional live HTTP/2 characterization through the deployed proxy path and HTTP/3 only when that production client feature is enabled. Record observed stream reset/connection reuse behavior without making it a blocking portable guarantee; the deterministic contract is guest-visible timeout plus request/body/source future drop.
- [ ] Run the Axum contract command; expect failures.
- [ ] Feed Reqwest `Response::chunk()` lazily through the core pipeline; retain method/mode/all policy fields before consuming request parts.
- [ ] Make response conversion async. Await routing and conversion inside exactly one `block_in_place(|| Handle::block_on(async { ... }))` boundary.
- [ ] On conversion failure, emit `EdgeError::into_response()`; never replace typed 502/504 errors with a generic 500.
- [ ] Switch request extensions to `HttpClient`, remove the adapter's legacy module, and do not leave a module/type alias. Do not change inbound buffering except compile-required typed stream conversion.
- [ ] Rerun Axum contract and crate tests; expect success.
- [ ] Commit: `feat(axum): enforce outbound response contracts`.

### Task 4: Prove a Cloudflare host-event yield primitive

**Files:**
- Create/Modify: `crates/edgezero-adapter-cloudflare/tests/host/outbound.mjs`
- Create: `crates/edgezero-adapter-cloudflare/tests/host/deployed.mjs`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.lock`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/wrangler.toml`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/src/lib.rs`
- Create: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/README.md`
- Modify: `crates/edgezero-adapter-cloudflare/package.json`
- Create: `crates/edgezero-adapter-cloudflare/package-lock.json`

- [ ] Put an empty `[workspace]` table in the fixture `Cargo.toml` so Cargo treats it as a standalone nested workspace rather than an undeclared member of the repository workspace.
- [ ] Pin the fixture's Worker dependency to exact `=0.8.3`. Generate and commit its independent Rust lock with `cargo check --offline --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml`; then require `cargo metadata --offline --locked --format-version 1 --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml` and `cargo tree --offline --locked --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml | rg 'worker v0\.8\.3'` before `worker-build`.
- [ ] Pin fixture `compatibility_date = "2023-05-01"` and no additional compatibility flags, exactly matching the generated production `wrangler.toml.hbs`. Add a fixture assertion that parses both files and fails if their date/flags diverge.
- [ ] Pin `worker-build = 0.8.3` in the fixture instructions/CI setup and add package script
  `build:outbound-fixture` running
  `worker-build --release tests/fixtures/outbound-worker`. The positional path selects the
  nested fixture crate; do not pass `--manifest-path`, which is only a forwarded Cargo option
  after `worker-build` has already selected a crate. Record `worker-build --version` and
  `wrangler --version` in the probe output.
- [ ] Add exact package scripts: `test:workerd` invokes `node tests/host/outbound.mjs --mode workerd`; `test:deployed-timing` invokes `node tests/host/deployed.mjs`. Neither script may turn absent credentials, zero probes, or unsupported host behavior into success.
- [ ] Generate and commit the adapter's first npm lockfile with `npm --prefix crates/edgezero-adapter-cloudflare install --package-lock-only --ignore-scripts`; subsequent host jobs use `npm ci` and may not float Wrangler independently of that lock.
- [ ] Build a deployed timing probe with a frozen guest clock and a continuously-ready stream. Compare candidate host-event yields; microtask/self-wake/immediately-ready futures are invalid.
- [ ] Establish an executable red in commit `test(cloudflare): add failing host-yield candidate`:
  use a microtask/self-wake/immediately-ready yield, set `RED_SHA=$(git rev-parse HEAD)`, and
  push that exact tree with
  `git push origin HEAD:refs/heads/outbound-probe-reviewed`; the protected ref must accept it
  only as a fast-forward. Dispatch the
  default-branch workflow with
  `RED_RUN_URL="$(gh workflow run outbound-cloudflare-deployed.yml --ref main -f commit_sha="$RED_SHA")"`,
  derive `RED_RUN_ID="${RED_RUN_URL##*/}"`, reject an empty/non-URL result, and run
  `gh run watch "$RED_RUN_ID" --exit-status`. Require that command to return nonzero because
  the named frozen-clock timer probe starves while every
  setup and positive-control probe executes. Missing credentials, zero probes, build failure,
  or an unrelated assertion is not the expected red. Record the workflow URL and `RED_SHA`
  before changing the reviewed ref.
- [ ] Select one candidate host-event primitive and freeze one quota in `1..=64` in commit
  `test(cloudflare): characterize outbound host yielding`. Set
  `GREEN_SHA=$(git rev-parse HEAD)`, push that exact tree to the same reviewed ref with
  `git push origin HEAD:refs/heads/outbound-probe-reviewed`, require the update to be a
  fast-forward from the red commit, and dispatch with
  `GREEN_RUN_URL="$(gh workflow run outbound-cloudflare-deployed.yml --ref main -f commit_sha="$GREEN_SHA")"`,
  derive `GREEN_RUN_ID="${GREEN_RUN_URL##*/}"`, reject an empty/non-URL result, and require
  `gh run watch "$GREEN_RUN_ID" --exit-status` to succeed. Require every named probe plus a
  positive count to pass, including timer and abort delivery under the frozen guest clock.
  Record the passing workflow URL, `GREEN_SHA`, primitive, quota, runtime, and observed bound
  before starting Task 5. The green commit is Task 4's final commit; do not create a third
  summary commit or rewrite either evidence-bearing SHA.
- [ ] If no candidate passes on the pinned Worker runtime, STOP Phase 4 and downgrade/re-review the Cloudflare deadline capability before implementation.

### Task 5: Implement the Cloudflare target-neutral driver

**Files:**
- Create: `crates/edgezero-adapter-cloudflare/src/outbound.rs` beside the temporary legacy `src/proxy.rs`
- Create: `crates/edgezero-adapter-cloudflare/src/outbound/worker.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [ ] Add failing native tests for full batch preflight/order/isolation, one-slot `send_all`/`send` equivalence, one method-entry `batch_now`, upload caps/errors/timeouts, send/body timeout races, encoded/decoded fairness, frozen-clock terminal decisions, abort ownership, every early-return/drop path, and 3xx visibility without redirect dispatch. Advance the injected clock during request preparation and prove that elapsed time consumes the original `send`/`send_all` budget. Include GET/HEAD body errors beating batch-only errors and no rejected-source polls.
- [ ] Through the target-neutral `NativeBody`/header seam, cover every content-encoding row, effective-identity pre-poll rejection including explicit `identity`, repeated `Set-Cookie`, 205 settlement, first-chunk-before-EOF, typed completion, and decoder/cap/deadline ownership. These tests own orchestration; Task 7 repeats Workers-only SDK response boundaries under workerd.
- [ ] Capture the exact canonical string supplied to `worker::Request` and assert core
  serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA
  input, empty paths, and queries; no bridge-side URL reconstruction is allowed. Do not call
  this the final wire URL: the Web API may perform a second WHATWG parse/serialization.
  Task 7 separately asserts the effective origin-observed target and Host against the
  corresponding canonical semantics without requiring byte equality to the input string.
- [ ] Run the Cloudflare native contract command; expect failure.
- [ ] Implement one driver parameterized by `Clock`, `Timer`, `HostYield`, `RawFetch`, `NativeBody`, and `AbortHandle`. Capture the clock as the first operation in each public send method, before normalization/preflight, and never re-anchor. Count ready items including empty chunks; preserve quota state across polls and reset only after the selected host event completes.
- [ ] Run `validate_for_dispatch` exactly once per request immediately after the method-entry snapshot and before batch-only checks, budget selection, body polling, normalization, or Worker request construction. Batch survivors enter a private already-validated helper.
- [ ] Keep the owning abort guard armed through decoder completion and native EOF. Disarm only on full success.
- [ ] Implement concurrent eligible `send_all` exchanges with complete preflight and stable indices.
- [ ] Compile/export the new module for contracts, but leave production request injection on the legacy client until Task 6 is green.
- [ ] Rerun native contract tests; expect success.
- [ ] Commit: `feat(cloudflare): add outbound exchange driver`.

### Task 6: Add Worker raw fetch and response bridges

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound/worker.rs`
- Delete: `crates/edgezero-adapter-cloudflare/src/proxy.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [ ] Add browser WASM tests that inspect `RequestRedirect::Manual` on the constructed `request.inner()` separately from `signal` plus `encodeResponseBody: "manual"` on the final fetch initializer; the latter must not reset redirect. Preserve request header list semantics and compile/exercise only portable Web API bridge construction.
- [ ] Do not call `worker::Headers::get_all` or claim real Worker SDK response conversion in the browser runner: Worker 0.8.3 binds the Workers-only `Headers.getAll()` API without a catch boundary. Compile the production response bridge here, but execute its real header/body conversion, repeated `Set-Cookie`, and checked completion assertions in Task 7's workerd fixture.
- [ ] Keep payload-length/decode/cap/body-disposition behavior in the target-neutral native suite. The workerd suite repeats the SDK boundary cases proving malformed/conflicting lengths and encoded/effective-identity overages reject before a Worker body stream is polled.
- [ ] Ensure the production bridge composes the same native-tested pipeline in both Buffered and Streamed modes. Task 7 runs real Worker cases stalled before decoded output, midstream, and after codec EOF but before native EOF; those cases must retain attributed 504, typed late source/completion errors, and exactly-once abort ownership.
- [ ] Add the same joined send-plus-immediate-body-consumption regression as Axum to the native driver suite. The first response chunk before source EOF is repeated through the real Worker response bridge in Task 7; collecting the whole body is not sufficient evidence.
- [ ] Add `adapter_final_dispatch_reapplies_request_normalization` at the raw-fetch boundary with post-construction `Connection` nominations, stale `Host`, `Content-Length`, and `Transfer-Encoding`. Assert stale Host is removed, no replacement Host header is inserted, and the exact canonical URL string is supplied when constructing the Web request. For headers, exercise request list semantics, preserve valid non-ASCII UTF-8 strings such as `café`, reject a raw invalid-UTF-8 value with an error rather than a panic, and assert only Cloudflare's visible normalized response-string baseline without claiming unavailable raw octets or final wire-URL byte equality.
- [ ] Inject raw-JS option-set failures and prove both a thrown `Reflect::set` and `Ok(false)` return typed Internal without invoking fetch. Also test checked JS-response conversion failure.
- [ ] The browser command must list and execute a nonzero portable bridge sentinel, but it is compile evidence only for the Workers-specific response path. Task 7's workerd suite is the required execution gate for that path.
- [ ] Run the WASM contract command; expect failure before implementation and nonzero success afterward.
- [ ] Implement lazy `Body::Stream` output and the raw JS/Web fetch bridge. Do not add direct Web dependencies unless the pinned Worker re-exports prove insufficient.
- [ ] Switch request extensions to `HttpClient`, delete the legacy module, and leave no module/type alias.
- [ ] Remove production `brotli`/`flate2` only after no production callsite remains.
- [ ] Commit: `feat(cloudflare): bridge raw outbound fetches`.

### Task 7: Land host evidence and CI gates

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/host/outbound.mjs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/host/deployed.mjs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/wrangler.toml`
- Modify: `crates/edgezero-adapter-cloudflare/package.json`
- Modify: `crates/edgezero-adapter-cloudflare/package-lock.json`
- Modify: `crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/README.md`
- Create: `scripts/run_test_nonzero.sh`

- [ ] Run workerd and deployed-origin tests for real Worker SDK response conversion, Workers-only `Headers.getAll("set-cookie")`, repeated `Set-Cookie`, origin-observed effective URL/Host agreement after the Web API's WHATWG normalization, send-future/body cancellation, early consumer drop, cap/decode failure, first chunk before source EOF, gzip/br decoding exactly once in Buffered and Streamed modes, effective-identity `Content-Length` rejection before body polling, the complete passthrough encoding matrix, raw upstream receipt under manual fetch encoding, downstream `EncodeBody::Manual`, 205 settlement, and selected host-yield/frozen-clock timing. Include dot-segment, numeric-IPv4, IDNA, percent-encoding, empty-path, and query rows. No browser-only result substitutes for any of these cases.
- [ ] Make `tests/host/outbound.mjs` require every exact probe identifier, print `PASS <identifier>` for each, print a positive final count, and fail on missing/duplicate/zero probes. Include `repeated_set_cookie_survives_response_conversion`, `cloudflare_origin_observes_canonical_host`, `cloudflare_stream_delivers_first_chunk_before_source_eof`, `response_content_length_explicit_identity_rejects_before_body_poll`, `cloudflare_205_reads_at_most_once`, and `cloudflare_frozen_clock_forces_host_yield`.
- [ ] Record runtime/tool versions and observed bounds in fixture output or adjacent README comments. Mocks do not satisfy this gate.
- [ ] Before `worker-build`, CI runs the fixture's locked metadata and Worker tree assertions from Task 4. Install the fixture builder with `cargo install worker-build --version 0.8.3 --locked`; verify `worker-build --version` before invoking `build:outbound-fixture`. Assert the fixture's compatibility date/flags match the production template. The README local command and CI use those same exact versions/settings.
- [ ] Keep deterministic workerd coverage in the normal untrusted `pull_request` workflow.
  Use the already-merged default-branch
  `.github/workflows/outbound-cloudflare-deployed.yml` bootstrap dispatcher with **only**
  `workflow_dispatch`, `contents: read`, and the protected `outbound-cloudflare-probe`
  environment; never use `pull_request_target`. It requires `commit_sha`, checks out that
  exact full SHA, and fails unless `git rev-parse HEAD` is byte-for-byte equal before any
  probe command receives secrets. Do not modify the workflow on this implementation branch
  and mistake that branch-only file for executable dispatch infrastructure.
- [ ] The protected job uses only a disposable Workers account and independently observable disposable origin. Define exact secrets `CLOUDFLARE_API_TOKEN` (Workers Scripts edit limited to that account), `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_WORKERS_SUBDOMAIN`, `OUTBOUND_PROBE_ORIGIN_URL`, and `OUTBOUND_PROBE_ORIGIN_TOKEN`; production account tokens, zones, routes, and origins are forbidden. Install Node/Rust from `.tool-versions`, run `npm ci`, assert both lock graphs and Worker 0.8.3, install `worker-build 0.8.3 --locked`, assert tool versions, build the fixture, then run `npm --prefix crates/edgezero-adapter-cloudflare run test:deployed-timing`.
- [ ] Implement the Phase 0-frozen origin protocol in the fixture README and driver:
  authenticated `POST /v1/probes/{run_id}/arm` creates isolated state; Worker requests use
  `/v1/probes/{run_id}/{case_id}` for raw bytes, delayed headers/chunks, stalled upload reads,
  and cancellation; authenticated `GET /v1/probes/{run_id}/observations` returns timestamped
  bytes/EOF/disconnect facts; authenticated `DELETE /v1/probes/{run_id}` removes them. Every
  request carries an unguessable run ID and token. The driver creates a unique Worker name
  from `GITHUB_RUN_ID` + `GITHUB_RUN_ATTEMPT`, polls its `workers.dev` URL, requires every
  exact probe ID plus a positive final count, and verifies cancellation/timing from origin
  observations rather than client timing alone. Changing that protocol requires updating and
  re-reviewing Phase 0 before dispatch.
- [ ] Put Worker deletion and origin-state deletion in `deployed.mjs` `finally` blocks. A failed assertion must still attempt both cleanups; a cleanup failure is reported and fails the job. The script must reject missing variables before deployment and must never reuse a fixed Worker name or observation namespace.
- [ ] Create executable POSIX `scripts/run_test_nonzero.sh <sentinel> <command...>`. It first invokes the command with `-- --list`, requires the sentinel as the exact terminal test-name component (an optional Rust module prefix ending in `::` is allowed) and at least one listed test, then invokes the command normally; list or execution failure propagates. Match a listed test identifier, not arbitrary diagnostic text. Use it for every native, browser-WASM, and capability-table Cargo test added to CI so cfg/feature drift cannot turn a gate into zero-test success.
- [ ] Add native Axum/Cloudflare commands and browser-WASM `test-utils` activation to CI through that helper. Add and execute explicit capability-table unit commands so feature-gated `cli.rs` tests run rather than merely compile under clippy:
  - `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
  - `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] Add the generated Cloudflare adapter target check to CI:
  `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-cloudflare --no-default-features --features cloudflare --target wasm32-unknown-unknown)`.
- [ ] Keep the workerd host suite and the trusted-environment deployed cancellation/timing probe as named CI jobs. Capability publication is blocked unless the recorded deployed job passes; native mocks or a credential-skipped zero-test result cannot substitute.
- [ ] For fork PRs, run only non-secret native/browser/workerd jobs automatically. A maintainer reviews the exact commit, reproduces and pushes that tree to `refs/heads/outbound-probe-reviewed`, dispatches the protected workflow for its SHA, and records the successful workflow URL and SHA on the PR. A skipped or mismatched-SHA run cannot publish Cloudflare's Native timing/upload/lazy-stream cells.
- [ ] Add table-driven capability tests and publish the overrides only now. Axum: Native for HTTP, header fidelity, deadlines, flexible phase budget, slot isolation, and upload deadlines; BestEffort for lazy response passthrough. Cloudflare: Native for HTTP, deadlines, flexible phase budget, slot isolation, upload deadlines, and lazy response passthrough; BestEffort for header fidelity. Both wildcard future capabilities to Unsupported.
- [ ] Commit: `ci: enforce axum and cloudflare outbound contracts`.

## Phase Verification

- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `env CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract`
- [ ] `(cd crates/edgezero-adapter-cloudflare && ../../scripts/run_test_nonzero.sh cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect cargo test --offline --locked --no-default-features --features cloudflare,test-utils --test contract)`; no target or runner override is permitted.
- [ ] `cargo check --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare --target wasm32-unknown-unknown`
- [ ] `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-cloudflare --no-default-features --features cloudflare --target wasm32-unknown-unknown)`
- [ ] `cargo metadata --offline --locked --format-version 1 --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml`
- [ ] `cargo tree --offline --locked --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml | rg 'worker v0\.8\.3'`
- [ ] `npm ci --prefix crates/edgezero-adapter-cloudflare`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run build:outbound-fixture`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run test:workerd`
- [ ] Push the reviewed phase HEAD to `refs/heads/outbound-probe-reviewed`, dispatch the
  default-branch `.github/workflows/outbound-cloudflare-deployed.yml` with
  `commit_sha=$(git rev-parse HEAD)`, and require a successful workflow URL whose checked-out
  SHA matches exactly. Directly running the credentialed npm script is not a substitute for
  protected-environment, exact-SHA, and cleanup-path evidence.
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: Axum and Cloudflare no longer expose adapter `proxy` modules, publish their reviewed capability rows atomically with passing contracts, and Cloudflare's Native timing claim is backed by host-observed evidence.
