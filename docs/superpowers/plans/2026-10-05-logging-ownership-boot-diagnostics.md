# Logging Ownership and Boot Diagnostics Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this approved plan with test-first changes and verification checkpoints.

**Goal:** Share log-level resolution and preserve boot warnings without raising ordinary logging verbosity.

**Architecture:** Use the existing `log` facade, one pure core resolver, and a fixed boot target. Reuse backend target filtering, initialize logging before application configuration, and defer configuration-read diagnostics as bounded state.

**Tech Stack:** Rust, log, fern/log-fastly, simple_logger, Cargo, WASM contract tests.

---

## Task 1: Shared Policy and Ownership

Files: `crates/edgezero-core/src/{logging,lib,app}.rs`.

- [x] Add resolver and boot-target tests; run `cargo test -p edgezero-core logging` and observe the missing-feature failure.
- [x] Implement `resolve_logging_level(&EnvConfig) -> LevelFilter`, `BOOT_LOG_TARGET`, and `BOOT_LOG_LEVEL`; document application filter/initialization ownership.
- [x] Rerun core tests and verify the resolver leaves the global maximum unchanged.

## Task 2: Runtime Filtering and Startup Order

Files: Fastly `src/{logger,lib}.rs`, Axum `src/dev_server.rs`, Cloudflare/Spin `src/lib.rs`, adapter contract tests.

- [x] Add failing filter/order tests before changing runtime initialization.
- [x] Share level parsing; preserve boot warnings at every Fastly filtering layer and in Axum target filtering.
- [x] Initialize logging before application configuration and defer early setup warnings with bounded state. Hard-cut Fastly resolution to `FastlyRuntimeConfig` with explicit one-shot emission.
- [x] Preserve application-owned logging state and explicitly retain Cloudflare/Spin no-op initialization.
- [x] Run affected adapter tests and WASM compilation/lint gates.

## Task 3: Demo, Generator, and Documentation

Files: CLI `src/{main,demo_server,adapter}.rs`, CLI tests/templates/README, adapter/configuration guides, `CHANGELOG.md`.

- [x] Add a failing bundled-demo logger-selection regression; avoid preinstalling the CLI backend for `demo`. Retain the capability gate using Axum's reusable logger-first preflight runner.
- [x] Update ownership/filter examples, no-op platform limitations, and boot `Warn` versus ordinary `Off` semantics.
- [x] Keep generated entrypoints free of duplicated logging implementation.
- [x] Run CLI/generator and demo checks. The bundled logging contract, excluded demo workspace tests, and full generated native/WASM workspace check pass.

## Task 4: Verification and Delivery

- [x] Review the complete diff with a read-only reviewer and resolve findings. Fastly and Axum/CLI reviewers found no production defect; harden SDK capture, public one-shot emission, conflicting-logger startup, and actual demo boot-warning assertions. Keep provider startup guarantees source-reviewed rather than claiming deployed evidence.
- [x] Run workspace tests, formatting, strict workspace clippy, feature checks, three WASM targets, docs lint/format, and generated/demo checks.

Delivery record: [PR #275](https://github.com/stackpop/edgezero/pull/275) commit history and hosted check results.

## Verification Evidence

- Required native workspace test gate: pass, including 713 core tests and the new startup probes.
- Strict all-feature workspace clippy, Fastly CLI-only/no-feature clippy, and feature compilation: pass.
- Cloudflare, Fastly, and Spin WASM clippy with runtime/test-utils features: pass.
- Excluded app-demo tests, strict clippy, formatting, and all three WASM builds: pass.
- Full generated-workspace native/strict-clippy/core-test/WASM check: pass.
- Bundled demo CI sentinel: pass; runtime `Off` retains a real boot warning and terminal stderr.
- Strict public Rustdoc, docs lint/format/build, outbound docs, codec pins, and JSON-map audits: pass.

Native `cargo test --all-features` is not the repository's test gate: it cannot link Fastly
runtime host imports on macOS. Runtime features are compile/lint checked and exercised by
provider-hosted CI. This change does not claim new deployed logger evidence.
