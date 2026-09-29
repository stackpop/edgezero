# Platform Resource Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish typed platform memory facts and provide fail-closed, scope-aware memory-envelope validation while clarifying outbound batch observation cutoffs.

**Architecture:** `edgezero-core` owns typed known/unknown resource facts, canonical memory-envelope arithmetic, and validation outcomes. Runtime and CLI adapters delegate to one target constant; application admission remains application-owned. Documentation and executable drift tests keep provider claims, demo code, and generated templates aligned.

**Tech Stack:** Rust 1.95, Cargo workspace tests, WASM adapter builds, Markdown/VitePress, Node.js contract scripts.

---

### Task 1: Core resource facts

**Files:**
- Modify: `crates/edgezero-core/src/platform.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-macros/tests/app_macro.rs`

- [x] Add `platform_fact_distinguishes_known_and_unknown_metadata` and `memory_envelope_accessors_preserve_declared_shape` in `platform.rs`. Use the desired public constructors and accessors so the tests fail to compile while the API is absent.
- [x] Run `cargo test --offline --locked -p edgezero-core platform_fact_distinguishes_known_and_unknown_metadata` and confirm the missing-type failure.
- [x] Add `PlatformFact<T>`, `PlatformResourceSource`, `PlatformUnknownReason`, `InboundRequestPopulationBound`, `HostIngressMemoryAccounting`, and scope-tagged `MemoryEnvelope` with const constructors/accessors. Hard-cut `MemoryCeiling::new` to remove provenance and move it into `PlatformFact::Known`.
- [x] Run the two focused tests and `cargo test --offline --locked -p edgezero-core platform` until green.
- [x] Update app and macro tests to construct three explicit platform facts; run `cargo test --offline --locked -p edgezero-core app::tests` and `cargo test --offline --locked -p edgezero-macros --test app_macro` until green.

### Task 2: Fail-closed memory validation

**Files:**
- Modify: `crates/edgezero-core/src/platform.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [x] Add failing tests named `validation_fits_per_execution_with_counted_host_memory`, `validation_fits_per_instance_with_host_memory_outside_ceiling`, `validation_reports_primary_or_stack_excess`, `validation_unknown_ceiling_is_indeterminate`, `validation_unknown_population_is_indeterminate`, `validation_unknown_host_accounting_is_indeterminate`, `validation_missing_published_stack_requirement_is_indeterminate`, `validation_known_minimum_excess_precedes_unknown_facts`, `validation_rejects_scope_mismatch`, `validation_rejects_primary_arithmetic_overflow`, `validation_rejects_unknown_population_lower_bound_overflow`, `validation_rejects_stack_shape_mismatch`, and `validation_rejects_non_unit_per_execution_population`.
- [x] Make zero population unrepresentable by requiring `NonZeroU32` in `InboundRequestPopulationBound::new`; test the constructor signature and round-trip a value of one rather than adding a runtime zero branch.
- [x] Run `cargo test --offline --locked -p edgezero-core platform::tests::validation_` and confirm failure because validation types and methods are absent.
- [x] Implement `MemoryEnvelopeValidation::{Fits, Exceeds, Indeterminate}`, typed indeterminate reasons, `MemoryEnvelopeValidationError`, and `PlatformMetadata::validate_memory_envelope`.
- [x] For unknown per-instance population, compute `fixed + per_request * 1` with checked arithmetic only as a lower-bound overflow proof; otherwise return `Indeterminate`. Check a separate stack allowance independently and require an explicit stack requirement before returning `Fits`.
- [x] Run `cargo test --offline --locked -p edgezero-core platform::tests::validation_` and `cargo test --offline --locked -p edgezero-core` until green.

### Task 3: Canonical adapter metadata hard cut

**Files:**
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`

- [x] Add `adapter_platform_metadata_default_is_unknown` in the registry; add `adapter_platform_metadata_matches_runtime_metadata` in Axum, Cloudflare, and Fastly; add `adapter_platform_metadata_distinguishes_generic_and_hosted_profiles` in Spin. Assert every fact and reason, including one request per execution for Fastly and the hosted Spin profile, and provider-unpublished Cloudflare population/framing facts.
- [x] Run `cargo test --offline --locked -p edgezero-adapter adapter_platform_metadata_default_is_unknown` and confirm it fails against the old trait. Run each adapter test with `scripts/run_test_nonzero.sh <test-name> cargo test --offline --locked -p edgezero-adapter-<adapter> --no-default-features --features cli --lib <test-name>` and confirm it fails against `Adapter::memory_ceiling()` or the old constants.
- [x] Replace `Adapter::memory_ceiling()` with `platform_metadata()` and no compatibility alias. Update each in-tree CLI adapter to return its runtime constant.
- [x] Hard-cut every adapter `PlatformMetadata::new` and `MemoryCeiling::new` caller to the three-fact model.
- [x] Run `cargo test --offline --locked -p edgezero-adapter`, then each adapter's platform metadata tests, until green.

### Task 4: Batch observation cutoff contract

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/time.rs`
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/tests/contract.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs`
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/tests/contract.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `docs/guide/proxying.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] Run the existing early-observation-cutoff adapter contract as a passing characterization test; do not claim it is a TDD red test.
- [x] Add a failing docs/source contract named `observationCutoffContract` that requires the public Rustdoc warning and rejects stale public/example parameter names that use bare `cutoff`.
- [x] Run `node scripts/check_outbound_docs_contract.mjs` and confirm the new contract fails for missing wording.
- [x] Rename parameters and example variables to `observation_cutoff` across the exact files above. Keep `start_batch_until`, `send_all_until`, and `BudgetSource::BatchCutoff`; add no debug assertion.
- [x] Run the docs contract, core outbound tests, and all four focused adapter contract tests until green.

### Task 5: Documentation, migration, demo, and generated-project drift

**Files:**
- Create: `CHANGELOG.md`
- Modify: `docs/guide/adapters/overview.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/superpowers/plans/2026-09-26-platform-memory-ceiling.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: `scripts/check_outbound_legacy_api.sh`
- Modify: `crates/edgezero-cli/tests/platform_memory_docs.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`

- [ ] Rewrite `platform_memory_docs.rs` first to derive every documented memory, population, accounting, scope, and provenance cell from adapter constants. Run `cargo test --offline --locked -p edgezero-cli --test platform_memory_docs` and confirm it fails against the old table.
- [ ] Extend the legacy checker to reject `MemoryCeilingSource`, `Adapter::memory_ceiling`, old one-argument `PlatformMetadata::new`, old four-argument `MemoryCeiling::new`, compatibility aliases, and stale batch parameter/example wording. Explicitly allowlist only `CHANGELOG.md` and `docs/superpowers/specs/2026-09-28-platform-resource-metadata-design.md`, and add checker self-tests proving the same spellings still fail in every other active surface. Run it and confirm failure before migration.
- [ ] Update the capability guide, normative outbound spec, completed platform-memory plan, demo, and generator template. State that unknown provider facts prevent complete validation and that the outbound six-connection limit is not inbound population evidence.
- [ ] Add the exact old-to-new migration table from the focused design to `CHANGELOG.md`; make the adapter overview link to that table without repeating legacy signatures.
- [ ] Run `cargo test --offline --locked -p edgezero-cli --test platform_memory_docs`, generator unit tests, `bash scripts/check_outbound_legacy_api.sh`, and `node scripts/check_outbound_docs_contract.mjs` until green.
- [ ] Run `rg -n "MemoryCeilingSource|fn memory_ceiling\\(&self\\)|\\.memory_ceiling\\(\\).*Option|new_with_monotonic_clock" crates examples docs scripts` and confirm no live stale API remains outside historical text intentionally checked by the migration table.

### Task 6: Review, verification, and delivery

**Files:**
- Review: all modified files

- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo test --offline --locked --workspace --all-targets`.
- [ ] Run `cargo check --offline --locked --workspace --all-targets --features "fastly cloudflare spin"`.
- [ ] Run `cargo check --offline --locked -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter-fastly --all-targets --features cli`, `cargo clippy --offline --locked -p edgezero-adapter-fastly --features cli --all-targets -- -D warnings`, and `cargo clippy --offline --locked -p edgezero-adapter-fastly --no-default-features --lib -- -D warnings`.
- [ ] Run `cargo run --offline --locked -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates` and `cargo test --offline --locked -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config`.
- [ ] Run `bash scripts/check_adapter_feature_matrix.sh axum native`, then the same command for `cloudflare`, `fastly`, and `spin` with `native`.
- [ ] Run `bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown`, `bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1`, and `bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2`.
- [ ] Run `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_contracts cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features cli --lib adapter_capability_matrix_matches_contracts`, then the same command for `cloudflare`, `fastly`, and `spin`.
- [ ] Run `scripts/run_test_nonzero.sh batch_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`, the corresponding Cloudflare command with `test-utils`, the Fastly `batch_preflight_rejects_streamed_slots_without_poisoning_siblings` command with `test-utils`, and the Spin `batch_preflight_precedence_and_indices` command with `test-utils`.
- [ ] Run `cargo clippy --offline --locked -p edgezero-adapter-axum --no-default-features --features axum --all-targets -- -D warnings` and the same command with `--features axum,test-utils`.
- [ ] Run `cargo clippy --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare --target wasm32-unknown-unknown --all-targets -- -D warnings` and the same command with `--features cloudflare,test-utils`.
- [ ] Run `cargo clippy --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly --target wasm32-wasip1 --all-targets -- -D warnings`, then repeat with `fastly,cli`, `fastly,test-utils`, and `fastly,cli,test-utils`.
- [ ] Run `cargo clippy --offline --locked -p edgezero-adapter-spin --no-default-features --features spin --target wasm32-wasip2 --all-targets -- -D warnings` and the same command with `--features spin,test-utils`.
- [ ] Run `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract` and `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh streamed_upload_eof_yields_before_final_precedence cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --lib`.
- [ ] Run `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" scripts/run_test_nonzero.sh dispatch_runs_router_and_returns_response cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract` and the same command with sentinel `closed_lifecycle_orders_hooks_and_returns_state_after_delivery`.
- [ ] From `crates/edgezero-adapter-fastly`, run `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" ../../scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked --no-default-features --features fastly,test-utils --lib`.
- [ ] Run `CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y -S http=y" scripts/run_test_nonzero.sh router_dispatches_get_and_returns_response cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract` and `CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y -S http=y" scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources`.
- [ ] Run `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds`.
- [ ] From `examples/app-demo`, run `cargo fmt --all -- --check`, `cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings`, and `cargo test --offline --locked --workspace --all-targets`.
- [ ] From `examples/app-demo`, run `cargo check --offline --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare`, `cargo check --offline --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly`, and `cargo check --offline --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin`.
- [ ] Run `RUSTDOCFLAGS="-D warnings" cargo doc --offline --locked --workspace --all-features --no-deps`.
- [ ] From `docs`, run `npm ci`, `npm run lint`, `npm run format`, and `npm run build`.
- [ ] Run `bash scripts/check_no_placeholder_pins.sh`, `bash scripts/check_no_legacy_typed_reads.sh`, `bash scripts/check_outbound_legacy_api.sh`, `node scripts/check_outbound_docs_contract.mjs`, `node scripts/check_brotli_dependency_contract.mjs`, and `node scripts/check_serde_json_map_contract.mjs`.
- [ ] Review `git diff --check`, the full diff, and `rg -n "midbid|Midbid|MIDBID" CHANGELOG.md crates examples docs scripts` for unrelated changes, stale APIs, and prohibited consumer-specific wording.
- [ ] Commit with a consumer-neutral message, push PR 275, and wait for all hosted checks.
