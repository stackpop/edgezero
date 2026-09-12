# Outbound HTTP Phase 6: Fastly Dispatch and Harvest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Fastly outbound HTTP with dispatch-all-before-wait fan-out, deterministic dynamic backends, host timer derivation, typed errors, streamed upload ownership, and bounded downstream conversion.

**Architecture:** A target-neutral synchronous engine owns slots, backend identity/cache behavior, timing arithmetic, and cleanup. The SDK layer supplies `PendingRequest`, backend registration, and body handles. Batch dispatch is a distinct first phase; ordered harvest may opportunistically poll later slots but never loses a result.

**Tech Stack:** Fastly SDK 0.12.1, `web-time`, `sha2`, core typed streams/decoders, native `test-utils`, wasm32-wasip1/Viceroy.

---

## Preconditions

- [ ] Phases 1b-5 pass all gates.
- [ ] The implementation index's inert workflow-bootstrap PR is merged to the default
  branch, so `.github/workflows/outbound-fastly-characterization.yml` can receive
  `workflow_dispatch` for an exact reviewed implementation SHA. Do not rely on a workflow
  definition introduced only on this implementation branch.
- [ ] Phase 0 has also provisioned the protected `outbound-fastly-probe` environment,
  required reviewers, exact disposable secrets, enabled/disabled services, fixed reviewed
  probe ref, and authenticated observable-origin protocol. Workflow presence alone is not a
  runnable host gate.
- [ ] Re-read spec §4.3, Fastly rows in §§5.2-5.5, and the Fastly file summary in §7.
- [ ] Keep outbound HTTP, deadlines, flexible phase budgets, slot isolation, streamed-upload deadlines, and lazy response passthrough at the exact reviewed BestEffort levels. Dynamic-backend service enablement is not statically provable, so `outbound-http` cannot be published as Native.
- [ ] Confirm `OutboundRequest::host_name()` exists from Phase 1b; never reparse the URI to construct backend identity.
- [ ] Require `viceroy --version` to print exactly the repository-pinned `0.17.0`. Run WASM tests from `crates/edgezero-adapter-fastly` so its committed `.cargo/config.toml` supplies both `wasm32-wasip1` and `viceroy run -C ../../examples/app-demo/crates/app-demo-adapter-fastly/fastly.toml --`; no CI environment override may replace that service configuration.

## Task Protocol

For each task: add only the named native or SDK-gated cases; run the exact native/WASM command and require a nonzero failure; implement through the shared production engine; rerun focused and package tests; run `git diff --check`; then stage only listed files and make the stated commit. Fake handles establish stage order and cleanup, not real host cancellation or a finite write bound.

"Run native contracts" means
`scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`.
"Run Fastly WASM contracts/library tests" means the two crate-local `wasm32-wasip1`
commands in Phase Verification with `fastly,test-utils`. Every command must execute a nonzero test count; a
target/linker error or a zero-test result is failure.

**Required exact test names:** `backend_identity_uses_every_canonical_property`, `backend_creation_error_table_is_exhaustive`, `backend_builder_keeps_pooling_for_identical_identity_and_settings`, `dispatch_guard_checks_expiry_before_slack`, `send_all_dispatches_every_slot_before_wait`, `send_all_reports_per_slot_elapsed`, `one_slot_send_all_matches_send`, `request_preparation_consumes_entry_budget`, `adapter_final_dispatch_reapplies_request_normalization`, `canonical_uri_wire_serialization_table`, `send_all_reverse_completion_preserves_order`, `done_error_survives_poll_sweep`, `send_error_cause_table_preserves_timeout_source`, `response_content_length_rejects_before_body_poll`, `response_content_length_explicit_identity_rejects_before_body_poll`, `response_read_checks_deadline_after_eof`, `repeated_set_cookie_survives_response_conversion`, `decoder_stalls_timeout_at_all_completion_boundaries`, `streamed_upload_finishes_exactly_once`, `streamed_upload_failure_never_waits`, `downstream_fallback_preserves_typed_error_envelope`, and `adapter_capability_matrix_matches_outbound_spec`.

**Expected red:** the old client waits during dispatch, backend names/settings collide, error variants map to generic 500, an EOF read escapes deadline checks, or failed uploads finish/wait. SDK-gated tests must fail assertions, not report zero tests.

**Engine skeleton:**

```rust
enum Slot {
    Done(OutboundSlotResult),
    Pending(PendingSlot),
    Taken,
}

struct BackendIdentity {
    budget_ms: u32,
    host: String,
    port: u16,
    scheme: Scheme,
    tls_mode: TlsMode,
}
```

Phase one converts every valid slot to `Pending` or `Done`; no wait occurs. Phase two waits in input order, polls later pending slots opportunistically into `Done`, and replaces an unresolved `Taken` with internal error rather than panicking.

### Task 1: Establish the engine and executable seams

**Files:**
- Modify: `Cargo.toml`
- Modify: `examples/app-demo/Cargo.toml`
- Modify: `crates/edgezero-adapter-fastly/Cargo.toml`
- Create: `crates/edgezero-adapter-fastly/src/outbound.rs` beside temporary `src/proxy.rs`
- Create: `crates/edgezero-adapter-fastly/src/outbound/{backend,engine,sdk,test_utils}.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `Cargo.lock`, `examples/app-demo/Cargo.lock`

- [ ] Pin root and app-demo `fastly` requirements to exact `=0.12.1`; refresh both locks with `cargo update --offline -p fastly --precise 0.12.1` and `cargo update --offline --manifest-path examples/app-demo/Cargo.toml -p fastly --precise 0.12.1`. Assert both `cargo tree --offline --locked` graphs resolve exactly `fastly v0.12.1` before writing exhaustive SDK mappings.
- [ ] Add direct `web-time` and independent `test-utils = []`; do not enable Fastly through test-utils.
- [ ] Move whole-file WASM gating to the SDK test module. Add a native deliberately failing smoke test and prove the native command executes it.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`; expect a nonzero test count and failure at the smoke assertion, not at linking/imports.
- [ ] Define injectable clock, dispatch-stage callback, backend registrar, synchronous pending/body handles, and hash function. Public retained callbacks must remain `Send + Sync`.
- [ ] Production and tests must call the same generic engine; test code may supply effects, not copy algorithms.
- [ ] Replace the sentinel with a passing seam-construction test and rerun the same native command; require a nonzero test count and success before committing.
- [ ] Commit: `test(fastly): add outbound engine seams`.

### Task 2: Implement backend identity and host timers

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/backend.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing native tests for DNS/IP/IPv6/plain/TLS identities, default/explicit ports, exact ceil-to-ms budgets, SHA-256 first-128-bit names, dedup, forced hash collision, externally occupied `NameInUse`, and concurrent lookup. Add a production-builder assertion that no `.enable_pooling(false)` override is applied: identical names/settings may use the SDK default pool, while distinct budgets/settings produce distinct identities.
- [ ] Add timer tests: for total >= 4 ms, connect = total/4, first byte = remainder, between = total; below 4 ms, connect/first each equal total.
- [ ] In the Fastly-featured SDK test module, add an exhaustive no-wildcard table over the real `fastly::backend::BackendCreationError`: `Disallowed` -> actionable unspecified 502; `NameInUse` -> cache/collision protocol; all three `*TimeoutTooLarge`, `NameTooLong`, and `EncodingError` -> internal 500; `HostError` -> unspecified 502. Native tests may exercise an adapter-owned policy model but cannot claim coverage of the SDK enum.
- [ ] Add guard-order tests immediately before dispatch: absolute expiry -> attributed 504; live-deadline setup over `BATCH_DISPATCH_SLACK_MAX` -> internal 500; no deadline uses the default budget without the live-deadline slack failure.
- [ ] Run native contracts; expect a nonzero failure. From `crates/edgezero-adapter-fastly`, run `../../scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked --no-default-features --features fastly,test-utils --lib`; expect a nonzero compile/assertion failure from the missing real-SDK classifier. Do not override the committed target/runner.
- [ ] Implement `BackendIdentity(scheme, host_name, resolved_port, tls_mode, budget_ms)` and `ez_` names. Cache name -> `(identity, backend)` behind `Mutex`; never hold the lock across `finish()`. Leave SDK connection pooling at its default: Fastly reuses only same-name, exact-same-settings backends, and the identity/settings already separate budgets and TLS modes.
- [ ] A cache miss followed by `NameInUse` fails closed. Do not recover with string matching or `Backend::from_name`.
- [ ] Implement the complete creation-error mapping separately from `SendErrorCause`; DNS/TLS/connect failures belong only to the send-stage table.
- [ ] Configure SNI only from `sni_hostname`; certificate checking from `cert_host`, including HTTPS IP literals.
- [ ] Rerun native contracts and the filtered Fastly WASM library test; each must execute a nonzero test count and succeed.
- [ ] Commit: `feat(fastly): configure deterministic outbound backends`.

### Task 3: Implement two-phase send_all

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing cases for empty batch, complete preflight and index alignment, every valid dispatch before first wait, one-slot `send_all`/`send` outcome equivalence, reverse completion with stable order, partial dispatch/wait/body errors, preservation of pre-existing `Done` results during poll sweeps, one method-entry `batch_started_at`, distinct per-slot elapsed values, and unresolved-slot internal error. Advance the clock during normalization/preflight/backend preparation and prove the original `send`/`send_all` snapshot owns the budget and elapsed measurement. Preflight/dispatch failures sample immediately; an opportunistically polled early completion keeps its own elapsed value while an earlier index blocks later vector return; backwards injected time yields zero plus Internal, while legitimate same-tick completion may also be zero with its ordinary outcome. Include GET/HEAD body errors beating batch-only errors and no polls of rejected source streams.
- [ ] Capture the exact SDK request target and assert it matches core serialization for dot segments, percent-encoded delimiters, numeric IPv4 aliases, IDNA input, empty paths, and queries; Fastly may derive backend identity from accessors but must not reconstruct the request URI.
- [ ] Add `adapter_final_dispatch_reapplies_request_normalization`: mutate `Connection` nominations plus stale `Host`, `Content-Length`, and `Transfer-Encoding` through `headers_mut`, then inspect the final Fastly request and prove normalization ran immediately before SDK construction. Only canonical host override and adapter-owned framing may remain.
- [ ] Add serial-harvest cases showing a later slot can expire while waiting behind an earlier slot without cancellation or Native slot-isolation claims.
- [ ] Run native contracts; expect failure.
- [ ] Implement `Slot::{Done, Pending, Taken}`. Capture the monotonic snapshot as the first operation in each public send method, then run `validate_for_dispatch` exactly once per request before batch-only checks, budget selection, body polling, normalization, backend construction, or SDK request construction; never re-anchor. Construct `OutboundSlotResult` at each slot's actual terminal point, including preflight/dispatch errors and immediately after wait/poll harvest; final vector assembly performs no clock read. Issue every valid `send_async` sequentially before the first wait; pinned Fastly 0.12.1 returns when transmission begins and continues buffered upload in the background, so a stalled buffered upload may leave its slot unresolved but must not be modelled as synchronously preventing later dispatch calls. Then wait in input order and opportunistically poll later pending slots.
- [ ] Preserve all policy/method/mode fields in `PendingSlot`. Resolve every output index without panic. Do not use SDK `select()`, which cannot preserve stable slot identity.
- [ ] Harvest every successfully dispatched buffered batch request even when siblings fail.
- [ ] Rerun native contracts; expect success.
- [ ] Commit: `feat(fastly): dispatch fanout before ordered harvest`.

### Task 4: Map SDK errors and process responses

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add SDK-gated construction tests for every known public `SendErrorCause` listed in
  §4.3, with a mandatory future `_` classifier arm. Split timeout policy into
  `BudgetedTimeout` (`ConnectionTimeout`, `HttpResponseTimeout`) and `ProviderTimeout`
  (`DnsTimeout`). Pass the absolute deadline, selected cause, and injected observation
  instant into the pure classifier; test all four `BudgetSource` values before, exactly at,
  and after expiry. Configured phase timers stay attributed; pre-deadline DNS timeout is 504
  with `BudgetSource::Unspecified`; at/after expiry the selected cause wins for every result.
- [ ] Add native response cases for cumulative guest-visible headers, repeated request fields, repeated response `Set-Cookie`, raw malformed response nomination/encoding, 3xx returned without a follow-up request, bodyless/205, encoded bytes, decoded output, Brotli window/decoder-state charge, independent final Buffered limits, encoding policy, gzip members/native EOF, errors, deadlines before/after each blocking read including EOF, and drop-on-early-termination. Unequal decoded/final caps prove raw passthrough bypasses only decoded accounting; `max_chunk_bytes` remains an emitted-item guarantee.
- [ ] Add identity/compressed/passthrough `Content-Length` cases proving malformed/conflicting values and encoded/effective-identity decoded overages reject before the first Fastly body read; include exactly one bare `Content-Encoding: identity`. Effective identity compares encoded, decoded, and final Buffered caps; raw passthrough compares encoded plus final Buffered but never decoded; compressed-to-decode compares only encoded.
- [ ] In both Buffered and Streamed modes, stall gzip and Brotli before decoded output, midstream, and after codec EOF but before native EOF. Assert attributed 504 after the blocking read returns, no decoder-end success before native EOF, and late typed source/completion preservation when observed in budget.
- [ ] Run native and Fastly WASM library/contract tests; expect failures.
- [ ] Map configured phase timeout -> attributed 504; early unconfigured DNS timeout ->
  unattributed 504; any result at/after absolute expiry -> attributed 504;
  DNS/destination/connect/TLS establishment before a response head -> typed 502
  `Unreachable`; later connection/I/O -> `Transport`; response framing -> `Protocol`;
  shared JSON/gzip/Brotli failures preserve exact coding identity; local invariant/platform internal -> 500; unspecified
  known/future -> unspecified 502 as specified. Define one
  `DYNAMIC_BACKENDS_DISABLED_MESSAGE` constant with the exact spec diagnostic and use it for
  `BackendCreationError::Disallowed` and the deployed disabled-service assertion.
- [ ] Apply the core response pipeline in order. Drop the owned native body on cap/decode/deadline/consumer termination; do not drain after failure and do not claim finite origin cancellation.
- [ ] Rerun native/WASM tests; expect success.
- [ ] Commit: `feat(fastly): classify and harvest outbound responses`.

### Task 5: Implement streamed uploads

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/outbound/engine.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound/sdk.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Add failing tests for source/cap/pre-read/post-read/write/flush failures, future drop, clean EOF, `finish` exactly once, finish failure, post-finish expiry, and no wait after failed finish.
- [ ] Retain writer and pending handles together. Every failure drops both without `finish`/`wait`; clean EOF finishes once and only then permits wait.
- [ ] Document that source pulls and host writes can stall beyond the cooperative check and that partial bytes may already be delivered.
- [ ] Rerun native/WASM tests; expect success.
- [ ] Commit: `feat(fastly): own streamed outbound uploads`.

### Task 6: Migrate request injection and downstream conversion

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Delete: `crates/edgezero-adapter-fastly/src/proxy.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [ ] Inject one `HttpClient::with_client(FastlyOutboundClient::new())` per Fastly session so the backend cache lifetime mirrors the SDK namespace.
- [ ] Delete the legacy module during this switch and leave no module/type alias.
- [ ] Add exact/over `FASTLY_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` tests and typed stream-error envelope tests.
- [ ] Implement bounded fallback before platform headers are committed. Preserve original status/kind on failure.
- [ ] Rerun adapter and workspace tests; expect success.
- [ ] Commit: `feat(fastly): wire outbound client and response fallback`.

### Task 7: Land CI gates

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Create: `crates/edgezero-adapter-fastly/tests/host/deployed.sh`
- Create: `crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml`
- Create: `crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.lock`
- Create: `crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/fastly.toml`
- Create: `crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/src/main.rs`
- Create: `crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/README.md`

- [ ] Add the native contract command and enable `test-utils` in Fastly WASM contract/library runs. Execute every test target through `scripts/run_test_nonzero.sh` with one exact sentinel; retain production target checks.
- [ ] Assert `viceroy --version` is exactly `0.17.0`, then execute both WASM suites from the adapter crate without `CARGO_TARGET_*_RUNNER`; this proves the checked-in runner and demo `fastly.toml` are used locally and in CI.
- [ ] Add the generated Fastly adapter target check:
  `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-fastly --no-default-features --features fastly --target wasm32-wasip1)`.
- [ ] Add a standalone deployed-probe crate with an empty `[workspace]`, exact
  `fastly = "=0.12.1"`, path dependencies on the production adapter/core, and its own
  committed lock. The binary directly exercises `FastlyOutboundClient` for every named
  probe case; it must not depend on the not-yet-migrated app-demo handler. Pin the package
  name and output to
  `tests/fixtures/outbound-fastly/pkg/edgezero-outbound-probe.tar.gz`. Generate the fixture
  lock once with
  `cargo check --offline --manifest-path crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml --target wasm32-wasip1`;
  subsequent metadata, tree, and build commands are locked. Its `fastly.toml` build script
  runs Cargo with `--offline --locked --profile release --target wasm32-wasip1`.
- [ ] Before any protected dispatch, run locked metadata/tree checks for the fixture, require
  exactly Fastly SDK 0.12.1, and build without secrets using
  `fastly compute build --non-interactive --dir crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly`.
  Require the fixed package path to exist. `deployed.sh` accepts only that prebuilt package;
  it exits before deployment if the artifact is absent and contains no build, dependency
  download, or package-selection fallback while secrets are present.
- [ ] Confirm each command lists and executes nonzero tests.
- [ ] Add and execute `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`; clippy compilation alone does not execute feature-gated capability tests.
- [ ] Add table-driven capability tests and publish Fastly's override only now: Native for header fidelity; BestEffort for outbound HTTP, deadlines, flexible phase budget, slot isolation, streamed-upload deadlines, and lazy response passthrough; Unsupported for complete resource accounting; future capabilities -> Unsupported.
- [ ] Use the already-merged default-branch trusted characterization dispatcher with the protected `outbound-fastly-probe` environment. It accepts an exact commit SHA, checks out and verifies that SHA, installs Fastly CLI `15.1.0`, and uses only disposable `FASTLY_DYNAMIC_SERVICE_ID`, `FASTLY_DYNAMIC_SERVICE_URL`, `FASTLY_DISABLED_SERVICE_ID`, `FASTLY_DISABLED_SERVICE_URL`, `FASTLY_API_TOKEN`, `OUTBOUND_PROBE_ORIGIN_URL`, and `OUTBOUND_PROBE_ORIGIN_TOKEN` secrets. Never use `pull_request_target`, expose secrets to a fork checkout, or modify a branch-only workflow and assume it is dispatchable.
- [ ] `tests/host/deployed.sh` records each service's prior active version, deploys the fixed prebuilt probe package to both disposable services, sends a unique run ID to an authenticated observable origin, and requires exact positive probe IDs for canonical arbitrary destinations, request method/body, timeout phases, upload cancellation characterization, and response streaming. Characterize which `SendErrorCause` each configured connect/response timer and the provider DNS timeout produces. The enabled service must succeed; the disabled service must return the exact `DYNAMIC_BACKENDS_DISABLED_MESSAGE` typed 502 and must not contact the origin. In a trap, reactivate each recorded prior version, delete only versions created by this run, and delete this run's origin observations; never delete a pre-existing version. Any cleanup failure fails the job. Record the workflow URL, commit SHA, tool versions, and observed bounds in the PR. This evidence characterizes the BestEffort row; it does not promote it to Native.
- [ ] For a fork PR, run only non-secret native/Viceroy checks automatically. A maintainer first reviews, reproduces, and pushes the exact tree to `refs/heads/outbound-probe-reviewed`, then dispatches the protected workflow for that SHA. Missing live evidence does not silently skip a probe or report zero tests.
- [ ] Commit: `ci: execute Fastly outbound contracts`.

## Phase Verification

- [ ] `scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`
- [ ] `test "$(viceroy --version)" = "viceroy 0.17.0"`
- [ ] `(cd crates/edgezero-adapter-fastly && ../../scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked --no-default-features --features fastly,test-utils --test contract)`
- [ ] `(cd crates/edgezero-adapter-fastly && ../../scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked --no-default-features --features fastly,test-utils --lib)`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `cargo check --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly --target wasm32-wasip1`
- [ ] `(cd examples/app-demo && cargo check --offline --locked -p app-demo-adapter-fastly --no-default-features --features fastly --target wasm32-wasip1)`
- [ ] `cargo metadata --offline --locked --format-version 1 --manifest-path crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml`
- [ ] `cargo tree --offline --locked --manifest-path crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml | rg 'fastly v0\.12\.1'`
- [ ] `fastly compute build --non-interactive --dir crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly`
- [ ] `test -f crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/pkg/edgezero-outbound-probe.tar.gz`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] Push the reviewed phase HEAD to `refs/heads/outbound-probe-reviewed`, dispatch the
  default-branch `.github/workflows/outbound-fastly-characterization.yml` with
  `commit_sha=$(git rev-parse HEAD)`, and attach a successful workflow URL whose checked-out
  SHA matches exactly; this remains characterization of the BestEffort service prerequisite.
- [ ] `git diff --check`

Expected result: Fastly dispatches all eligible fan-out slots before harvest, preserves typed per-slot outcomes, and documents rather than overclaims its unavoidable host-blocking gaps.
