# Lifecycle contract completion

> Historical implementation record, reconciled with PR #275's outbound hard-cut.
> Raw conversion, response-returning dispatch, and collected response paths are
> superseded by canonical admission and adapter-owned lazy egress. The
> [outbound HTTP design](../specs/2026-05-21-outbound-http-design.md) is authoritative;
> no compatibility aliases restore removed APIs. Recorded checkmarks and test
> counts below predate reconciliation and require current verification.

**Goal:** Make the supported custom lifecycle executable and keep target-specific
configuration validation scoped to its destination without disclosing secret references.

**Architecture:** Keep provider scheduling separate from successful-only app
retention. Assemble with `App::build::<A>(PLATFORM) -> Result<App, EdgeError>` and
fallible `Hooks::configure`. Fastly uses `Sandbox`, `serve_app`, and
`serve_app_with_hooks` with borrowing preparation/finalization and owned sending;
no raw conversion or response-returning helper bypasses admission or egress.
Cloudflare dispatch takes `(&App, Request, Env, Context)` and returns a Worker
response; Spin takes `(&App, SpinRequest)` and returns raw `SpinResponse`. Both
select `A::stores()` and resolve fresh registries/admission/egress per invocation,
without building an app or installing logging. Applications own retry policy and
never cache initialization errors. Standalone config validation remains the
portability check; push and diff validate only their selected adapter.

## Lifecycle acceptance

- [x] Extend the custom Fastly fixture and smoke runner with health bypass,
      failed initialization, successful retry, and subsequent retained reuse.
- [x] Produce request-specific finalization metadata inside the router and verify
      its value after dispatch, including across reused callbacks.
- [x] Document the provider/application boundary, retry policy, version pinning,
      and the existing repeatable compatibility command in the Fastly guide.
- [x] Run fixture unit tests and the actual Fastly smoke suite. Missing observed
      reuse must remain unverified, never a passing recovery assertion.

## Configuration correctness

- [x] Add regression tests for redaction of invalid and colliding Spin secret
      references; verify failure before changing diagnostics.
- [x] Add selected-adapter push/diff regressions with unrelated invalid Spin
      configuration, preserving standalone validation and selected-Spin failures.
- [x] Scope shared, typed, and strict capability validation to the selected
      adapter for push/diff, retaining global schema and handler checks.
- [x] Keep field paths and naming rules in errors; omit raw and normalized values.

## Historical verification

- [x] Run scoped tests after code changes, then workspace tests, format, Clippy,
      all-adapter feature checks, and Spin WASM compilation.
- [x] Run documentation format/lint and independent diff review.
- [x] Report local runtime evidence separately from deployed behavior. Do not
      publish, tag, deploy, or change release pins as part of this work.

Verified locally: 1,432 workspace tests passed (one existing generated-project
test ignored); 19 fixture tests passed; workspace and fixture Clippy passed,
including the Fastly WASM fixture; all-adapter checks and Spin WASM check passed;
documentation format/lint passed. Viceroy 0.17.0 smoke passed with all five
initialization callbacks in one guest. Independent lifecycle and CLI reviews
found no blocking defects. Deployed behavior remains outside this evidence.
