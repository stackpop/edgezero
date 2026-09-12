# Outbound HTTP Review Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining current-head review gaps by preventing provider diagnostics from reaching HTTP responses, splitting typed config failure reasons, and carrying manifest route class plus one injectable monotonic clock through ingress and outbound HTTP.

**Architecture:** Keep detailed errors available inside `EdgeError`, but render fixed category-only messages at the wire boundary and stop adapters from embedding provider values where no internal diagnostic is needed. Hard-cut the ambiguous config reasons so exhaustive downstream matches must migrate to the precise taxonomy. Route identity remains method plus pattern, while route class is optional metadata. Use one `App`-owned portable clock handle backed by `web_time` by default; the same handle captures ingress start, evaluates admitted body deadlines, and is cloned into the standard outbound client installed by every adapter.

**Tech Stack:** Rust 2024, `web-time`, `http`, Axum/Tokio, Cloudflare Workers, Fastly Compute, Fermyon Spin, Cargo workspace and WASM target checks.

---

### Task 1: Wire-Safe Server Error Messages

**Files:**
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Test: colocated unit tests in the files above

- [x] Add token-bearing tests proving `BadGateway`, `GatewayTimeout`, `Internal`, and `ResponseTooLarge` preserve their internal diagnostics while `IntoResponse` emits only fixed category messages and never serializes the token, URL, query, or provider text.
- [x] Run `cargo test -p edgezero-core error` and verify the new tests fail on the current `self.message()` wire rendering.
- [x] Add a private exhaustive `wire_message()` policy and use it only in `IntoResponse`; keep `message()`, `Display`, typed reasons, and causes available for internal inspection.
- [x] Replace Axum, Cloudflare, and Fastly outbound provider-error interpolation with fixed diagnostics. Keep Spin's already category-only mappings unchanged.
- [x] Add token-bearing wire regression coverage and adapter-facing secret-error assertions; scan every outbound provider mapping to verify rendered responses cannot contain provider values.
- [x] Run `cargo test -p edgezero-core` and focused adapter tests.

### Task 2: Split Typed Config Extraction Reasons

**Files:**
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/extractor.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `docs/superpowers/specs/2026-06-16-blob-app-config.md`
- Modify: `docs/superpowers/plans/2026-06-17-blob-app-config.md`

- [x] Add failing tests for four independently inspectable reasons: `Deserialization`, `Validation`, `MalformedEnvelope`, and `UnsupportedVersion`.
- [x] Run the focused extractor and error tests and verify the pre-split reason assertions fail.
- [x] Add the four variants to the non-exhaustive enum and its exhaustive wire status/kind policy. Remove the ambiguous predecessor variants; this is an intentional compile-time migration break.
- [x] Map malformed envelope parsing, unsupported envelope version/discriminator, typed `data` deserialization, validator failures, and structural secret-walk failures to their exact new reasons.
- [x] Update stored-error round trips and exhaustive reason tests.
- [x] Run `cargo test -p edgezero-core`.

### Task 3: Manifest Route Class Metadata

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`
- Modify: `crates/edgezero-core/src/router.rs`
- Modify: `crates/edgezero-macros/src/app.rs`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/plans/2026-09-08-inbound-ingress-admission.md`

- [x] Add failing manifest, macro-token, router-resolution, admission-head, and request-context tests for optional `class = "auction"` metadata.
- [x] Run focused core and macro tests and verify the field/API are absent.
- [x] Add optional validated `class` to `ManifestHttpTrigger`; do not overload the existing unique trigger `id`.
- [x] Add optional class to `RouteMetadata` without changing `RouteId`; add `RouteMetadata::class()` and a builder path for classed routes while preserving all existing route methods.
- [x] Make `app!` propagate each trigger's class for every generated method. Ensure 404/405 behavior and deterministic allowed-route ordering remain unchanged.
- [x] Run `cargo test -p edgezero-core` and `cargo test -p edgezero-macros`.

### Task 4: Paired Injectable Ingress Clock

**Files:**
- Modify: `crates/edgezero-core/src/time.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-core/src/ingress.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Test: colocated core and adapter tests

- [x] Add failing tests with a manually advanced clock proving the same source captures `request_start`, normalizes the admission deadline, wins simultaneous body readiness/expiry, and poisons repeated body reads exactly once.
- [x] Add a cloneable `MonotonicClock` handle wrapping `Fn() -> MonotonicInstant + Send + Sync`, with a `web_time` default. Make `Deadline::remaining_at` and `is_expired_at` public clock-paired operations.
- [x] Store the clock on `App`, expose a setter and snapshot method, attach it to admitted ingress, and carry it into `RequestContext`.
- [x] Change all standard adapter entry points to capture start from `App`'s clock as their first EdgeZero-owned operation. Pass the admitted clock into each lazy body wrapper and use it for pre-poll/post-ready deadline decisions; retain each platform's strongest available timer/cancellation primitive.
- [x] Keep low-level conversion APIs explicitly non-admitting and preserve their existing default-clock behavior.
- [x] Run focused core and all four adapter request/contract tests.

### Task 5: Documentation, Metadata, and Full Verification

**Files:**
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/superpowers/specs/2026-06-16-blob-app-config.md`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: relevant implementation plans under `docs/superpowers/plans/`
- Modify: PR 275 title/body through `gh`

- [x] Document fixed wire messages, split config reasons, route class semantics, and the paired clock contract.
- [x] Preserve explicit `Unsupported` declarations for raw parser accounting, provider-side config allocation/cancellation, and transport-observed response abort/backpressure/completion/write deadlines.
- [x] Update the PR title from design-only wording and refresh the body without claiming unsupported guarantees.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Compile and lint Cloudflare, Fastly, and Spin on their target-specific WASM triples; run the exact Fastly and Spin CI sentinels locally.
- [x] Make the Fastly outbound concurrency sentinel part of the WASM contract binary, as required by the existing CI matrix, and prove it under Viceroy.
- [x] Confirm the Cloudflare browser-runtime contract through the PR's Linux CI job; local Safari WebDriver cannot start in this environment.
- [x] Run documentation contract, formatting, lint, and build checks.
- [x] Commit, push, wait for PR checks, and verify the branch is clean and synchronized.

### Task 6: Final Self-Review Corrections

**Files:**
- Modify: `crates/edgezero-core/src/error.rs`
- Modify: `crates/edgezero-core/src/extractor.rs`
- Modify: `crates/edgezero-core/src/ingress.rs`
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `.github/workflows/test.yml`

- [x] Add red regressions for escaped JSON discriminators, config provider-message redaction,
  generic Cloudflare fetch classification, paired relative admission deadlines, and native source
  release when a terminal body error is emitted.
- [x] Replace textual discriminator detection with structural JSON inspection and keep malformed
  envelopes distinct from unsupported versions.
- [x] Redact direct config-store provider failures at conversion and classify an opaque Cloudflare
  fetch rejection as `Unspecified` rather than claiming connection-phase evidence.
- [x] Add a request-start-relative deadline helper, stamp ingress before store resolution on every
  adapter path, and release body sources at the terminal item rather than retaining them until the
  wrapper is dropped.
- [x] Correct BestEffort cancellation wording, stale Spin request-conversion docs, and unsafe
  config-plan examples; add explicit Fastly WASM concurrency and public ingress-capability checks.
- [x] Remove the superseded serde constructor and verify no active code depends on it.
- [x] Run focused red/green tests after each correction, then repeat every Task 5 verification gate.

### Task 7: Pair outbound clients with the application clock

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/outbound.rs`
- Modify: `crates/edgezero-adapter-axum/src/request.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: adapter contract tests under `crates/edgezero-adapter-*/tests/contract.rs`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `docs/guide/capabilities.md`

- [x] Add failing per-adapter tests proving method-entry anchoring, preflight elapsed timing, post-ready expiry, backwards-clock handling, and clock retention by streamed upload/response paths. Cloudflare and Spin deferred-path probes run through their hosted contract binaries rather than unexecuted library-only tests.
- [x] Store a `MonotonicClock` on every native outbound client. Preserve default-clock constructors for low-level use and add explicit clock constructors for standard adapter wiring and tests.
- [x] Install each standard request's outbound client with the exact `App::monotonic_clock()` clone used by ingress; keep standalone low-level request converters explicitly default-clocked.
- [x] Replace every production outbound `MonotonicInstant::now`, `Deadline::remaining`, and `Deadline::is_expired` call with the client clock plus explicit `remaining_at` or `is_expired_at`; carry clock clones into all deferred body streams and Fastly pending slots.
- [x] Keep `DispatchBudget` as the copyable result of pure budget selection while ensuring its start snapshot, all later deadline checks, error precedence, dispatch-slack checks, and slot completion observations use one clock domain. Fastly backend identity uses the method-entry `budget.duration`; preparation-time clock samples cannot fragment a homogeneous batch's dynamic-backend identity.
- [x] Update the outbound specification and capability guide with constructor semantics, app-clock propagation, low-level defaults, and the full injected-clock acceptance matrix.
- [x] Run focused outbound adapter tests, source scans for global-clock bypasses, full workspace tests, strict Clippy, feature builds, and all three WASM target checks before committing and pushing.

### Task 8: Final implementation-review corrections

**Files:**
- Modify: `crates/edgezero-core/src/{app,extractor,lib,outbound,response_egress}.rs`
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/{outbound,response}.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `docs/guide/proxying.md`
- Modify: outbound and response-egress specifications

- [x] Validate HEAD/304 representation `Content-Length` as one consistent `u64`, reject malformed/conflicting/overflow values before body polling, retain valid metadata, and continue stripping the field on 1xx/204.
- [x] Carry the application clock through `OutboundResponse`, cooperative bounded-until collection, typed config extraction, `ResponseEgressEnvelope`, and every adapter response converter.
- [x] Preserve Cloudflare's intentional abort-on-drop for framing-bodyless responses; handle a null-body 205 without calling `stream()`, disarm only clean absent/zero-length suppression, and keep positive declared length on the abort path.
- [x] Pin Spin's `BodyWriter` default-on-drop host-error contract in source and retain nonblocking deadline exits instead of awaiting after budget expiry.
- [x] Add `max_chunk_bytes` to the demo, generated template, generator sentinel, and both copyable proxy-guide examples; document that rechunking bounds emitted item shape, not provider allocation.
- [x] Add Cloudflare/Spin three-slot preflight-order coverage, executable adapter write-deadline probes, Cloudflare abort lifecycle coverage, and host-duration floor/saturation tests.
- [x] Document intentional zero-cap deny-all behavior and keep loop/self-reference policy explicitly application-owned.
- [x] Run every workspace, strict lint, documentation, generated-project, demo, and target-specific WASM gate.
