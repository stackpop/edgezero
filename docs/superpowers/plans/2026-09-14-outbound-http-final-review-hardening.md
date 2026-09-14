# Outbound HTTP Final Review Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every independently reproduced defect from the final whole-feature self-review without compatibility shims.

**Architecture:** Preserve the existing core-owned outbound, ingress, config, and response-egress contracts. Tighten validation and lifecycle ordering at their current ownership boundaries, carry the application monotonic clock through bounded store APIs, and enforce Spin platform-host drift before runtime-producing actions.

**Tech Stack:** Rust 2024, `http`, `url`, `futures`, Reqwest, Workers/Web APIs, Fastly SDK, Spin SDK/WASI HTTP, TOML manifests, Cargo and Node documentation gates.

**Execution note:** Unchecked `run_test_nonzero.sh` red-phase steps are retained as the original
TDD recipe. All implementation and final green-phase checks are recorded below.

---

### Task 1: Core protocol and stream terminal hardening

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/compression.rs`
- Modify: `crates/edgezero-core/src/response_egress_framing.rs`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`

- [x] Add failing tests named `response_normalization_rejects_non_utf8_content_length`, `typed_outbound_request_rejects_empty_userinfo`, `outbound_normalization_strips_proxy_connection`, and `terminal_stream_error_releases_source_before_yield`.
- [x] Run `scripts/run_test_nonzero.sh response_normalization_rejects_non_utf8_content_length cargo test --offline --locked -p edgezero-core response_normalization_rejects_non_utf8_content_length`, `scripts/run_test_nonzero.sh typed_outbound_request_rejects_empty_userinfo cargo test --offline --locked -p edgezero-core typed_outbound_request_rejects_empty_userinfo`, `scripts/run_test_nonzero.sh outbound_normalization_strips_proxy_connection cargo test --offline --locked -p edgezero-core outbound_normalization_strips_proxy_connection`, and `scripts/run_test_nonzero.sh terminal_stream_error_releases_source_before_yield cargo test --offline --locked -p edgezero-core terminal_stream_error_releases_source_before_yield`; confirm failure for the reviewed reason.
- [x] Preserve framing-sensitive headers until validation, apply typed-URI raw-authority checks, extend request/upstream-response/downstream-egress hop-by-hop stripping to `Proxy-Connection`, and drop terminal stream state before yielding errors.
- [x] Amend the normative field-preservation and terminal-cleanup rules for those exact behaviors.
- [x] Run `cargo test --offline --locked -p edgezero-core` and `node scripts/check_outbound_docs_contract.mjs`.

### Task 2: Axum and Cloudflare deadline ordering

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/src/response.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [x] Add failing tests named `request_timer_arms_from_final_remaining_budget` and `response_egress_post_ready_deadline_precedence` for Axum, plus `raw_fetch_timer_arms_from_final_remaining_budget`, `response_egress_preflight_precedes_writer_operation`, and `bodyless_response_checks_cancel_and_deadline_before_handoff` for Cloudflare.
- [ ] Run `scripts/run_test_nonzero.sh request_timer_arms_from_final_remaining_budget cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils request_timer_arms_from_final_remaining_budget` and `scripts/run_test_nonzero.sh response_egress_post_ready_deadline_precedence cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils response_egress_post_ready_deadline_precedence`; confirm the reviewed failures.
- [ ] Run `scripts/run_test_nonzero.sh raw_fetch_timer_arms_from_final_remaining_budget cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils raw_fetch_timer_arms_from_final_remaining_budget`, `scripts/run_test_nonzero.sh response_egress_preflight_precedes_writer_operation cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils response_egress_preflight_precedes_writer_operation`, and `scripts/run_test_nonzero.sh bodyless_response_checks_cancel_and_deadline_before_handoff cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils bodyless_response_checks_cancel_and_deadline_before_handoff`; confirm the reviewed failures.
- [x] Move outbound remaining-budget samples to the last pre-dispatch boundary, make Cloudflare operation construction lazy until after preflight, and apply post-ready response-egress arbitration consistently.
- [x] Run `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils`, `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils`, and `cargo check --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --all-targets`.
- [x] Run `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract` and `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh streamed_upload_eof_yields_before_final_precedence cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --lib`.

### Task 3: Owned ingress and error egress on every adapter

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: each adapter's `tests/contract.rs`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`

- [x] Add `normalized_ingress_error_returns_typed_response_without_application_observation` and `post_admission_failure_uses_owned_error_egress` to every applicable adapter contract suite.
- [ ] Run both named filters through `scripts/run_test_nonzero.sh` for each adapter: Axum with `--no-default-features --features axum,test-utils --test contract`, and Cloudflare, Fastly, and Spin with `--no-default-features --features test-utils --test contract`; confirm all eight named invocations fail for the reviewed boundary mismatch.
- [x] Keep provider parser failures before normalized ingress metadata outside response egress. Route normalized-head failures and admission refusals through a detached envelope carrying the app clock, bounded default policy, and no-op observer.
- [x] Treat successful `PreparedIngress` creation as the owned response-egress boundary. Add one core helper that constructs a post-admission error envelope with the app clock, request method/start, route metadata where available, application policy, and observer.
- [x] Route body-source construction, core-request conversion, and dispatch-rendering failures after that boundary through the owned helper; retain provider errors only when bounded response construction or delivery also fails.
- [x] Amend the ingress/egress specs to pin this exact boundary and its no-application-observation pre-admission behavior.
- [x] Run `cargo test --offline --locked -p edgezero-core`, `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`, `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`, `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`, and `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`.

### Task 4: Config deadline clock and precedence hard cut

**Files:**
- Modify: `crates/edgezero-core/src/config_store.rs`
- Modify: `crates/edgezero-core/src/secret_store.rs`
- Modify: `crates/edgezero-core/src/extractor.rs`
- Modify: all adapter `config_store.rs` and `secret_store.rs` implementations
- Modify: `docs/superpowers/specs/2026-06-16-blob-app-config.md`
- Modify: `docs/guide/capabilities.md`

- [x] Add core tests `bounded_read_uses_injected_clock` and `bounded_read_deadline_wins_ready_provider_error`; add `bounded_config_read_uses_injected_clock` and `bounded_secret_read_uses_injected_clock` to every adapter.
- [ ] Run `scripts/run_test_nonzero.sh bounded_read_uses_injected_clock cargo test --offline --locked -p edgezero-core bounded_read_uses_injected_clock` and `scripts/run_test_nonzero.sh bounded_read_deadline_wins_ready_provider_error cargo test --offline --locked -p edgezero-core bounded_read_deadline_wins_ready_provider_error`; confirm the reviewed failures.
- [ ] Run the two adapter filters through `scripts/run_test_nonzero.sh` for Axum with `--no-default-features --features axum,test-utils`, and for Cloudflare, Fastly, and Spin with `--no-default-features --features test-utils`; include provider-error-at-expiry coverage and confirm all eight named invocations fail.
- [x] Hard-cut bounded config/secret read signatures to receive the application `MonotonicClock`; remove process-global checks from extraction paths.
- [x] Perform post-ready deadline arbitration before propagating provider results.
- [x] Update every normative API/extractor sketch and capability explanation to require the same application clock.
- [x] Run `cargo test --offline --locked -p edgezero-core`, `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils`, `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils`, `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils`, and `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils`.

### Task 5: Spin host plumbing, response settlement, evidence, and documentation

**Files:**
- Modify: Spin adapter CLI validation and tests
- Modify: `crates/edgezero-cli` runtime/generator tests as needed
- Modify: generated and demo Spin manifests only if their canonical defaults change
- Modify: outbound implementation/specification documents and documentation checker
- Modify: Fastly and Spin outbound/contract/resource tests

- [x] Add failing selected-component host-drift tests with the shared prefix `spin_selected_component_` for absent/default, explicit hosts, wildcard expansion, normalization, malformed platform entries, ordering, and ambiguous components.
- [ ] Run `scripts/run_test_nonzero.sh spin_selected_component_ cargo test --offline --locked -p edgezero-adapter --features cli spin_selected_component_` and confirm failure.
- [x] Enforce canonical Spin `allowed_outbound_hosts` equality before build/serve/deploy without linking the Spin adapter into no-default-feature CLI builds.
- [x] Add `spin_host_drift_blocks_runtime_actions_before_dispatch` in `edgezero-cli` covering build, serve, deploy, staged deploy, shell and registered targets, and proving zero child/adapter side effects on drift.
- [ ] Run `scripts/run_test_nonzero.sh spin_host_drift_blocks_runtime_actions_before_dispatch cargo test --offline --locked -p edgezero-cli spin_host_drift_blocks_runtime_actions_before_dispatch` and confirm failure; also run `cargo check --offline --locked -p edgezero-cli --no-default-features --features cli` to prove the check does not require the Spin adapter.
- [x] Exercise production Fastly dispatch/harvest under Viceroy, plus Spin coordinator and target-SDK resource construction/drop behavior. Keep deployed provider-observation promotion evidence explicitly unavailable.
- [x] Align Spin framing-bodyless and `205 Reset Content` settlement with the shared contract: framing-bodyless responses release without polling, declared-body 205 responses release immediately, and undeclared-body 205 responses perform at most one deadline-bounded read before release.
- [x] Mark implemented phase status accurately and remove stale Spin SDK/Viceroy references; extend the docs checker.
- [x] Run `CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract`.
- [x] Run `CARGO_TARGET_WASM32_WASIP2_RUNNER='/tmp/wasmtime-v48.0.2-aarch64-macos/wasmtime run -W component-model-async=y -S p3=y -S http=y' scripts/run_test_nonzero.sh sdk_request_and_response_completion_resources_construct cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`.
- [x] Run `cargo test --offline --locked -p edgezero-adapter --features cli`, `cargo test --offline --locked -p edgezero-cli`, `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds`, `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`, and `node scripts/check_outbound_docs_contract.mjs`.

### Task 6: Full verification and delivery

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run `bash scripts/check_adapter_feature_matrix.sh axum native`, `bash scripts/check_adapter_feature_matrix.sh cloudflare native`, `bash scripts/check_adapter_feature_matrix.sh fastly native`, and `bash scripts/check_adapter_feature_matrix.sh spin native`.
- [x] Run `bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown`, `bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1`, and `bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2`.
- [x] Run `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract`, `CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract`, and `CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' scripts/run_test_nonzero.sh router_dispatches_get_and_returns_response cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract`.
- [x] Run `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh streamed_upload_eof_yields_before_final_precedence cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --lib`, `CARGO_TARGET_WASM32_WASIP1_RUNNER='viceroy run' scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --lib`, and `CARGO_TARGET_WASM32_WASIP2_RUNNER='wasmtime run -W component-model-async=y -S p3=y -S http=y' scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`.
- [x] Run `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds`.
- [x] From `examples/app-demo`, run `cargo test --locked --workspace --all-targets`, `cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare`, `cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly`, and `cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin`.
- [x] Run `bash scripts/check_outbound_legacy_api.sh`, `node scripts/check_outbound_docs_contract.mjs`, and from `docs`, `npm run lint`, `npm run format`, and `npm run build`.
- [x] Run `git diff --check`, review `git diff --stat` and `git diff`, commit the remediation, run `git diff --check origin/main...HEAD`, push PR 275, and report the exact SHA and remaining provider limitations.
