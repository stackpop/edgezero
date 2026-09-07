# Outbound HTTP Phase 3: Capabilities and CLI Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make outbound requirements declarative and enforce them against one pinned app/runtime pair before build, serve, deploy, staged deploy, or demo execution.

**Architecture:** Core owns the manifest schema and host grammar; macros bake the validated contract; the adapter registry reports support; the CLI resolves one target and its associated contract once, then gates before shell or registry dispatch. Spin platform-host validation compares canonical atomic host sets for only the selected component.

**Tech Stack:** Rust 1.95, `serde`, `toml`, `validator`, proc macros/`trybuild`, adapter registry, CLI integration tests.

---

## Preconditions

- [ ] Phase 2 passes all repository gates.
- [ ] Read spec §3.5 in full, CLI portions of §§5-7, and current adapter discovery paths.
- [ ] Do not edit generated templates, app-demo, or public guide content in this phase; Phase 7 owns consumer declarations and docs.

## Task Protocol

For each task: write only the named unit/integration/trybuild cases; run the exact package/filter command and require a nonzero compile/assertion failure; implement the listed surface; rerun focused and package tests; run `git diff --check`; then stage only the task's files and make the stated commit. Resolver tests receive explicit directories/environment maps and never mutate process cwd/env concurrently.

**Required exact test names:** `capability_manifest_rejects_unknown_duplicate_and_overlap`, `outbound_host_grammar_table`, `outbound_host_default_is_https_only`, `baked_manifest_states_are_distinct`, `app_macro_manifest_caches_are_per_implementation`, `adapter_capability_default_is_unsupported`, `explicit_manifest_is_authoritative`, `runtime_resolver_rejects_cross_app_pair`, `runtime_resolver_rejects_equal_distance_ambiguity`, `spin_selected_component_hosts_match_as_atomic_sets`, `execute_gates_each_runtime_action_once`, and `execute_bypasses_operational_actions`.

**Expected red:** schema types are initially unresolved; macro fixtures initially compile or share state when they must not; the registry lacks `capability`; resolver tests select/fall back instead of rejecting; Spin drift passes incorrectly; dispatch counters show zero or duplicate gate calls.

### Task 1: Add strict capability and host schema

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

**Public surface:** `Capability`, `CapabilitySupport`, `ManifestCapabilities`, `ManifestOutboundCapability`, `AtomicHost`, `HostParseError`, `HostPat`, `Port`, `Scheme`, and `canonicalize_outbound_host` exactly as specified in §3.5.1.

- [ ] Add failing manifest tests for all seven kebab-case capabilities, unknown keys, duplicate required/optional entries, required/optional overlap, and serialization round trips.
- [ ] Add table tests for every accepted/rejected host grammar row from §3.5.1, including IPv4/IPv6, wildcard expansion, ports, LDH limits, punycode, raw Unicode, userinfo, paths, queries, fragments, whitespace, and malformed brackets.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib manifest::tests::`; expect failure.
- [ ] Implement strict `deny_unknown_fields` schema. Keep host parsing inside `manifest.rs`, because that file is textually included by the macro crate.
- [ ] Make absent hosts canonicalize to `https://*:*`; explicit `"*"` expands to both HTTP and HTTPS atomic entries. Validation, rendering, and drift checks must call the same parser.
- [ ] Rerun focused/core tests; expect success.
- [ ] Commit: `feat(core): add outbound capability manifest schema`.

### Task 2: Add baked-manifest state and macro isolation

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-macros/src/app.rs`
- Modify: `crates/edgezero-macros/tests/app_macro.rs`
- Create: `crates/edgezero-macros/tests/ui/app_capabilities_nested.rs` and `.stderr`
- Create: `crates/edgezero-macros/tests/ui/trigger_capabilities_nested.rs` and `.stderr`
- Create: `crates/edgezero-macros/tests/ui/environment_capabilities_nested.rs` and `.stderr`
- Create: `crates/edgezero-macros/tests/ui/adapter_build_capabilities_nested.rs` and `.stderr`
- Create: `crates/edgezero-macros/tests/ui/array_capabilities_nested.rs` and `.stderr`
- Create matching TOML inputs under `crates/edgezero-macros/tests/fixtures/` with the same stems

**Contract:** `BakedManifest::{Absent, Malformed, Present}`, `ManifestContract::{Malformed, None, Present}`, and `Hooks::{manifest_json, manifest}`.

- [ ] Add failing tests for absent, malformed, and present baked manifests; runtime/baked validation parity; and two app implementations whose caches cannot leak into each other.
- [ ] Add trybuild failures for `capabilities` misplaced under nested tables and arrays at every depth listed by §3.5.3.
- [ ] Run `cargo test --offline --locked -p edgezero-macros`; expect failure.
- [ ] Generate one `OnceLock` per app implementation, never in a shared default trait method. Recursively reject misplaced capability tables before typed deserialization.
- [ ] Rerun macro and core tests; expect success.
- [ ] Commit: `feat(macros): bake outbound capability contracts`.

### Task 3: Add fail-closed adapter capability metadata

**Files:**
- Modify: `crates/edgezero-adapter/Cargo.toml`
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `Cargo.lock`, `examples/app-demo/Cargo.lock`

- [ ] Add failing registry tests for the default `Unsupported` result and a fixture adapter that returns each support level. Do not publish in-tree matrix cells before the owning adapter behavior and evidence land.
- [ ] Run adapter/registry tests; expect failure.
- [ ] Add the direct `edgezero-core` dependency and defaulted `Adapter::capability`. Until Phases 4-6, in-tree adapters inherit `Unsupported`; each owning phase adds its exhaustive override atomically with passing contracts and any required host proof.
- [ ] Run registry tests; expect success.
- [ ] Commit: `feat(adapter): add fail-closed capability metadata`.

### Task 4: Build the paired runtime resolver

**Files:**
- Create: `crates/edgezero-cli/src/manifest_source.rs`
- Modify: `crates/edgezero-cli/src/lib.rs`
- Modify: `crates/edgezero-cli/src/adapter.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`

**Contract:** `ManifestSource`, `ResolvedManifest`, and owned `ResolvedRuntime { contract, target }`.

- [ ] Add pure failing tests for explicit `EDGEZERO_MANIFEST`, captured invocation-directory resolution, ancestor precedence, bounded workspace descendants, equal-distance ambiguity, target/contract mismatch, symlink containment, non-files, malformed contracts, and the narrow valid `contract: None` case.
- [ ] Add tests proving shell-command and registry-backed adapters receive the same pinned target and cannot rediscover a different manifest.
- [ ] Run `cargo test --offline --locked -p edgezero-cli manifest_source`; expect failure.
- [ ] Extract target-argument parsing and discovery into pure helpers. Explicit env selection is authoritative and never falls back. Canonical platform files must remain under the canonical app root.
- [ ] Preserve direct adapter API discovery for callers outside the CLI, but pass a pinned internal target from the CLI path.
- [ ] Rerun focused CLI tests; expect success.
- [ ] Commit: `refactor(cli): resolve paired runtime contracts`.

### Task 5: Validate selected Spin component host drift

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `crates/edgezero-cli/src/manifest_source.rs`

- [ ] Add failing tests for explicit component, single inferred component, missing/ambiguous components, absent hosts, wildcard expansion, scheme case normalization, malformed platform entries, set-order independence, and drift diagnostics containing manifest path/component/expected list.
- [ ] Run focused Spin CLI tests; expect failure.
- [ ] Implement one pure component selector shared by adapter validation and CLI enforcement. Compare only selected-component canonical atomic sets.
- [ ] Run focused tests with and without the Spin registry adapter linked; enforcement must work in both configurations.
- [ ] Commit: `feat(cli): reject Spin outbound host drift`.

### Task 6: Gate actions exactly once

**Files:**
- Modify: `crates/edgezero-cli/src/adapter.rs`
- Modify: `crates/edgezero-cli/src/lib.rs`
- Modify: `crates/edgezero-cli/src/auth.rs`
- Modify: `crates/edgezero-cli/src/demo_server.rs`
- Modify: `crates/edgezero-cli/Cargo.toml`

**Dispatch signatures:** both public dispatch functions accept the owned `ResolvedRuntime`; `ensure_capabilities(adapter_name, ManifestContract<'_>)` remains a crate-private gate called by one private post-gate dispatcher.

```rust
pub fn execute(
    adapter_name: &str,
    action: Action,
    runtime: ResolvedRuntime,
    adapter_args: &[String],
) -> Result<(), String>;

pub fn execute_capture(
    adapter_name: &str,
    action: Action,
    runtime: ResolvedRuntime,
    adapter_args: &[String],
) -> Result<Option<String>, String>;
```

- [ ] Add failing tests with fixture adapters for required versus optional capabilities at all four support levels, missing registry behavior, Fastly's dynamic-backend notice, and malformed/absent contracts.
- [ ] Add a counter seam proving `Build`, `Serve`, `Deploy`, and `DeployStaged` gate once in both `execute` and `execute_capture` before any shell/registry side effect.
- [ ] Add bypass tests for auth, emit-version, healthcheck, rollback, provision, and config. Add a demo test proving Axum checks the baked manifest before startup.
- [ ] Run `cargo test --offline --locked -p edgezero-cli`; expect failure.
- [ ] Implement one private post-gate dispatcher so `execute_capture` cannot re-enter public `execute`. Required accepts only Native/BoundedCooperative; optional warns for BestEffort/Unsupported.
- [ ] Rerun CLI tests; expect success. Adapter-specific capability values remain Unsupported until their implementation phase.
- [ ] Commit: `feat(cli): enforce outbound capabilities`.

## Phase Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `cargo test -p edgezero-adapter --all-targets`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: every execution path evaluates one manifest/target pair once, before side effects, while platform support claims remain explicit and conservative.
