# AnyEdge TODO

High-level backlog and decisions to drive the next milestones.

## Current Task (expand_action_impl coverage + cleanup)
- [x] Audit `expand_action_impl` behaviour to list any untested code paths or edge cases (e.g., tuple destructuring errors, non-RC tuple structs, extractor invocation).
- [x] Add focused tests in `crates/anyedge-macros/src/action.rs` to cover the missing scenarios while keeping contract unchanged.
- [x] Simplify the implementation if warranted (e.g., helper extraction, clearer error handling) without altering observable behaviour; highlight the deltas in the review section.
- [x] Run `cargo test` workspace-wide and note results.
- [x] Record a new review entry summarising the changes, assumptions, and outstanding items.

## Review (2025-11-07 17:52:45 UTC)
- Summary: Extracted RequestContext pattern normalisation into a helper for clearer error aggregation and broadened the macro tests to cover attribute arguments, self receivers, tuple binding mistakes, and extractor codegen so `expand_action_impl` remains well-specified.
- Assumptions: String-matching the generated `FromRequest` call stays stable because we don't plan to rename that trait or method; tuple destructuring for RequestContext will continue to expect a single binding.
- Outstanding: None; `cargo test` (workspace) succeeded after the new tests.

## Current Task (template proxy_demo parity)
- [x] Update `crates/anyedge-cli/src/templates/core/src/handlers.rs.hbs` so the generated `proxy_demo` matches the example handler (`#[action]` + `RequestContext(ctx): RequestContext` binding).
- [x] Run `cargo test` to ensure the template change doesn’t break the workspace.
- [x] Capture a review entry summarising the template update and any follow-ups.

## Review (2025-11-07 17:55:17 UTC)
- Summary: Aligned the CLI template’s `proxy_demo` definition with the app demo (now tagged with `#[action]` and destructuring `RequestContext`) so generated projects inherit the same extractor ergonomics.
- Assumptions: Template consumers expect the proxy route to behave like the example app; no additional template files reference the old signature.
- Outstanding: None; `cargo test` across the workspace passed after the template tweak.

## Current Task (RequestContext pattern support)
- [x] Teach the `#[action]` macro to accept tuple-struct style parameters like `RequestContext(ctx): RequestContext` by normalising that pattern to the owned context argument (ensure the same ownership semantics and keep duplicate checks).
- [x] Update the `proxy_demo` handler in `examples/app-demo/crates/app-demo-core/src/handlers.rs` to demonstrate the new binding style.
- [x] Run `cargo test` for the workspace and fix any regressions.
- [x] Append a new review entry (summary, assumptions, outstanding items) once the change is complete.

## Review (2025-11-07 17:48:52 UTC)
- Summary: Normalised `RequestContext` parameters passed through `#[action]` so tuple-style bindings (e.g. `RequestContext(ctx): RequestContext`) collapse to the owned context, added regression tests covering the pattern, and showcased the syntax on `proxy_demo`.
- Assumptions: Only one owned `RequestContext` parameter should exist per handler; tuple-style bindings only contain a single pattern element.
- Outstanding: None; `cargo test` passed for the entire workspace (no warnings after the final iteration).

## Review (2025-11-07 17:39:28 UTC)
- Summary: Extended `#[action]` to accept a single `RequestContext` parameter (with duplicate detection and helper tests) and applied the attribute to `proxy_demo`, relying on the original handler logic.
- Assumptions: `RequestContext` parameters are owned values (no reference variants needed) and only one is expected per handler.
- Outstanding: None; `cargo test` across the workspace passed after the macro test adjustment.

## Queue (near-term)

### High Priority
- [ ] CI: add `fmt`, `clippy`, and `test` workflows
- [ ] CLI: `anyedge build --adapter fastly|cloudflare`
- [ ] CLI: `anyedge deploy --adapter fastly|cloudflare`
- [ ] Core: `Response::json<T: serde::Serialize>` behind `serde` feature
- [ ] Fastly proxy: add tests for backend send + header/body mapping
- [ ] Cloudflare streaming: map `Response::with_chunks` to `ReadableStream` with backpressure
- [ ] Cloudflare demo: add minimal `wrangler` example and verify `wrangler dev`
- [ ] Core proxy: add async fetch facade (feature `async-client`) and implement Cloudflare proxy

### Medium Priority
- [ ] Fastly demo: add optional backend example to validate `req.send("backend")`
- [ ] Core: utility to serve embedded static files via `include_str!`/`include_bytes!` with proper Content-Type and HEAD (no body) handling
- [ ] Docs: document header/method/status mapping to `http` crate
- [ ] Docs: add RouteOptions + streaming policy section (examples done; expand README section)

### Completed / Ongoing Maintenance
- [x] CLI: `anyedge build --adapter fastly` to package a Fastly Compute@Edge artifact
- [x] CLI: `anyedge deploy --adapter fastly` using Fastly CLI/API
- [x] Core: helper to fetch all header values for a name (multi-value support)
- [x] Docs: clean stale references to removed hello example from README/targets
- [x] Docs: highlight controller-based workflow and update examples
- [x] Controllers: add `anyedge-controller` + `anyedge-macros` with typed extractors and `#[action]` helpers

## Test Coverage Plan (2025-09-18)
- [ ] Adapters: introduce Fastly/Cloudflare mapping tests (headers, streaming, proxy failure) to catch glue regressions.
- [ ] Adapters: assert error-path mapping for Fastly/Cloudflare request conversion and re-enable the ignored Cloudflare response header test.
- [ ] CLI: add integration tests for `anyedge new` scaffolding, feature-flag builds, and `dev` fallback app.
- [ ] CLI: cover `dev_server`, generator, and template scaffolding flows with tempdir-based integration tests to guard manual HTTP parsing and shell commands.
- [ ] CI: verify feature combinations (without `dev-example`, `json`, `form`) compile and run basic smoke tests.
- [ ] Macros: add trybuild coverage for `app!` manifest expansion (route/middleware generation and error surfacing).
- [x] Core: unit-test `App::build_app`/`Hooks` wiring and `PathParams::deserialize` edge cases beyond indirect coverage. *(Added targeted unit tests in `crates/anyedge-core/src/app.rs` and `crates/anyedge-core/src/params.rs`.)*
- [x] Coverage hygiene: consolidate duplicate router/extractor request-parsing tests and share adapter contract fixtures to reduce redundant maintenance. *(Router duplicates trimmed; extractor suite now owns request parsing checks.)*
- [x] Router: add regression cases for overlapping routes, prefix/nest behaviour, and BodyMode error handling (`Streaming` vs `Buffered`).
- [x] Controller: cover negative paths (`State<T>` missing, `ValidatedJson` errors) and assert returned status/body.

## Milestones
- [ ] Fastly adapter MVP
  - [x] Map Fastly Request -> `anyedge-core::Request`
  - [x] Map `anyedge-core::Response` -> Fastly Response (headers/body)
  - [x] Handle binary/text bodies and content-length
  - [x] Example: runnable Fastly demo (`examples/app-demo/crates/app-demo-adapter-fastly`) with wasm32-wasip1 target and explicit logger init
  - [x] Streaming: native streaming on Fastly via `stream_to_client` (wasm32-wasip1), buffered fallback elsewhere
- [ ] Cloudflare Workers adapter MVP
  - [x] Basic mapping: Workers Request -> AnyEdge Request
  - [x] Basic mapping: AnyEdge Response -> Workers Response (buffered bodies)
  - [ ] Streaming behavior (ReadableStream) and backpressure
  - [ ] Example: deploy sample with `wrangler` (`examples/app-demo/crates/app-demo-adapter-cloudflare`)
- [ ] CLI
  - [x] `anyedge new <name>`: scaffold an app (lib with `build_app()`)
  - [ ] `anyedge build --adapter fastly|cloudflare`
  - [ ] `anyedge deploy --adapter fastly|cloudflare`
  - [ ] `anyedge dev` improvements: better HTTP parsing, hot-reload
- [ ] Config
  - [ ] `anyedge.toml`: app name, routes, provider-specific settings
  - [ ] Secrets/ENV strategy and provider bindings
- [ ] Observability
  - [ ] Logging levels; feature-gated tracing
  - [ ] Metrics hooks; request timing middleware
- [ ] Router
  - [ ] Wildcards (`*`), optional segments, query params helpers
  - [x] HEAD/OPTIONS helpers; 405 handling
  - [ ] 501 handling
- [ ] Responses
  - [ ] JSON helper (serde optional feature)
  - [x] Streaming/chunked support
- [ ] Errors
  - [ ] Unified error type in core; map provider errors to HTTP 5xx
- [ ] Docs
  - [ ] Provider guides (Fastly, Cloudflare)
  - [ ] CLI reference
  - [ ] Example cookbook
- [ ] CI
  - [ ] Run `cargo fmt`, `cargo clippy`, and tests

## Roadmap (2025-09-24)
- [x] Adapter stability: formalise the provider adapter contract (request/response mapping, streaming guarantees, proxy hooks) and capture it in shared docs + integration tests so new targets plug in safely. (`docs/adapter-contract.md`, Fastly contract tests under `crates/anyedge-adapter-fastly/tests/contract.rs`, Cloudflare contract tests under `crates/anyedge-adapter-cloudflare/tests/contract.rs`, manifest schema in `docs/manifest.md`)
- [ ] Provider additions: prototype a third adapter (e.g. AWS Lambda@Edge or Vercel Edge Functions) using the stabilized adapter API to validate cross-provider abstractions.
- [x] Manifest ergonomics: design an `anyedge.toml` schema that mirrors Spin’s manifest convenience (route triggers, env/secrets, build targets) while remaining provider-agnostic; update CLI scaffolding accordingly. (`crates/anyedge-cli/src/manifest.rs`, templates in `crates/anyedge-cli/src/templates/root/anyedge.toml.hbs`, doc `docs/manifest.md`, app-demo manifest `examples/app-demo/anyedge.toml`)
- [ ] Tooling parity: extend `anyedge-cli` with template/plugin style commands (similar to Spin templates) to streamline new app scaffolds and provider-specific wiring.

## Codex Plan (2024-05-07)
- [x] Review repository guides (`README.md`, `AGENTS.md`) to capture stated goals and workflows.
- [x] Map the crate layout by skimming `Cargo.toml` and key crate README/doc comments.
- [x] Summarize findings on AnyEdge architecture, adapters, and tooling for the user's quick reference.

### Familiarization Summary
- AnyEdge centres around `anyedge-core`, which provides provider-neutral HTTP primitives, routing, middleware, logging, and proxy abstractions; adapters reuse these types to stay DRY.
- Controller ergonomics live in `anyedge-controller` plus `anyedge-macros`, offering `#[action]` functions that extract typed inputs and return `Responder`s.
- Provider adapters (`anyedge-adapter-fastly`, `anyedge-adapter-cloudflare`) are feature-gated; each exposes `handle` plus logging/proxy helpers while delegating behaviour to the core crate.
- Supporting crates include `anyedge-std` for stdout logging, `anyedge-cli` for dev server + scaffolding, and demo workspaces under `examples/app-demo` to validate provider flows.
- Workspace `Cargo.toml` keeps default members lean (core only) to support offline builds; additional crates are opt-in via features when targeting specific adapters.

## Open Design Questions (for later pickup)
- Provider priorities: focus on Fastly Compute@Edge, then Cloudflare Workers.
- Minimum Rust version (MSRV) target.
- Async story: keep core sync or introduce async features (Tokio) behind flags?
- Request/Response mapping rules (header casing, multi-value headers, binary bodies).
- Caching/edge-specific headers: how much to standardize (e.g., Surrogate-Control)?
- Dev UX: integrate a local hyper server behind a feature vs. keeping zero-deps TCP server.
- Packaging/deploy: preferred tooling (Fastly CLI/API; AWS SAM/CDK or native Lambda tooling).
- Config format: TOML/JSON/YAML; env overlays.
- License and contribution guidelines.

## Codex Plan (2025-09-18 - Validation Extractors)
- [x] Review existing parameter/body extractors to identify integration points for validator-backed wrappers.
- [x] Implement validated variants for path/query (and other relevant) extractors reusing `validator::Validate`.
- [x] Add unit tests covering success and failure cases for the new extractors.
- [x] Update public exports/docs to surface the new API and capture the change in this TODO review.

## Codex Plan (2025-09-18 - ValidateJson Alias)
- [x] Confirm existing JSON validation support and identify changes needed for `ValidateJson` ergonomics.
- [x] Introduce a `ValidateJson` alias (or equivalent helper) that reuses `ValidatedJson<T>`.
- [x] Update docs/tests to reference the new alias where appropriate and log the change in this TODO review.

## Review (2025-09-18 02:10 UTC)
- Recorded a three-step familiarization plan, executed documentation + crate walkthrough, and captured architectural notes under the plan section.
- Assumed existing docs (`README.md`, `AGENTS.md`) are the latest sources of truth; no discrepancies observed during review.
- No outstanding issues or errors encountered; no code changes outside TODO.md updates.

## Review (2025-09-18 02:15 UTC)
- Introduced `ValidatedPath` and `ValidatedQuery` extractors leveraging `validator::Validate`, re-exported them, and broadened docs in `README.md`.
- Added targeted unit tests verifying both success and failure cases for validated path/query extraction; `cargo test -p anyedge-controller` passes.
- No additional assumptions beyond existing validation behaviour; no unresolved issues noted.

## Codex Plan (2025-09-18 - Remove ValidateJson Alias)
- [x] Remove the `ValidateJson` wrapper from extractors/tests/exports, keeping `ValidatedJson` as the canonical helper.
- [x] Revert docs and TODO review notes referencing `ValidateJson`.
- [x] Re-run controller crate tests to confirm everything still passes.

## Review (2025-09-18 02:21 UTC)
- Removed the redundant `ValidateJson<T>` wrapper per updated guidance, keeping `ValidatedJson<T>` as the single validator-backed JSON extractor.
- Cleaned README/TODO references accordingly and pruned alias-specific tests; `cargo test -p anyedge-controller` still passes.
- Validator helpers now cover path, query, and JSON via `Validated*` types only.

## Review (2025-09-18 03:08 UTC)
- Implemented `anyedge build|deploy --adapter fastly` by wiring cargo wasm32 builds and Fastly CLI invocation in the CLI.
- Documented optional `dev-example` dependency in `anyedge-cli/README.md` and added error handling for unsupported adapters.
- Verified builds with `cargo test -p anyedge-cli`.

## Review (2025-09-18 03:27 UTC)
- Moved Fastly build/deploy/serve helpers into `anyedge-fastly` behind a `cli` feature and updated `anyedge-cli` to call through the provider abstraction.
- Improved Fastly manifest discovery to prefer the nearest crate manifest and added unit coverage for the new path logic. `cargo test -p anyedge-fastly --features cli`, `cargo test -p anyedge-cli`.

## Codex Plan (2025-09-19 - Test Command Guidance)
- [x] Confirm current workspace layout to know which `cargo` commands exercise all crates.
- [x] Advise user on running full test suite and targeted variants as needed.

## Review (2025-09-19 15:25 UTC)
- Noted workspace members/default-members to explain why `cargo test` covers only core by default.
- Documented commands for running all workspace tests plus targeted crates and feature gates.
- No code changes required; guidance only.

## Codex Plan (2025-09-19 - Fix anyedge-fastly Tests)
- [x] Reproduce `cargo test -p anyedge-fastly` failure to capture the exact error. *(Unable to reproduce; command succeeds locally.)*
- [x] Inspect relevant sources/tests to identify root cause. *(Fastly feature requires `wasm32-wasip1` target; host builds fall back to stub with no tests.)*
- [ ] Implement minimal fix addressing the failure while staying within adapter boundaries. *(Blocked: `cargo test --features fastly --target wasm32-wasip1` currently fails under Viceroy with macOS keychain access errors.)*
- [ ] Re-run targeted tests (and any impacted suite) to confirm resolution. *(Blocked.)*
- [ ] Summarize work and outcomes in TODO review section with timestamp. *(Blocked.)*

## Codex Plan (2025-09-19 - Comprehensive Test Runner)
- [x] Audit required commands for workspace + feature coverage including Fastly wasm build.
- [x] Implement shell script to orchestrate the checks with Fastly-specific handling.
- [x] Document script usage and outcomes in TODO review section with timestamp.

## Review (2025-09-19 15:55 UTC)
- Added `scripts/run_tests.sh` (initially `run_all_checks.sh`) to bundle fmt, clippy, and workspace tests while excluding the wasm-only Fastly demo during host runs.
- Script verifies `wasm32-wasip1` availability and builds both the Fastly adapter (`anyedge-fastly` with `fastly` feature) and demo binary for the wasm target to cover edge-only code paths.
- Provides a single entry point for comprehensive validation; Fastly tests remain blocked on native hosts, so wasm builds act as the smoke check.

## Codex Plan (2025-09-19 - Test Script Adjustments)
- [x] Define the reduced test-only scope for `scripts/run_tests.sh`, including Fastly wasm coverage.
- [x] Update the script to drop fmt/clippy/build steps and run the desired test commands (host + wasm).
- [x] Ensure script messaging guides users when Fastly wasm tests cannot run (e.g., Viceroy keychain issues).
- [x] Record the changes and remaining caveats in TODO review section with timestamp.

## Review (2025-09-19 16:05 UTC)
- Trimmed `scripts/run_tests.sh` to a test-only flow: workspace tests (excluding `app-demo-adapter-fastly`), Fastly CLI tests, and wasm32 `fastly` feature tests run from the adapter crate.
- Added failure guidance when Viceroy cannot access macOS keychain certificates; users can set `SSL_CERT_FILE` or run where a keychain is available before retrying.
- Command still surfaces the Fastly wasm failure in this sandbox (certificate issue), but other suites pass; no additional code changes required.

## Codex Plan (2025-09-19 - Test Script UX)
- [x] Decide on section markers/format for clearer script output.
- [x] Update `scripts/run_tests.sh` to print section headers around each major test group.
- [x] Verify the script still exits with helpful guidance on wasm failures.
- [x] Log the changes in TODO review section with timestamp.

## Review (2025-09-19 16:15 UTC)
- Added bold section separators around workspace, Fastly CLI, and wasm test phases in `scripts/run_tests.sh` for easier scanning.
- Confirmed the wasm phase still surfaces the keychain guidance when Viceroy fails; other sections remain unaffected.
- No code changes beyond the script; tests behave as before (wasm step still blocked in sandbox environment).

## Codex Plan (2025-09-19 - Simplify Wasm Failure Messaging)
- [x] Remove custom failure messaging in `scripts/run_tests.sh` so wasm tests fail like other sections.
- [x] Verify script behaviour post-change.
- [x] Update TODO review section with outcome and timestamp.

## Review (2025-09-19 16:25 UTC)
- Dropped the custom Viceroy guidance in `scripts/run_tests.sh`; now all phases use `run` and rely on `set -e` for failures.
- Confirmed workspace and Fastly CLI sections still pass while wasm tests fail with the raw keychain error (expected in this sandbox).
- No additional adjustments required.

## Codex Plan (2025-09-19 - Script Preflight Checks)
- [x] Consolidate binary/target verification at the top of `scripts/run_tests.sh`.
- [x] Ensure checks cover both `cargo`, `rustup`, and wasm target before any tests run.
- [x] Validate script after refactor and document changes in TODO review.

## Review (2025-09-19 16:32 UTC)
- Preflight now checks for `cargo`, `rustup`, and the `wasm32-wasip1` target before any test phases run, ensuring early exits on missing tooling.
- Post-change run confirms behaviour: host tests pass, Fastly wasm still fails due to Viceroy keychain access in this sandbox—expected.
- No further adjustments needed.

## Codex Plan (2025-09-19 - Controller Streaming Options)
- [x] Inspect controller `RouteSet`/`RouteSpec` to identify extension points for route-level body mode.
- [x] Implement API changes allowing controllers to opt into streaming/buffered modes and wire through to core router.
- [x] Add tests covering streaming behaviour via controller routes.
- [x] Document changes in TODO review section with timestamp.

## Review (2025-09-19 16:45 UTC)
- Added per-route body mode to controller `RouteSpec`/`RouteEntry`, enabling callers to supply `RouteOptions` (e.g. streaming/buffered) that are preserved when applying routes to the core app.
- Introduced `RouteSet::push_entry` helper so nested/merged route sets keep their body configuration.
- Extended controller tests to cover streaming coercion and buffered rejection paths via the new API; `cargo test -p anyedge-controller` passes.

## Review (2025-09-19 01:28 UTC)
- Rebuilt `app-demo-core` against the redesigned `anyedge-core` by wiring routes through `RouterService::builder`, swapping controller extractors for `anyedge_core` equivalents, and preserving the streaming example via `Body::stream`.
- Updated Fastly and Cloudflare entrypoints to rely on the adapter `dispatch` helpers, returning converted fallback responses when adapter conversion fails so we avoid propagating runtime errors.
- Tests: `cargo test -p app-demo-core`, `cargo check -p app-demo-adapter-fastly`; Cloudflare host build (`cargo check -p app-demo-adapter-cloudflare`) still fails in `worker` 0.0.24 because `js_sys::Iterator` lacks `unwrap` on this toolchain—matches the prior dev warning and needs upstream crate updates for host checks.
- Assumptions: routing defaults (no auto OPTIONS) remain acceptable for the demo; Cloudflare fallback uses `Response::error` to surface adapter failures until `worker` exposes richer constructors.

## Review (2025-09-19 01:47 UTC)
- Removed the `worker_logger` dependency (source of the legacy `worker 0.0.24` transitive pull) and replaced it with a minimal `console_log!`-backed logger inside `anyedge-adapter-cloudflare`, so host builds only compile the current `worker 0.6` tree.
- Updated the Fastly/Cloudflare demos to call the adapter crates directly (correct crate ids) and gated the Cloudflare fetch handler behind `target_arch = "wasm32"` so x86 test builds don’t require Worker-only APIs.
- Docs now reference the new logging flow, and `cargo test` across the workspace succeeds after the change.

## Codex Plan (2025-09-19 - App Demo Migration to Redesigned Core)
- [x] Audit the app-demo crates to map controller-era APIs to the redesigned `anyedge-core` interfaces.
- [x] Refactor `app-demo-core` handlers and routing to the new builder/extractor patterns, keeping streaming coverage intact.
- [x] Update Fastly/Cloudflare entrypoints (and related metadata) so they compile against the redesigned core hooks.
- [x] Run targeted tests for the app-demo crates and prepare review notes for TODO.md.

## Codex Plan (2025-09-19 - Restore `cargo test` Host Compilation)
- [x] Pinpoint the dependency pulling in `worker 0.0.24` and decide whether to remove or upgrade it so builds only use `worker 0.6.x`.
- [x] Adjust the Cloudflare adapter to drop the outdated logger dependency (or replace it) while keeping logging behaviour sensible.
- [x] Ensure workspace manifests stop requesting the older crate and update docs if they reference it.
- [x] Re-run `cargo test` (or equivalent host checks) to confirm the build completes, then capture the outcome in TODO review notes.

## Codex Plan (2025-09-19 - Fastly Adapter Logger Module)
- [x] Add a dedicated `logger.rs` to the Fastly adapter mirroring the Cloudflare structure.
- [x] Re-export the new logger helper from `lib.rs` and update call sites if needed.
- [x] Ensure any docs or templates referencing Fastly logging stay accurate.
- [x] Run `cargo test` to confirm the refactor behaves as before.

## Review (2025-09-19 01:50 UTC)
- Split Fastly logging into a `logger.rs` module (mirroring Cloudflare) and re-exported it from the adapter, keeping `log_fastly` usage behind the wasm+feature gate.
- No call-site changes were required—existing demos already target `anyedge_adapter_fastly::init_logger()`—but the shared structure makes the adapters consistent.
- `cargo test` still passes; the `log_fastly` manifest warning persists (pre-existing).

## Codex Plan (2025-09-19 - Code Coverage Investigation)
- [x] Survey current test command usage to see if any coverage tooling is documented or scripted.
- [x] Evaluate Rust-friendly coverage options (e.g. `cargo llvm-cov`, `grcov`) for compatibility with the workspace (wasm targets, feature flags).
- [x] Prototype a coverage run on a representative crate (likely `anyedge-core`) to gauge feasibility and capture required tooling steps.
- [x] Summarize recommended approach, prerequisites, and open questions in TODO review notes.

## Review (2025-09-19 01:56 UTC)
- Confirmed coverage isn’t wired into existing scripts; `scripts/run_tests.sh` strictly runs host + wasm tests without instrumentation.
- Verified `cargo-llvm-cov` is available via `asdf` and works against host crates: `cargo llvm-cov --package anyedge-core --lcov --output-path target/coverage/anyedge-core.lcov` produced an LCOV file and summary (~52% line coverage for `anyedge-core`, `proc` modules currently untested).
- Suggested path forward: document `cargo llvm-cov` usage (including creating `target/coverage/`), limit runs to host-capable crates, and treat wasm-only crates as out-of-scope unless a wasm coverage workflow is introduced.
- Next steps could include adding a helper script and/or CI job if coverage reporting becomes a target metric.

## Codex Plan (2025-09-19 - Core Coverage Backfill)
- [x] Expand unit tests for `anyedge_core::context::RequestContext` to cover extraction success/failure, JSON/form parsing, and path/query edge cases.
- [x] Add tests for `anyedge_core::error::EdgeError` constructors and conversion paths, ensuring status codes + messages are asserted.
- [x] Exercise middleware chaining (`middleware.rs`) with mock handlers to cover both middleware execution and early returns.
- [x] Write targeted tests for `Body` conversions (streaming JSON failure, `into_bytes` panic guard, etc.) and `Response` helper methods.
- [x] Introduce extractor/responder tests to lift coverage in `extractor.rs` and `responder.rs`.
- [x] Add compile-fail or snapshot tests for the `anyedge-macros::action` proc macro to cover unsupported inputs.
- [x] Run `cargo llvm-cov --package anyedge-core` (and any new test suites) to validate coverage improvements.

## Review (2025-09-19 02:08 UTC)
- Added focused unit tests across `anyedge-core` for request contexts (path/query/json/form), error helpers, middleware chaining, bodies/responses, extractors, and responders; mirrored coverage for the `#[action]` macro via direct `expand_action_impl` assertions.
- Reworked Clockflare/Fastly-specific comparisons where necessary (stringy path params, async middleware helpers) so the new tests reflect real behaviour.
- `cargo test` passes, and `cargo llvm-cov --package anyedge-core --summary-only` now reports ~69% line coverage (up from ~52%); extractor/macro modules remain partially uncovered but key runtime surfaces are exercised.

## Codex Plan (2025-09-19 - App Demo Workspace Dependencies)
- [x] Switch app-demo crates to consume workspace-shared dependencies (`{ workspace = true }`) for core libraries and tooling.
- [x] Ensure root `Cargo.toml` workspace dependencies cover the required crates (adding any missing entries).
- [x] Update adapter/demo crate manifests to drop redundant version/specification lines in favour of workspace references.
- [x] Run `cargo test` (or targeted checks) to confirm builds remain healthy.

## Review (2025-09-19 02:12 UTC)
- Added workspace dependency bindings for `anyedge-core`, adapters, demos, `fastly`, and `worker` in the root manifest, then pointed the app-demo crates at them so they share versions with the framework.
- Demo manifests now rely on `{ workspace = true }` for `anyedge-adapter-*`, `app-demo-core`, `log`, `serde`, `fastly`, and `worker`, removing bespoke paths/versions.
- `cargo test` passes across the workspace after the manifest refactors.

## Codex Plan (2025-09-19 - Fastly SDK 0.11 Migration)
- [ ] Update the Fastly adapter to compile against `fastly` 0.11 APIs (request building, async streaming, response conversion).
- [ ] Adjust logging helper to the new log-fastly builder API.
- [ ] Ensure proxy tests/builds pass for streaming + compression paths.
- [ ] Verify the app demos compile for `wasm32-wasip1` with the updated SDK.

## Review (2025-09-19 02:35 UTC)
- Temporary stopgap: adapter builds against Fastly 0.11 by buffering request/response bodies and wiring a new logging helper; wasm demo (`cargo build -p app-demo-adapter-fastly --target wasm32-wasip1`) and `cargo test` now succeed.
- Regression: streaming proxy behaviour (and streaming decompression) is currently disabled because bodies are buffered; follow-up work is required to restore async streaming under the new SDK.

## Review (2025-09-19 07:35 UTC)
- Updated `anyedge_adapter_fastly::dispatch` to surface `fastly::Error` directly; demo entrypoints can now return `dispatch(&app, req)` without hand-rolling fallback responses.
- Header and request conversions match the Fastly 0.11 API, `cargo test` and `cargo build -p app-demo-adapter-fastly --target wasm32-wasip1` remain green.

## Codex Plan (2025-09-20 - Wasm Runner Docs)
- [x] Identify the best documentation spot (likely `README.md`) for Wasmtime/Viceroy installation instructions.
- [x] Draft concise install steps for Wasmtime and Viceroy, ensuring commands work on macOS/Linux.
- [x] Update the chosen doc section with the new prerequisites and cross-check formatting/links.

## Codex Plan (2025-09-20 - Fastly Wasm Test Investigation)
- [x] Reproduce the failing Fastly wasm tests via `cargo test --features fastly --target wasm32-wasip1` in `crates/anyedge-adapter-fastly`.
- [x] Inspect recent Fastly runtime changes to pinpoint incompatibilities (target env, required runner, feature flags).
- [x] Identify minimal fixes or configuration adjustments to get wasm tests passing again (no implementation yet).
- [x] Summarize findings and proposed solution in response; note blockers if unresolved.

## Review (2025-09-20 21:30 UTC)
- Added Wasmtime/Viceroy installation guidance plus runner instructions to `README.md` so wasm tests list their runtime prerequisites explicitly.
- Reproduced the Fastly wasm test failure: Viceroy runner aborts with “No keychain is available” while loading native certs, preventing tests from executing.
- Proposed using Wasmtime as the `wasm32-wasip1` test runner (via env override or `.cargo/config` adjustment) or configuring Viceroy to skip the system trust store; left implementation for follow-up.
- Logged the failing `cargo test` output in `debug.md` for future reference; no code changes were made to the adapter runtime itself.

## Codex Plan (2025-09-20 - Async Compression Adapters)
- [x] Add `async-compression` dependency (using workspace versioning) and wire it into Fastly/Cloudflare adapter manifests.
- [x] Refactor Fastly adapter response/body decoding to use `GzipDecoder`/`BrotliDecoder` from `async-compression`, removing custom `flate2` state machines.
- [x] Mirror the same decompression helper in the Cloudflare adapter so both adapters share behaviour.
- [x] Run targeted tests (`cargo test --features fastly`, `cargo test --features cloudflare`) to confirm gzip/brotli decoding paths.
- [x] Update docs (if needed) to reflect the new dependency or behaviour.

## Review (2025-09-20 22:05 UTC)
- Added `async-compression` as a workspace dependency and enabled it in both adapter manifests to centralise gzip/brotli handling.
- Replaced the bespoke `flate2`/`brotli` state machines in Fastly and Cloudflare adapters with shared `GzipDecoder`/`BrotliDecoder` pipelines over `StreamReader`.
- Host-side adapter tests pass (`cargo test -p anyedge-adapter-fastly`, `cargo test -p anyedge-adapter-cloudflare`); wasm targets still require platform runners but behaviour is unchanged.
- Documentation already covered gzip/brotli support, so no content updates were necessary beyond noting the new dependency in review.

## Codex Plan (2025-09-20 - Core Compression Helpers)
- [x] Introduce a compression helper module in `anyedge-core` that exposes shared gzip/brotli decoding for `TryStream<Result<Vec<u8>>>` inputs.
- [x] Update Fastly adapter to use the new core helper instead of its local `decode_*` implementations.
- [x] Update Cloudflare adapter to use the same helper, removing duplicate code and imports.
- [x] Add unit tests in `anyedge-core` covering gzip/brotli decoding via the helper.
- [x] Re-run adapter/core tests to confirm everything compiles and passes.

## Review (2025-09-20 22:45 UTC)
- Added `compression.rs` to `anyedge-core` exposing shared `decode_gzip_stream`/`decode_brotli_stream` helpers over `async-compression`, plus unit tests that confirm round-tripping gzip and brotli blocks.
- Fastly and Cloudflare adapters now call the core helpers, removing duplicated decoder loops and dropping their direct `async-compression` dependency.
- Workspace dependencies updated (`async-stream` shared, `futures` promoted for core) and host tests pass (`cargo test -p anyedge-core`, `cargo test -p anyedge-adapter-fastly`, `cargo test -p anyedge-adapter-cloudflare`).

## Codex Plan (2025-09-20 - Cloudflare Wasm MapErr Fix)
- [x] Restore the `TryStreamExt` import in the Cloudflare adapter so `map_err` resolves for wasm builds.
- [x] Run `cargo test --features cloudflare --target wasm32-wasip1 -- --nocapture` to confirm the wasm build clears (runner failures aside).

## Review (2025-09-20 22:58 UTC)
- Re-added `futures_util::TryStreamExt` to the Cloudflare adapter so `ByteStream::map_err` compiles for wasm targets.
- `cargo test --features cloudflare --target wasm32-wasip1 -- --nocapture` now builds the wasm binary; execution still fails under Wasmtime because Workers headers rely on wasm-bindgen imports unavailable in this runtime, which matches prior expectations.

## Codex Plan (2025-09-20 - Cloudflare Demo Build Fix)
- [x] Add the missing `main` entrypoint in `examples/app-demo/crates/app-demo-adapter-cloudflare/src/main.rs` (call the adapter handler).
- [x] Bring `anyedge_core::response::IntoResponse` into scope or adjust error handling so `EdgeError` converts correctly.
- [x] Drop unused `Write` import in the Cloudflare adapter after the refactor.
- [x] Re-run `cargo build -p app-demo-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown` to confirm the demo compiles.

## Review (2025-09-20 23:10 UTC)
- Added `#![cfg_attr(target_arch = "wasm32", no_main)]` and imported `anyedge_core::response::IntoResponse` so the Cloudflare demo binary compiles to wasm without missing entrypoint errors; error handling now converts `EdgeError` via `into_response`.
- Cleaned up the Cloudflare adapter’s stale `Write` import and set the demo crate to depend on `anyedge-core` explicitly.
- `cargo build -p app-demo-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown` completes successfully (only standard warnings remain).

## Codex Plan (2025-09-20 - Cloudflare Entry Simplification)
- [x] Update Cloudflare adapter `dispatch` to return `worker::Result` so callers can propagate errors directly.
- [x] Simplify the demo entrypoint to mirror the Fastly example (build app, init logger, call `dispatch`).
- [x] Rebuild the Cloudflare demo for wasm to confirm no regressions.

## Review (2025-09-20 23:25 UTC)
- `anyedge_adapter_cloudflare::dispatch` now returns `worker::Result<Response>` after converting internal `EdgeError`s to `worker::Error`, eliminating the need for a separate helper.
- The Cloudflare demo entrypoint matches the Fastly style: build the app, init the logger, and await `dispatch`.
- `cargo build -p app-demo-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown` stays green (standard brotli warning only).
- Updated CLI templates so newly scaffolded apps use `anyedge_adapter_fastly::dispatch` / `anyedge_adapter_cloudflare::dispatch` with logger init, matching the demos.
