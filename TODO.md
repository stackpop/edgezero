# AnyEdge TODO

High-level backlog and decisions to drive the next milestones.

## Queue (near-term)
- [x] CLI: `anyedge build --provider fastly` to package a Fastly Compute@Edge artifact
- [x] CLI: `anyedge deploy --provider fastly` using Fastly CLI/API
- [ ] Fastly demo: add optional backend example to validate `req.send("backend")`
- [ ] Core: `Response::json<T: serde::Serialize>` behind `serde` feature
- [x] Core: helper to fetch all header values for a name (multi-value support)
- [ ] Core: utility to serve embedded static files via `include_str!`/`include_bytes!` with proper Content-Type and HEAD (no body) handling
- [ ] Docs: document header/method/status mapping to `http` crate
- [x] Docs: clean stale references to removed hello example from README/targets
- [ ] Docs: add RouteOptions + streaming policy section (examples done; expand README section)
- [x] Docs: highlight controller-based workflow and update examples
- [x] Controllers: add `anyedge-controller` + `anyedge-macros` with typed extractors and `#[action]` helpers
- [ ] CI: add `fmt`, `clippy`, and `test` workflows
- [ ] Cloudflare demo: add minimal `wrangler` example and verify `wrangler dev`
- [ ] Cloudflare streaming: map `Response::with_chunks` to `ReadableStream` with backpressure
- [ ] Core proxy: add async fetch facade (feature `async-client`) and implement Cloudflare proxy
- [ ] Fastly proxy: add tests for backend send + header/body mapping

## Test Coverage Plan (2025-09-18)
- [x] Router: add regression cases for overlapping routes, prefix/nest behaviour, and BodyMode error handling (`Streaming` vs `Buffered`).
- [x] Controller: cover negative paths (`State<T>` missing, `ValidatedJson` errors) and assert returned status/body.
- [ ] Adapters: introduce Fastly/Cloudflare mapping tests (headers, streaming, proxy failure) to catch glue regressions.
- [ ] CLI: add integration tests for `anyedge new` scaffolding, feature-flag builds, and `dev` fallback app.
- [ ] CI: verify feature combinations (without `dev-example`, `json`, `form`) compile and run basic smoke tests.

## Milestones
- [ ] Fastly adapter MVP
  - [x] Map Fastly Request -> `anyedge-core::Request`
  - [x] Map `anyedge-core::Response` -> Fastly Response (headers/body)
  - [x] Handle binary/text bodies and content-length
  - [x] Example: runnable Fastly demo (`examples/app-demo/crates/app-demo-fastly`) with wasm32-wasip1 target and explicit logger init
  - [x] Streaming: native streaming on Fastly via `stream_to_client` (wasm32-wasip1), buffered fallback elsewhere
- [ ] Cloudflare Workers adapter MVP
  - [x] Basic mapping: Workers Request -> AnyEdge Request
  - [x] Basic mapping: AnyEdge Response -> Workers Response (buffered bodies)
  - [ ] Streaming behavior (ReadableStream) and backpressure
  - [ ] Example: deploy sample with `wrangler` (`examples/app-demo/crates/app-demo-cloudflare`)
- [ ] CLI
  - [x] `anyedge new <name>`: scaffold an app (lib with `build_app()`)
  - [ ] `anyedge build --provider fastly|cloudflare`
  - [ ] `anyedge deploy --provider fastly|cloudflare`
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

## Codex Plan (2024-05-07)
- [x] Review repository guides (`README.md`, `AGENTS.md`) to capture stated goals and workflows.
- [x] Map the crate layout by skimming `Cargo.toml` and key crate README/doc comments.
- [x] Summarize findings on AnyEdge architecture, adapters, and tooling for the user's quick reference.

### Familiarization Summary
- AnyEdge centres around `anyedge-core`, which provides provider-neutral HTTP primitives, routing, middleware, logging, and proxy abstractions; adapters reuse these types to stay DRY.
- Controller ergonomics live in `anyedge-controller` plus `anyedge-macros`, offering `#[action]` functions that extract typed inputs and return `Responder`s.
- Provider adapters (`anyedge-fastly`, `anyedge-cloudflare`) are feature-gated; each exposes `handle` plus logging/proxy helpers while delegating behaviour to the core crate.
- Supporting crates include `anyedge-std` for stdout logging, `anyedge-cli` for dev server + scaffolding, and demo workspaces under `examples/app-demo` to validate provider flows.
- Workspace `Cargo.toml` keeps default members lean (core only) to support offline builds; additional crates are opt-in via features when targeting specific providers.

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
- Implemented `anyedge build|deploy --provider fastly` by wiring cargo wasm32 builds and Fastly CLI invocation in the CLI.
- Documented optional `dev-example` dependency in `anyedge-cli/README.md` and added error handling for unsupported providers.
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
- Trimmed `scripts/run_tests.sh` to a test-only flow: workspace tests (excluding `app-demo-fastly`), Fastly CLI tests, and wasm32 `fastly` feature tests run from the adapter crate.
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
