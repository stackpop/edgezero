# Platform Memory Ceiling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Historical status:** The original memory-ceiling work was completed on 2026-09-26.
>
> **Retrospective revision (2026-09-29):** The plan was superseded by the
> [platform resource metadata design](../specs/2026-09-28-platform-resource-metadata-design.md),
> and its API references were updated to the current `PlatformFact` and `PlatformMetadata`
> contract so they cannot be mistaken for migration guidance. The checked items below are a
> retrospective final-state map of the original task areas, not an execution log and not a claim
> that those exact types or tests existed on 2026-09-26. Repository history remains the record of
> the original implementation. See the
> [Unreleased changelog](../../../CHANGELOG.md#platform-resource-metadata) for the hard-cut
> migration table.

**Retrospective final-state goal:** Expose target memory ceilings, accounting scope, and provenance through both CLI adapter metadata and runtime application configuration.

**Retrospective final-state architecture:** `edgezero-core` owns an extensible platform descriptor containing explicit known
or unknown memory, inbound population, and host-ingress accounting facts. Runtime adapters pass
that descriptor into application construction before the configuration callback runs, while CLI
adapter implementations delegate to the same target constants. Generic Spin remains unknown unless
an entrypoint explicitly selects a hosted or runtime-configured descriptor.

**Tech Stack:** Rust, EdgeZero adapter registry, proc macros, Cargo tests, Markdown contract checks.

---

### Task 1: Core platform metadata

**Files:**
- Create: `crates/edgezero-core/src/platform.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-macros/src/app.rs`

- [x] Add failing core tests for `MemoryCeiling`, fact-level provenance, `PlatformMetadata`, and visibility inside `Hooks::configure`.
- [x] Run the focused core tests and confirm the missing API failures.
- [x] Add non-exhaustive platform metadata types with const constructors and accessors; provenance is carried by `PlatformFact::Known`; immediately run the focused core tests until green.
- [x] Add `App::platform()` and hard-cut core-owned `App::build::<A>(platform)` as the single construction path; immediately run the focused app tests until green.
- [x] Update macro-generated `Hooks` implementations so platform metadata is installed before `configure`; immediately run macro and generated-code tests until green.

### Task 2: Canonical adapter metadata

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

- [x] Add a failing registry test proving the inherited CLI default contains three explicit unknown facts; run it and confirm the missing API failure.
- [x] Add failing Axum tests proving CLI and runtime memory, population, and host-accounting facts are unknown with the operator-configured reason; run them and confirm failure before implementation.
- [x] Add failing Cloudflare tests proving `128_000_000` primary bytes, no separate stack, per-instance scope, platform-limit provenance, and provider-unpublished population/accounting facts; run them and confirm failure before implementation.
- [x] Add failing Fastly tests proving `128_000_000` primary bytes, a separate `1_000_000`-byte stack, per-execution scope, a population of one with platform-limit provenance, and provider-unpublished host accounting; run them and confirm failure before implementation.
- [x] Add failing Spin tests proving generic CLI/runtime facts are runtime-configured unknowns and the explicit Akamai profile is `128 * 1024 * 1024` bytes per execution with a population of one and hosted-default provenance; prove the explicit descriptor is visible inside `configure`.
- [x] Add default `Adapter::platform_metadata()` returning fully unknown metadata; run the registry test until green.
- [x] Add Axum's canonical unknown metadata and CLI override; run Axum tests until green.
- [x] Add Cloudflare's canonical platform constant and CLI override; run Cloudflare tests until green.
- [x] Add Fastly's canonical platform constant and CLI override; run Fastly tests until green.
- [x] Add Spin's canonical unknown metadata, CLI override, explicit runtime entrypoint, and Akamai-hosted descriptor; run Spin tests until green.

### Task 3: Runtime propagation

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/dev_server.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-spin/src/lib.rs`
- Test: adapter contract/unit test modules adjacent to these entrypoints

- [x] Add a failing Axum runtime test, require explicit platform metadata during construction, and run Axum tests until green.
- [x] Add a failing Cloudflare runtime test, require explicit platform metadata during construction, and run Cloudflare tests until green.
- [x] Add a failing Fastly runtime test, require explicit platform metadata during construction, and run Fastly tests until green.
- [x] Add failing generic and explicit-profile Spin runtime tests, require explicit platform metadata during construction, and run Spin tests until green.

### Task 4: Documentation and drift checks

**Files:**
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Modify: adapter/CLI integration tests as needed

- [x] Add a platform-resource table that distinguishes exact bytes/units, separate stack, scope, population, host-ingress accounting, provenance, and unknown reasons.
- [x] Document that generic Spin is runtime-configured and that the Akamai hosted profile is explicit.
- [x] Add checker assertions for the table and a Rust integration test that derives every documented resource cell from the canonical constants.
- [x] Run the docs contract and focused registry tests.

### Task 5: Full verification and delivery

**Files:**
- Verify: workspace and example workspaces

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- [x] Run `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds`.
- [x] Run `cargo test --locked --workspace --all-targets` from `examples/app-demo`.
- [x] From `examples/app-demo`, run `cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare`.
- [x] From `examples/app-demo`, run `cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly`.
- [x] From `examples/app-demo`, run `cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin`.
- [x] Run `bash scripts/check_adapter_feature_matrix.sh <adapter> native` for Axum, Cloudflare, Fastly, and Spin.
- [x] Run `bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown`, `bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1`, and `bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2`.
- [x] From `docs`, run `npm run lint`, `npm run format`, and `npm run build`.
- [x] Review the diff for unrelated changes and forbidden downstream-specific wording.
- [x] Commit and push PR 275.
