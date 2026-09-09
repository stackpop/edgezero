# Outbound HTTP Phase 7: Public Migration, Documentation, and Final Gates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate all remaining consumers, delete the legacy proxy API without aliases, publish capability/outbound documentation, and make every generated, example, adapter, WASM, host, and docs gate executable in CI.

**Architecture:** Migrate non-workspace consumers before deleting core's temporary legacy module. Then enforce the hard public cutoff with a scoped symbol scan. Deterministic merge gates stay separate from the required protected Fastly characterization and the optional Spin teardown characterization; Cloudflare host evidence is already mandatory in Phase 4.

**Tech Stack:** Rust workspace and generated workspaces, Handlebars templates, VitePress/npm, GitHub Actions, ripgrep structural gates.

---

## Preconditions

- [ ] Phases 1b-6 pass all their deterministic and required host gates.
- [ ] All four adapters already use `outbound` modules and `HttpClient`; only core scaffolding, generated/template consumers, app-demo, and prose may retain legacy names.
- [ ] Re-read spec §§5-7 and the final capability matrix. Do not promote BestEffort cells during documentation cleanup.

## Task Protocol

For each task: first add or run the named structural/build test and capture the expected stale-symbol, missing-capability, or compile failure; perform only that migration slice; rerun it to success plus `git diff --check`; then stage only listed files and make the stated commit. Do not combine the hard core deletion with unrelated prose cleanup.

**Required exact test/gate names:** `generated_workspace_compiles`, `generated_manifest_declares_outbound_http_optional`, `generated_spin_hosts_default_to_https_only`, `generated_outbound_http_smoke`, `generated_core_tests_execute`, `generated_core_test_gate_rejects_zero_tests`, `app_demo_outbound_client_preserves_method_and_uri`, `app_demo_manifest_declares_outbound_http_optional`, `typed_body_stream_boundaries_preserve_edge_error`, `capabilities_page_lists_exact_matrix`, `docs_sidebar_links_capabilities`, and `outbound_docs_contract`. The source-wide legacy-symbol check is owned by `scripts/check_outbound_legacy_api.sh`, not by a Rust test that would need to embed the forbidden strings.

**Expected red:** generated/template tests find missing capabilities or stale behavior; app-demo fails against the new API; the hard-cut scan reports exact remaining source/test/template paths; docs checks find missing matrix/sidebar content. Each red result must disappear in its owning task, not be added to an exclusion.

### Task 1: Migrate scaffolds and strengthen generated-project tests

**Files:**
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/README.md.hbs`
- Modify: `crates/edgezero-adapter-spin/src/templates/spin.toml.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `crates/edgezero-cli/tests/generated_project_builds.rs`

- [ ] Add failing structural tests for `optional = ["outbound-http"]`, outbound API names, Spin absent-host rendering as HTTPS-only, explicit wildcard HTTP+HTTPS expansion, and no implicit cleartext grant. The generated project is portable across all four adapters, so it cannot require a capability Fastly truthfully reports BestEffort. Generated README text states that `optional` explicitly accepts both documented behavioral deviations and runtime failure when an unverified deployment prerequisite is absent; an app whose behavior depends on outbound success must promote the declaration to `required` and select an adapter/deployment whose matrix result passes. Do not add a Rust assertion containing legacy API spellings; Task 3's source-wide negative gate owns their absence after all code/tests/templates migrate.
- [ ] First add only the harness regression
  `generated_core_test_gate_rejects_zero_tests`. Run
  `cargo test -p edgezero-cli --test generated_project_builds generated_core_test_gate_rejects_zero_tests`
  and require a nonzero assertion failure showing the current harness accepts an
  otherwise-successful zero-test fixture; dependency/build failure or a filtered zero-test
  command is not the expected red.
- [ ] Add a generated core test named `generated_outbound_http_smoke`. Extend the generated-project harness to run `cargo test -p scaffold-probe-core --lib -- --list`, parse listed test identifiers, and require at least one test plus exactly one identifier whose terminal component is `generated_outbound_http_smoke` (the current full name is `handlers::tests::generated_outbound_http_smoke`). Match an optional Rust module prefix ending in `::`, not arbitrary diagnostic text or an impossible bare output line. Then run `cargo test -p scaffold-probe-core --lib`. Rerun the exact `generated_core_test_gate_rejects_zero_tests` command and require a nonzero passing test, then run the `generated_workspace_compiles` sentinel gate below.
- [ ] Run `cargo test -p edgezero-cli generator`; expect failure, then migrate templates/renderers and rerun.
- [ ] Run `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test -p edgezero-cli --test generated_project_builds`; expect generated compilation and nonzero core tests.
- [ ] Commit: `feat(scaffold): generate outbound HTTP clients`.

### Task 2: Migrate app-demo and its independent lockfile

**Files:**
- Modify: `examples/app-demo/edgezero.toml`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`
- Modify: `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs`
- Modify: `examples/app-demo/Cargo.lock`

- [ ] Add/update tests for URI merge/query preservation, injected client success, no-client 501, original method/URI propagation, `OutboundResponse::into_response`, and shipped capability parsing.
- [ ] Before migration, run `(cd examples/app-demo && cargo test --offline --locked -p app-demo-core app_demo_outbound_client_preserves_method_and_uri)` and require a nonzero failure from missing/stale outbound behavior, not dependency resolution or a zero-test filter.
- [ ] Update the mock to implement both required trait methods and preserve partial batch results.
- [ ] Declare outbound HTTP optional so the multi-adapter demo remains deployable to Fastly
  with the standard BestEffort warning; make selected Spin hosts match the canonical expected
  set. The demo handler must render the typed outbound failure, including the exact
  dynamic-backend-disabled 502, rather than assuming optional means success. The test must
  still prove the capability is present and not silently omitted.
- [ ] Run these exact independent-workspace gates and require success: `(cd examples/app-demo && cargo fmt --all -- --check)`, `(cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)`, `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`, `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare)`, `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly)`, and `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin)`.
- [ ] Commit: `refactor(app-demo): migrate to outbound HTTP API`.

### Task 3: Perform the hard core cutoff

**Files:**
- Delete: `crates/edgezero-core/src/proxy.rs`
- Modify: `crates/edgezero-core/src/lib.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Create: `scripts/check_outbound_legacy_api.sh`
- Modify: each remaining runtime/template path printed by the first red run of that script; adding an exclusion instead is not permitted

- [ ] Create `scripts/check_outbound_legacy_api.sh` as the deterministic negative gate for code and generated templates in this task. It initially scans `crates` and `examples/app-demo` for only the legacy API symbols below. Task 4 expands its roots to active documentation after that task owns the prose migration. Implement exit handling explicitly: ripgrep status 0 prints matches and exits 1; status 1 exits 0; status greater than 1 propagates as a scan failure.

```sh
#!/usr/bin/env bash
set -uo pipefail

if matches="$(rg -n 'Proxy(Client|Handle|Request|Response|Service)|proxy_handle|edgezero_core::proxy|crate::proxy|pub mod proxy' \
  crates examples/app-demo --glob '*.rs' --glob '*.hbs')"; then
  printf '%s\n' "$matches"
  exit 1
else
  status=$?
  if [ "$status" -eq 1 ]; then exit 0; fi
  exit "$status"
fi
```

- [ ] Run `bash scripts/check_outbound_legacy_api.sh`; expect failure with every remaining source/test/template match. Migrate each match; do not preserve forbidden spellings in Rust string-literal assertions because this raw-source gate intentionally rejects them too.

```sh
bash scripts/check_outbound_legacy_api.sh
```

- [ ] Delete the legacy module and `proxy_handle`; export only outbound names. Add no deprecated aliases, forwarding traits, or feature flags.
- [ ] Preserve the wire/header constant `PROXY_HEADER`, `x-edgezero-proxy`, route names such as `/proxy/{*rest}`, and ordinary reverse-proxy terminology where semantically correct.
- [ ] Rerun `bash scripts/check_outbound_legacy_api.sh`; expect exit 0 and no output. Run workspace and app-demo tests.
- [ ] Separately inventory every typed-stream boundary; this audit is not satisfied by the legacy-symbol scan:

```sh
rg -n 'Body::from_stream|Body::from_external_stream|Body::Stream|Self::Stream|\.into_stream\(' \
  crates examples/app-demo --glob '*.rs' --glob '*.hbs'
```

- [ ] This reruns the Phase 2 audit rather than creating a new adjacent test for comments, definitions, or unchanged inbound introspection. Classify construction/consumption matches only: EdgeZero-owned outbound/decoder/deadline paths must carry `Result<Bytes, EdgeError>`; genuine platform/foreign inputs alone use `from_external_stream`. Existing Phase 2 and adapter error-identity tests must cover every error-producing owned boundary introduced by the migration.
- [ ] Rerun the inventory after migration and review every remaining construction/consumption line. Then run `cargo test --workspace --all-targets`, `cargo check --workspace --all-targets --features "fastly cloudflare spin"`, the Cloudflare WASM contract command, both Fastly WASM contract/library commands, and both Spin WASM contract/SDK-resource commands from Task 6. Compilation plus the named Phase 2/adapter error-identity tests is the `typed_body_stream_boundaries_preserve_edge_error` gate.
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
- Modify: `scripts/check_outbound_legacy_api.sh`
- Create: `scripts/check_outbound_docs_contract.mjs`
- Modify: `CLAUDE.md`, `.claude/agents/code-architect.md`, `Cargo.toml`, `TODO.md` where they describe the active API

- [ ] Document all eight outbound capability names, support ladder semantics, exact matrix, every BestEffort footnote including Fastly's dynamic-backend service prerequisite, host grammar/defaults, request/response limits, batch memory model, and no-client behavior. The exact rows are:

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `outbound-http` | Native | Native | BestEffort | Native |
| `outbound-complete-resource-accounting` | Unsupported | Unsupported | Unsupported | Unsupported |
| `outbound-header-fidelity` | Native | BestEffort | Native | Native |
| `outbound-deadlines` | Native | Native | BestEffort | BestEffort |
| `outbound-flexible-phase-budget` | Native | Native | BestEffort | BestEffort |
| `send-all-slot-isolation` | Native | Native | BestEffort | Native |
| `streamed-upload-deadlines` | Native | Native | BestEffort | BestEffort |
| `lazy-streamed-response-passthrough` | BestEffort | Native | BestEffort | BestEffort |

- [ ] Explain that Cloudflare manual upstream fetch encoding and downstream encoded passthrough are distinct controls.
- [ ] Explain 16 MiB downstream conversion fallback on Axum/Fastly/Spin and Native lazy passthrough only on Cloudflare.
- [ ] Explain that the body/header/window/decoder-state controls bound named guest-visible terms, `max_chunk_bytes` shapes emitted items without limiting source allocation, and complete pre-admission process/isolate accounting is Unsupported on every current adapter because provider parser, field-section, native chunk, allocator, and host-copy terms remain opaque.
- [ ] Before editing prose, expand `scripts/check_outbound_legacy_api.sh` to scan `docs/guide`, `README.md`, `CLAUDE.md`, `TODO.md`, root `Cargo.toml`, and `.claude/agents/code-architect.md` in addition to its existing code/template roots, adding `*.md` and `*.toml` globs. Using explicit active-source roots excludes internal history under `docs/superpowers/specs` and `docs/superpowers/plans`. Run it and require the expected stale-document failure.
- [ ] Add exactly one sidebar item whose link is `/guide/capabilities`; verify all internal links/anchors.
- [ ] Create `scripts/check_outbound_docs_contract.mjs`. It reads `docs/guide/capabilities.md` and `docs/.vitepress/config.mts`, locates exactly one Markdown table with the exact header `Capability | Axum | Cloudflare | Fastly | Spin`, and parses every contiguous data row in that table rather than filtering by a name prefix. It strips backticks around the first cell and only a trailing superscript footnote reference (`¹` through `⁹`) or Markdown footnote reference (`[^…]`) from support cells, then deep-compares the resulting ordered entries to the literal eight-row matrix above. It fails on a missing/duplicate matrix table or any duplicate, missing, extra, reordered, misspelled, or differently supported row, including the two valid names that do not begin `outbound-`. It also requires exactly one `/guide/capabilities` sidebar link. These assertions own `capabilities_page_lists_exact_matrix`, `docs_sidebar_links_capabilities`, and `outbound_docs_contract`.
- [ ] Run `node scripts/check_outbound_docs_contract.mjs`; expect failure before the page/sidebar are complete, then exit 0 after the documentation change.
- [ ] Run `npm --prefix docs run lint`, `npm --prefix docs run format`, and `npm --prefix docs run build`; expect success.
- [ ] Run `bash scripts/check_outbound_legacy_api.sh` over the now-updated active docs; expect no stale API references. Legitimate protocol terminology is outside the script's exact symbol pattern and needs no broad exclusion.
- [ ] Commit: `docs: publish outbound HTTP and capabilities guide`.

### Task 5: Make the completion matrix a CI contract

**Files:**
- Modify: `.github/workflows/test.yml`
- Modify: `.github/workflows/format.yml`
- Modify: `scripts/run_tests.sh`
- Modify: `scripts/run_test_nonzero.sh`

- [ ] Add/confirm native contract commands for all four adapters and `test-utils` in each supported WASM contract run. Route every native, browser-WASM, WASI SDK-resource, Fastly SDK, and CLI capability suite through `scripts/run_test_nonzero.sh [--ignored] <exact-sentinel> <command...>`. The helper first runs the command with harness `--list` (and `--ignored` when requested), requires the sentinel as the exact terminal test-name component with only an optional Rust module prefix, and requires at least one listed test, then runs the same selected suite; cfg/feature drift to zero tests fails CI. The generated-project integration target uses this helper, while its nested generated-core command keeps Task 1's equivalent in-harness list/sentinel/count assertion because it runs from a generated directory.
- [ ] Add generated-project core tests, app-demo fmt/clippy/workspace-test and three WASM checks, and PR-time docs lint/format/build. Retain the existing explicit `examples/app-demo` format and clippy steps in `.github/workflows/format.yml`; the root workspace excludes that directory, so root gates cannot replace them.
- [ ] Retain Cloudflare workerd/deployed host-observed cancellation and timing jobs from Phase 4.
- [ ] Keep pinned Spin teardown characterization clearly non-blocking for its existing BestEffort claims. Retain Phase 6's required Fastly enabled/disabled entitlement characterization as evidence that the adapter actually issues requests and maps disabled-service failure; that evidence does not promote the static `outbound-http` cell. Any future Native promotion requires a deployment-aware prerequisite, new evidence, and a spec change.
- [ ] Add `bash scripts/check_outbound_legacy_api.sh` and `node scripts/check_outbound_docs_contract.mjs` as deterministic nonzero-on-violation gates, with the legacy scan roots documented beside its workflow step.
- [ ] Add the four CLI capability suites as explicit CI commands so the declarations and action-specific gate paths cannot disappear behind broad workspace success:

```sh
scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec
scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec
scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec
scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec
```

- [ ] Make the generated-project job run `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test -p edgezero-cli --test generated_project_builds`. The integration harness then performs Task 1's `generated_outbound_http_smoke` list/count assertion inside the generated workspace before executing its core suite. A successful zero-test command at either level is a CI failure.
- [ ] Commit: `ci: enforce outbound HTTP completion matrix`.

### Task 6: Final full-system verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `scripts/run_test_nonzero.sh adapter_capability_matrix_matches_outbound_spec cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli --lib adapter_capability_matrix_matches_outbound_spec`
- [ ] `env CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner scripts/run_test_nonzero.sh cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features cloudflare,test-utils --target wasm32-unknown-unknown --test contract`
- [ ] `(cd crates/edgezero-adapter-cloudflare && ../../scripts/run_test_nonzero.sh cloudflare_fetch_options_are_raw_manual_abortable_and_no_redirect cargo test --offline --locked --no-default-features --features cloudflare,test-utils --test contract)`; this separate run has no target/runner override and proves the committed crate-local Cargo config selects `wasm32-unknown-unknown` plus `wasm-bindgen-test-runner`.
- [ ] `test "$(viceroy --version)" = "viceroy 0.17.0"`
- [ ] `(cd crates/edgezero-adapter-fastly && ../../scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked --no-default-features --features fastly,test-utils --test contract)`
- [ ] `(cd crates/edgezero-adapter-fastly && ../../scripts/run_test_nonzero.sh backend_creation_error_table_is_exhaustive cargo test --offline --locked --no-default-features --features fastly,test-utils --lib)`
- [ ] `cargo metadata --offline --locked --format-version 1 --manifest-path crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml`
- [ ] `cargo tree --offline --locked --manifest-path crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/Cargo.toml | rg 'fastly v0\.12\.1'`
- [ ] `fastly compute build --non-interactive --dir crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly`
- [ ] `test -f crates/edgezero-adapter-fastly/tests/fixtures/outbound-fastly/pkg/edgezero-outbound-probe.tar.gz`
- [ ] `(cd crates/edgezero-adapter-spin && ../../scripts/run_test_nonzero.sh response_caller_result_succeeds_only_after_native_eof cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test contract)`; the crate-local runner must contain the exact Phase 5 validated command.
- [ ] `(cd crates/edgezero-adapter-spin && ../../scripts/run_test_nonzero.sh spin_error_code_table_is_exhaustive cargo test --offline --locked --no-default-features --features spin,test-utils --target wasm32-wasip2 --test sdk_resources)`
- [ ] `cargo metadata --offline --locked --format-version 1 --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml`
- [ ] `cargo tree --offline --locked --manifest-path crates/edgezero-adapter-cloudflare/tests/fixtures/outbound-worker/Cargo.toml | rg 'worker v0\.8\.3'`
- [ ] `npm ci --prefix crates/edgezero-adapter-cloudflare`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run build:outbound-fixture`
- [ ] `npm --prefix crates/edgezero-adapter-cloudflare run test:workerd`; the host driver requires every exact probe ID and a positive count.
- [ ] Push the reviewed final HEAD to `refs/heads/outbound-probe-reviewed`, dispatch the
  default-branch `.github/workflows/outbound-cloudflare-deployed.yml` with
  `commit_sha=$(git rev-parse HEAD)`, and require a successful workflow URL whose recorded
  checkout SHA matches exactly and whose driver reports every exact probe ID plus a positive
  count. A direct credentialed npm invocation does not prove protected-environment or
  default-branch workflow behavior.
- [ ] Dispatch the default-branch
  `.github/workflows/outbound-fastly-characterization.yml` for the same exact reviewed SHA
  and require its successful workflow URL, exact checkout SHA, positive probe count, and
  cleanup result.
- [ ] `scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test -p edgezero-cli --test generated_project_builds`
- [ ] `(cd examples/app-demo && cargo fmt --all -- --check)`
- [ ] `(cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly)`
- [ ] `(cd examples/app-demo && cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin)`
- [ ] `bash scripts/check_outbound_legacy_api.sh`
- [ ] `node scripts/check_outbound_docs_contract.mjs`
- [ ] `npm --prefix docs ci`
- [ ] `npm --prefix docs run lint`
- [ ] `npm --prefix docs run format`
- [ ] `npm --prefix docs run build`
- [ ] Rerun the Task 3 typed-stream inventory. Every construction/consumption match must retain its reviewed owned/external classification; compilation plus the named Phase 2 and adapter tests must cover each error-producing owned boundary introduced by the migration. Then run `git diff --check`.
- [ ] Review `git status --short`; only intended implementation/docs/lock/CI changes may remain.

Expected result: the repository has one public outbound HTTP API, generated consumers and app-demo exercise it, documentation matches the exact capability contract, and CI covers every deterministic acceptance surface.
