# Demo and Generator Lifecycle Alignment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the checked-in demo and newly generated projects exercise the current outbound HTTP and ingress-admission contracts instead of relying on compatibility defaults.

**Architecture:** Extend `app!` with one optional `configure = <expr>` callback that implements the existing `Hooks::configure(&mut App)` seam. The demo and core template use that callback to install a finite, route-aware admission policy and opaque request grant. Their outbound examples share explicit encoded, decoded, final-buffer, request-body, header, Brotli, and timeout policy, while a batch endpoint exposes positional per-slot elapsed time.

**Tech Stack:** Rust, proc macros (`syn`/`quote`), Handlebars templates, TOML manifests, EdgeZero core/adapters, native and WASM contract tests.

---

### Task 1: Add the macro configuration callback

**Files:**
- Modify: `crates/edgezero-macros/src/app.rs`
- Modify: `crates/edgezero-macros/tests/app_macro.rs`

- [x] Add parser tests for `configure = crate::configure_app`, mixed keyword ordering, duplicate rejection, and the updated unknown-key diagnostic.
- [x] Run the focused macro tests and confirm they fail because `configure` is not accepted.
- [x] Parse one optional configure expression and emit `Hooks::configure(app)` as a call to it; preserve the empty default for existing macro invocations.
- [x] Add an integration test proving `Hooks::build_app()` invokes the emitted callback and installs its policy.
- [x] Run all macro tests and strict macro clippy.

### Task 2: Align ingress behavior in the demo and scaffold

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `examples/app-demo/edgezero.toml`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`

- [x] Add failing demo tests proving manifest route classes reach resolution, the configured callback supplies finite class-aware deadlines, and the opaque grant can be consumed exactly once by a handler.
- [x] Add route classes for health, diagnostic, and outbound routes in both manifests.
- [x] Define a small application-owned admission lease and configure callback in both core crates/templates; outbound routes receive a tighter read budget and every request receives a finite deadline.
- [x] Add an admission diagnostic handler that consumes the typed grant and confirms its route class without exposing provider data.
- [x] Run the demo core tests and generated-source assertions.

### Task 3: Align outbound examples with current limits and timing

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `examples/app-demo/edgezero.toml`
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/README.md.hbs`

- [x] Extend outbound mock tests first to require explicit per-call timeout, request-body limit, independent encoded/decoded/final limits, header byte/count caps, and Brotli policy.
- [x] Apply one shared outbound policy helper to proxy and batch requests in both demo and template.
- [x] Add a positional batch endpoint that computes one absolute deadline, calls `send_all`, and returns each slot's index, elapsed milliseconds, and typed success/failure category.
- [x] Add tests for positional ordering, empty batches, and distinct per-slot elapsed values.
- [x] Document the generated routes, configured limits, timing semantics, and adapter timing-quality caveat.

### Task 4: Lock generator/demo parity

**Files:**
- Modify: `crates/edgezero-cli/src/generator.rs`
- Test: generated project under a temporary directory

- [x] Add generator assertions for the configure callback, ingress policy/grant, route classes, independent outbound caps, and per-slot timing endpoint.
- [x] Run focused generator tests and confirm the new assertions fail before template changes, then pass after them.
- [x] Generate a fresh all-adapter project and run its tests and strict clippy.
- [x] Run the checked-in demo tests, strict clippy, and all three adapter WASM checks.

### Task 5: Full verification and PR update

**Files:**
- Modify: PR 275 description through `gh`

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run `scripts/run_tests.sh` and documentation checks.
- [ ] Update PR metadata, commit, push, wait for all checks, and verify a clean synchronized branch.
