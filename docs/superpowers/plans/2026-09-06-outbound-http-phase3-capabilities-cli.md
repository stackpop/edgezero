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

**Required exact test names:** `capability_manifest_rejects_unknown_duplicate_and_overlap`, `capability_manifest_rejects_reserved_key_at_every_depth_and_case`, `outbound_host_grammar_table`, `outbound_host_default_is_https_only`, `baked_manifest_states_are_distinct`, `baked_manifest_rejects_reserved_key_at_every_depth_and_case`, `app_macro_manifest_caches_are_per_implementation`, `adapter_capability_default_is_unsupported`, `explicit_manifest_is_authoritative`, `runtime_resolver_rejects_cross_app_pair`, `runtime_resolver_rejects_equal_distance_ambiguity`, `spin_selected_component_hosts_match_as_atomic_sets`, `execute_runtime_gates_each_runtime_action_once`, and `operational_actions_keep_existing_dispatch_path`.

**Expected red:** schema types are initially unresolved; macro fixtures initially compile or share state when they must not; the registry lacks `capability`; resolver tests select/fall back instead of rejecting; Spin drift passes incorrectly; dispatch counters show zero or duplicate gate calls.

### Task 1: Add strict capability and host schema

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

**Public surface:** `Capability`, `CapabilitySupport`, `ManifestCapabilities`, `ManifestOutboundCapability`, `AtomicHost`, `HostParseError`, `HostPat`, `Port`, `Scheme`, and `canonicalize_outbound_host` exactly as specified in §3.5.1.

- [ ] Add failing manifest tests for all seven kebab-case capabilities, unknown keys, duplicate required/optional entries, required/optional overlap, and serialization round trips. Add a table that rejects any nested key equal to `capabilities` ignoring ASCII case, rejects non-lowercase top-level spellings, and accepts only exact lowercase top-level `capabilities`; recurse through tables and arrays.
- [ ] Add table tests for every accepted/rejected host grammar row from §3.5.1, including IPv4/IPv6, wildcard expansion, ports, LDH limits, punycode, raw Unicode, userinfo, paths, queries, fragments, whitespace, and malformed brackets.
- [ ] Map every rejected host row to the exact non-exhaustive `HostParseError` variant and stable nonempty Display text specified in §3.5.1; messages must not echo caller input. Use `Port::Any`, never a nonexistent wildcard variant.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib manifest::tests::`; expect failure.
- [ ] Implement strict `deny_unknown_fields` schema plus the runtime TOML reserved-key walker. Keep the shared `is_reserved_capabilities_key` policy and host parsing inside `manifest.rs`, because that file is textually included by the macro crate.
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
- Create: `crates/edgezero-macros/tests/ui/app_capabilities_mixed_case.rs` and `.stderr`
- Create matching TOML inputs under `crates/edgezero-macros/tests/fixtures/` with the same stems

**Contract:** `BakedManifest::{Absent, Malformed, Present}`, `ManifestContract::{Malformed, None, Present}`, and `Hooks::{manifest_json, manifest}`.

- [ ] Add failing tests for absent, malformed, and present baked manifests; runtime/baked validation parity; reserved keys at every depth/case in both raw TOML and baked JSON; and two app implementations whose caches cannot leak into each other.
- [ ] Add trybuild failures for `capabilities` misplaced under nested tables and arrays at every depth listed by §3.5.3, plus a mixed-case nested spelling.
- [ ] Run `cargo test --offline --locked -p edgezero-macros`; expect failure.
- [ ] In `expand_app`, parse the source manifest to `toml::Value`, run the recursive reserved-key scan before typed deserialization can discard misplaced tables, then deserialize, validate, finalize, and serialize. Generate one `OnceLock` per app implementation, never in a shared default trait method. At runtime, parse baked JSON to `serde_json::Value`, run the equivalent JSON walker using the same case-insensitive key predicate and array recursion, then deserialize, validate, and finalize. Any scan/parse/validation failure is `BakedManifest::Malformed`, never `Absent` or an empty `Present` contract.
- [ ] Rerun macro and core tests; expect success.
- [ ] Commit: `feat(macros): bake outbound capability contracts`.

### Task 3: Add fail-closed adapter capability metadata

**Files:**
- Modify: `crates/edgezero-adapter/Cargo.toml`
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `Cargo.lock`, `examples/app-demo/Cargo.lock`

- [ ] Add failing registry tests for the default `Unsupported` result and a fixture adapter that returns each support level. Do not publish in-tree matrix cells before the owning adapter behavior and evidence land.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter --features cli adapter_capability_`; expect a nonzero failure.
- [ ] Add the direct `edgezero-core` dependency and defaulted `Adapter::capability`. Until Phases 4-6, in-tree adapters inherit `Unsupported`; each owning phase adds its exhaustive override atomically with passing contracts and any required host proof.
- [ ] Refresh both independent lock graphs with `cargo check --offline -p edgezero-adapter --features cli` and `cargo check --offline --manifest-path examples/app-demo/Cargo.toml -p app-demo-cli --tests`; these dependency-refresh commands are intentionally unlocked. The demo command must select a package in the demo workspace; Cargo rejects feature selection for the root workspace's `edgezero-adapter` package through the demo manifest.
- [ ] Assert the new edge with `cargo tree --offline --locked -p edgezero-adapter -e normal | rg 'edgezero-core'` and `cargo tree --offline --locked --manifest-path examples/app-demo/Cargo.toml -p edgezero-adapter -e normal | rg 'edgezero-core'`.
- [ ] Rerun `cargo test --offline --locked -p edgezero-adapter --features cli adapter_capability_`; expect a nonzero test count and success.
- [ ] Commit: `feat(adapter): add fail-closed capability metadata`.

### Task 4: Build the paired runtime resolver

**Files:**
- Modify: `crates/edgezero-adapter/src/lib.rs`
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Create: `crates/edgezero-cli/src/manifest_source.rs`
- Modify: `crates/edgezero-cli/src/lib.rs`
- Modify: `crates/edgezero-cli/src/adapter.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`

**Contract:** declare the exact spec §3.5.3 types in lint order. In
`edgezero-adapter::registry`, add private-field `AdapterExecutionTarget { app_root,
component, platform_manifest }` with public `new` plus read-only accessors, and add
`Adapter::execute_target(action, &target, args)`. Its default returns an explicit pinned-target
unsupported error; it must not call the cwd-discovering `execute`. In `manifest_source`, add
`ResolvedAdapterTarget::{Registered(AdapterExecutionTarget), Shell}`, `ResolvedManifest {
loader, path }`, owned `ResolvedRuntime { action, adapter, contract, target }`, and
`ResolvedShellTarget { bind_host, bind_port, command, environment, root }`. Keep the latter
fields private to `manifest_source`; `ResolvedRuntime` exposes only crate-private read-only
`action()`, `adapter_name()`, `manifest()`, and `target()` accessors, and the contained types
expose read-only accessors for their listed fields. No `ResolvedRuntime` constructor or
mutator is exposed outside the resolver, and module items/struct fields/enum variants/impl
methods must satisfy `arbitrary_source_item_ordering`.

- [ ] Add pure failing tests for explicit `EDGEZERO_MANIFEST`, captured invocation-directory resolution, ancestor precedence, bounded workspace descendants, equal-distance ambiguity, target/contract mismatch, symlink containment, malformed contracts, and the narrow valid `contract: None` case. Assert app/shell roots are absolute canonical directories, contract/platform manifests are absolute canonical regular files contained by their corresponding root, and file/directory type swaps fail closed.
- [ ] Add tests proving shell-command and registry-backed runtime-producing actions receive
  the same pinned target and cannot rediscover a different manifest. Prove the canonical
  adapter identity and requested action are stored in each resolved runtime. For every
  in-tree adapter, run from an unrelated cwd with conflicting discoverable manifests and
  assert `execute_target` uses only `app_root`, `platform_manifest`, and `component`; assert
  the default fixture adapter fails instead of entering its cwd-discovering `execute`.
  Assert the resolver rejects operational actions so auth/version/healthcheck/rollback
  cannot accidentally enter this outbound-only path.
- [ ] Run `cargo test --offline --locked -p edgezero-cli manifest_source`; expect failure.
- [ ] Extract target-argument parsing and discovery into pure helpers. Explicit env selection is authoritative and never falls back. Canonical platform files must remain under the canonical app root. Resolve and pin exactly one adapter identity, manifest contract, target, and runtime-producing action; no later dispatcher step may read environment variables or scan ancestors/workspace descendants.
- [ ] Preserve direct adapter API discovery for callers outside the CLI. Refactor each
  in-tree adapter's build/serve/deploy implementation into target-aware internal helpers;
  direct `execute` discovers once and delegates, while the paired CLI path calls
  `execute_target` with the cross-crate typed target and performs no discovery. Do not use
  process-wide cwd changes or synthetic path/component entries in `adapter_args`.
- [ ] Rerun focused CLI tests; expect success.
- [ ] Commit: `refactor(cli): resolve paired runtime contracts`.

### Task 5: Validate selected Spin component host drift

**Files:**
- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Modify: `crates/edgezero-adapter/src/cli_support.rs`
- Modify: `crates/edgezero-cli/src/manifest_source.rs`

- [ ] Add failing tests for explicit component, single inferred component, missing/ambiguous components, absent hosts, wildcard expansion, scheme case normalization, malformed platform entries, set-order independence, and drift diagnostics containing manifest path/component/expected list.
- [ ] Run `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli spin_selected_component_`; expect a nonzero failure.
- [ ] Implement one pure component selector shared by adapter validation and CLI enforcement. Compare only selected-component canonical atomic sets.
- [ ] Rerun `cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features cli spin_selected_component_`; expect success.
- [ ] Run `cargo test --offline --locked -p edgezero-cli --no-default-features --features cli spin_selected_component_`; expect success with the Spin registry adapter genuinely unlinked.
- [ ] Separately run `cargo test --offline --locked -p edgezero-cli spin_selected_component_`; expect success with the default linked-adapter set.
- [ ] Commit: `feat(cli): reject Spin outbound host drift`.

### Task 6: Gate actions exactly once

**Files:**
- Modify: `crates/edgezero-cli/src/adapter.rs`
- Modify: `crates/edgezero-cli/src/lib.rs`
- Modify: `crates/edgezero-cli/src/demo_server.rs`
- Modify: `crates/edgezero-cli/Cargo.toml`

**Dispatch signatures:** add two outbound-scoped dispatch functions that accept only the owned `ResolvedRuntime` and adapter arguments. They call private `ensure_action_capabilities(&runtime)` exactly once before branching. That helper derives the canonical adapter name, runtime-producing action, and manifest through read-only accessors and delegates to `ensure_capabilities(runtime.adapter_name(), ManifestContract::from_opt(runtime.manifest()))`. The private post-gate dispatcher also accepts the complete runtime and never gates again. There is no duplicate adapter/action argument that can diverge between admission and the eventual side effect. Keep the existing `execute(..)` / `execute_capture(..)` APIs for exempt operational actions unchanged.

```rust
pub fn execute_runtime(
    runtime: ResolvedRuntime,
    adapter_args: &[String],
) -> Result<(), String>;

pub fn execute_capture_runtime(
    runtime: ResolvedRuntime,
    adapter_args: &[String],
) -> Result<Option<String>, String>;
```

- [ ] Add failing tests with fixture adapters for required versus optional capabilities at all four support levels, missing registry behavior, and malformed/absent contracts.
- [ ] Add a counter seam proving `Build`, `Serve`, `Deploy`, and `DeployStaged` gate once in both `execute_runtime` and `execute_capture_runtime` before any shell/registry side effect. Use two fixture adapters and distinct actions to prove capability lookup, shell diagnostics/registry lookup, and final `AdapterAction` conversion all observe the adapter/action stored by the resolver; the outbound-scoped dispatch API must offer no override arguments.
- [ ] Add `operational_actions_keep_existing_dispatch_path`: all three auth actions, emit-version, healthcheck, and rollback continue to call the existing dispatcher, remain exempt from capability gating, and preserve registered no-project behavior. The test must fail if any of them constructs `ResolvedRuntime`, invokes paired manifest discovery, or acquires a manifest requirement. Provision and config are outside this outbound specification and receive no requirements here. Add a demo test proving Axum checks the baked manifest before startup.
- [ ] Run `cargo test --offline --locked -p edgezero-cli`; expect failure.
- [ ] Implement one private post-gate dispatcher so `execute_capture_runtime` cannot re-enter public `execute_runtime`. Required accepts only Native/BoundedCooperative; optional warns for BestEffort/Unsupported. Derive adapter/action exclusively from `ResolvedRuntime` in every outbound-gated branch. Leave `auth.rs` and the existing emit-version/healthcheck/rollback dispatch path unchanged; after a captured Deploy lacks a version, retain today's registered `EmitVersion` call rather than broadening this resolver.
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
