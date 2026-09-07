# Outbound HTTP Phase 7: Public Migration, Documentation, and Final Gates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate all remaining consumers, delete the legacy proxy API without aliases, publish capability/outbound documentation, and make every generated, example, adapter, WASM, host, and docs gate executable in CI.

**Architecture:** Migrate non-workspace consumers before deleting core's temporary legacy module. Then enforce the hard public cutoff with a scoped symbol scan. Deterministic merge gates stay separate from optional Fastly/Spin live characterization; Cloudflare host evidence is already mandatory in Phase 4.

**Tech Stack:** Rust workspace and generated workspaces, Handlebars templates, VitePress/npm, GitHub Actions, ripgrep structural gates.

---

## Preconditions

- [ ] Phases 1b-6 pass all their deterministic and required host gates.
- [ ] All four adapters already use `outbound` modules and `HttpClient`; only core scaffolding, generated/template consumers, app-demo, and prose may retain legacy names.
- [ ] Re-read spec §§5-7 and the final capability matrix. Do not promote BestEffort cells during documentation cleanup.

## Task Protocol

For each task: first add or run the named structural/build test and capture the expected stale-symbol, missing-capability, or compile failure; perform only that migration slice; rerun it to success plus `git diff --check`; then stage only listed files and make the stated commit. Do not combine the hard core deletion with unrelated prose cleanup.

**Required exact test/gate names:** `generated_manifest_requires_outbound_http`, `generated_spin_hosts_default_to_https_only`, `generated_core_tests_execute`, `app_demo_outbound_client_preserves_method_and_uri`, `app_demo_manifest_declares_outbound_http`, `legacy_outbound_api_symbols_absent`, `typed_body_stream_boundaries_preserve_edge_error`, `capabilities_page_lists_exact_matrix`, and `docs_sidebar_links_capabilities`.

**Expected red:** generated/template tests find old symbols or missing capabilities; app-demo fails against the new API; the hard-cut scan reports exact remaining production paths; docs checks find missing matrix/sidebar content. Each red result must disappear in its owning task, not be added to an exclusion.

### Task 1: Migrate scaffolds and strengthen generated-project tests

**Files:**
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/README.md.hbs`
- Modify: `crates/edgezero-adapter-spin/src/templates/spin.toml.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `crates/edgezero-cli/tests/generated_project_builds.rs`

- [ ] Add failing structural tests for `required = ["outbound-http"]`, outbound API names, no legacy symbols, Spin absent-host rendering as HTTPS-only, explicit wildcard HTTP+HTTPS expansion, and no implicit cleartext grant.
- [ ] Extend the generated-project integration test to execute `cargo test -p scaffold-probe-core --lib`; a workspace check alone is insufficient.
- [ ] Run `cargo test -p edgezero-cli generator`; expect failure, then migrate templates/renderers and rerun.
- [ ] Run `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`; expect generated compilation and nonzero core tests.
- [ ] Commit: `feat(scaffold): generate outbound HTTP clients`.

### Task 2: Migrate app-demo and its independent lockfile

**Files:**
- Modify: `examples/app-demo/edgezero.toml`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`
- Modify: `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs`
- Modify: `examples/app-demo/Cargo.lock`

- [ ] Add/update tests for URI merge/query preservation, injected client success, no-client 501, original method/URI propagation, `OutboundResponse::into_response`, and shipped capability parsing.
- [ ] Update the mock to implement both required trait methods and preserve partial batch results.
- [ ] Declare required outbound HTTP and make selected Spin hosts match the canonical expected set.
- [ ] Run app-demo workspace tests plus Cloudflare, Fastly, and Spin WASM checks; expect success under `--locked`.
- [ ] Commit: `refactor(app-demo): migrate to outbound HTTP API`.

### Task 3: Perform the hard core cutoff

**Files:**
- Delete: `crates/edgezero-core/src/proxy.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify any remaining runtime Rust source revealed by the scoped scan
- Modify tests adjacent to every body-stream boundary revealed by the typed-stream inventory

- [ ] First run the scoped scan and save the expected remaining production callsites:

```sh
rg -n 'Proxy(Client|Handle|Request|Response|Service)|proxy_handle|edgezero_core::proxy|crate::proxy|pub mod proxy' \
  crates examples/app-demo --glob '*.rs' --glob '*.hbs'
```

- [ ] Delete the legacy module and `proxy_handle`; export only outbound names. Add no deprecated aliases, forwarding traits, or feature flags.
- [ ] Preserve the wire/header constant `PROXY_HEADER`, `x-edgezero-proxy`, route names such as `/proxy/{*rest}`, and ordinary reverse-proxy terminology where semantically correct.
- [ ] Rerun the scan; expect no legacy API matches in production/generated Rust. Run workspace and app-demo tests.
- [ ] Separately inventory every typed-stream boundary; this audit is not satisfied by the legacy-symbol scan:

```sh
rg -n 'Body::(from_stream|from_external_stream|into_stream|Stream)|\.into_stream\(' \
  crates examples/app-demo --glob '*.rs' --glob '*.hbs'
```

- [ ] Classify every match before editing: EdgeZero-owned outbound/decoder/deadline producers and consumers must carry `Result<Bytes, EdgeError>` through `from_stream`, `into_stream`, or direct `Body::Stream`; only genuine platform/foreign-error inputs may use `from_external_stream`. Add an adjacent regression that injects a distinctive non-500 `EdgeError` at each owned boundary and asserts its status/kind/reason survives; external-boundary regressions assert deliberate `Internal` mapping. No match may be accepted without one of those two classifications.
- [ ] Rerun the inventory after migration, review every remaining line, then run `cargo test --workspace --all-targets` and `cargo check --workspace --all-targets --features "fastly cloudflare spin"`. Run the Cloudflare, Fastly, and Spin WASM contract commands from Task 6 so target-only constructors/consumers compile and execute. This compile-driven audit is the `typed_body_stream_boundaries_preserve_edge_error` gate.
- [ ] Commit: `refactor(core): remove legacy proxy API`.

### Task 4: Publish outbound and capability documentation

**Files:**
- Modify: `docs/guide/proxying.md`
- Modify: `docs/guide/handlers.md`
- Modify: `docs/guide/architecture.md`
- Modify: `docs/guide/streaming.md`
- Modify: `docs/guide/what-is-edgezero.md`
- Modify: `docs/guide/configuration.md`
- Modify: `docs/guide/adapters/{overview,axum,cloudflare,fastly,spin}.md`
- Create: `docs/guide/capabilities.md`
- Modify: `docs/.vitepress/config.mts`
- Modify: `CLAUDE.md`, `.claude/agents/code-architect.md`, `Cargo.toml`, `TODO.md` where they describe the active API

- [ ] Document all seven capability names, support ladder semantics, exact matrix, every BestEffort footnote, host grammar/defaults, request/response limits, batch memory model, and no-client behavior.
- [ ] Explain that Cloudflare manual upstream fetch encoding and downstream encoded passthrough are distinct controls.
- [ ] Explain 16 MiB downstream conversion fallback on Axum/Fastly/Spin and Native lazy passthrough only on Cloudflare.
- [ ] Add sidebar navigation and verify all internal links/anchors.
- [ ] Run docs lint/format/build; expect success.
- [ ] Run a docs/active-source legacy API scan excluding `docs/superpowers/specs`, `docs/superpowers/plans`, and legitimate protocol terminology; expect no stale API references.
- [ ] Commit: `docs: publish outbound HTTP and capabilities guide`.

### Task 5: Make the completion matrix a CI contract

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `.github/workflows/format.yml`
- Modify: `scripts/run_tests.sh`

- [ ] Add/confirm native contract commands for all four adapters and `test-utils` in each supported WASM contract run. Assert nonzero tests.
- [ ] Add generated-project core tests, app-demo workspace and three WASM checks, and PR-time docs lint/format/build.
- [ ] Retain Cloudflare workerd/deployed host-observed cancellation and timing jobs from Phase 4.
- [ ] Keep optional pinned Spin/Fastly live characterization clearly non-blocking for existing BestEffort claims. A future Native promotion requires new finite host evidence and a spec change.
- [ ] Add the scoped legacy-symbol command as a deterministic gate with exclusions documented beside it.
- [ ] Commit: `ci: enforce outbound HTTP completion matrix`.

### Task 6: Final full-system verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
- [ ] `cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] `cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`
- [ ] `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`
- [ ] `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --test contract`
- [ ] `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features fastly,test-utils --target wasm32-wasip1 --lib`
- [ ] `(cd crates/edgezero-adapter-spin && cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract)`; the crate-local runner must contain the exact Phase 5 validated command.
- [ ] `(cd crates/edgezero-adapter-spin && cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources)`
- [ ] `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin)`
- [ ] `(cd docs && npm ci && npm run lint && npm run format && npm run build)`
- [ ] Rerun both Task 3 scans: the legacy API scan must have no forbidden matches, and every typed-stream inventory match must still have its reviewed owned/external classification and adjacent error-identity regression. Then run `git diff --check`.
- [ ] Review `git status --short`; only intended implementation/docs/lock/CI changes may remain.

Expected result: the repository has one public outbound HTTP API, generated consumers and app-demo exercise it, documentation matches the exact capability contract, and CI covers every deterministic acceptance surface.
