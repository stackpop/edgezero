# Ingress Fallback Review Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let applications hold an admission lease while enforcing a finite body cap and absolute read deadline before EdgeZero emits a pre-resolved 404 or 405, with application-controlled overflow and timeout responses.

**Architecture:** Extend the synchronous, body-blind `ReadBodyBeforeFallback` disposition with a transferable `IngressGrant` and buffered terminal responses. Core retains the exact resolved route token and grant for the full drain lifetime, adapters install the existing deadline-bound lazy body, and resolved dispatch maps typed drain outcomes to either the application response or canonical 404/405 without middleware, handlers, rematching, or string classification. Response-egress capability levels remain unchanged and explicitly document the current certification boundary.

**Tech Stack:** Rust 2024, `edgezero-core`, Axum/Hyper adapter integration tests, Handlebars project templates, VitePress Markdown, Cargo workspace CI.

---

## Task 1: Add the bounded fallback admission contract

**Files:**
- Modify: `crates/edgezero-core/src/ingress.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/router.rs`
- Modify: `crates/edgezero-core/src/app.rs`

- [x] Add failing core tests proving `ReadBodyBeforeFallback` is accepted only for `NotFound` and `MethodNotAllowed` and is rejected for matched routes before body polling.
- [x] Add failing dispatch tests proving an exact-cap lengthless body produces the canonical 404/405, the first byte over produces 400 before 404/405, and an expired absolute deadline produces 408.
- [x] Run the focused core tests and record the expected failures caused by the missing variant and dispatch behavior.
- [x] Add the initial bounded fallback disposition. Task 6 hardens its final contract with a transferable grant and exact buffered terminal responses while retaining the no-context/no-handler path.
- [x] Reuse the existing deadline checks in a bounded count-and-discard drain for fallback dispatch. Do not rematch, invoke middleware, construct a routed request context, or poll bodies for ordinary `Admit`/`Refuse` outcomes.
- [x] Run `cargo test -p edgezero-core --lib ingress`, `cargo test -p edgezero-core --lib app`, and `cargo test -p edgezero-core --lib router` until green.

## Task 2: Prove adapter-level 400-before-404/405 behavior

**Files:**
- Modify: `crates/edgezero-adapter-axum/src/service.rs`

- [x] Add failing Axum service tests with body streams that omit `Content-Length`: exact-cap unmatched and wrong-method requests retain 404/405, while one-byte-over requests return 400.
- [x] Add a deadline test proving fallback draining uses the admitted absolute read deadline and releases the native body without middleware or handler execution.
- [x] Run the focused tests before production changes to confirm the missing core behavior is the failure source.
- [x] Make only the adapter wiring changes required by the public contract; standard adapters must continue to wrap the unread native body before `dispatch_admitted`.
- [x] Run `cargo test -p edgezero-adapter-axum --all-targets`.

## Task 3: Keep the demo and generated project current

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`

- [x] Add failing demo/template assertions for a 4 KiB unmatched/wrong-method fallback cap and finite fallback deadline.
- [x] Update both admission callbacks to return `ReadBodyBeforeFallback` for `NotFound` and `MethodNotAllowed`; matched routes retain route-class grants and class-aware deadlines.
- [x] Add demo lifecycle tests for under-cap 404/405 and overflow 400 precedence.
- [x] Extend generator source assertions so future templates cannot silently drop the fallback policy.
- [x] Run demo core tests, generator tests, and the ignored generated-workspace integration test.

## Task 4: Correct evidence and certification documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/superpowers/specs/2026-09-08-response-egress-design.md`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/superpowers/plans/2026-09-08-inbound-ingress-admission.md`
- Modify: `docs/superpowers/plans/2026-09-08-response-egress-implementation.md`

- [x] Specify the bounded fallback decision, ordering, cap/deadline errors, route-token preservation, and no-handler/no-middleware behavior.
- [x] Mark parser-level head-limit and Axum raw-socket framing rows as future acceptance criteria. State that every current adapter reports `HostManaged` and both raw ingress capabilities remain `Unsupported`.
- [x] State that Axum currently reports `ResponseReturned` after conversion but before Hyper transmission. Keep abort, backpressure, completion, and write-deadline capabilities `Unsupported` and identify them as certification blockers, not implemented delivery guarantees.
- [x] Update implementation-plan status notes without claiming unfinished raw-boundary or transport-egress work.
- [x] Run documentation format, lint, build, and contract checks.

## Task 5: Full verification and PR update

- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- [x] Run `scripts/run_tests.sh`, generated-project validation, demo target checks, and `git diff --check`.
- [x] Self-review the complete diff for precedence, cancellation, grant lifetime, error redaction, capability honesty, and demo/template parity.
- [ ] Update PR 275 metadata, commit, push, wait for all checks, and verify a clean synchronized branch.

## Task 6: Transfer the fallback grant and terminal responses

**Files:**
- Modify: `crates/edgezero-core/src/ingress.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/router.rs`
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-spin/src/request.rs`
- Modify: adapter contract tests under `crates/edgezero-adapter-*/tests/contract.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`
- Modify: `crates/edgezero-cli/src/templates/core/src/lib.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `docs/superpowers/specs/2026-08-22-inbound-body-design.md`
- Modify: `docs/guide/capabilities.md`

- [x] Add failing core tests proving a non-empty fallback grant remains live during every body poll and drops exactly once after EOF, overflow, timeout, source failure, or cancellation.
- [x] Add failing core tests proving saturated admission returns an application 503 without polling the body, and fallback terminal responses preserve exact application-selected status, headers, and buffered bytes.
- [x] Add a cloneable buffered ingress-response value and extend `ReadBodyBeforeFallback` with `grant`, `on_exceeded`, and `on_timeout`. Keep `Refuse` as the zero-read overload path.
- [x] Replace fallback drain message classification with private `Complete`, `Exceeded`, and `TimedOut` outcomes. Hold the grant through the drain, release it before response conversion, preserve source errors, and retain canonical 404/405 only after clean EOF.
- [x] Extend Axum, Cloudflare, Fastly, and Spin contract suites so each adapter exercises exact-cap `NotFound` and `MethodNotAllowed` requests, first-byte-over precedence, absolute deadline behavior, zero middleware/handler calls, source release, zero-read refusal, and grant lifecycle at the strongest capability each host exposes. Axum additionally pins deadline-bounded release after an outer abort request because its blocking bridge is not promptly cancellable.
- [x] In every adapter contract suite, assert the exact application-selected overflow and timeout status, headers, and buffered body bytes so adapter conversion cannot silently restore the generic JSON 400/408 renderer.
- [x] Update the demo and generated app policy to carry a fallback lease plus explicit plain-text overflow and timeout responses; extend generator assertions and lifecycle tests so the two remain identical.
- [x] Update the ingress specification, capability guide, and this plan to remove the empty-grant and generic-error claims and state the exact lifecycle and wire-response contract.
- [x] Run focused core, adapter, demo, generator, documentation, full workspace, strict Clippy, feature, and WASM target checks before committing and pushing.
