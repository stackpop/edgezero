# Pre-dispatch interception and inbound request fidelity implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use @superpowers:subagent-driven-development or @superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add generic method-independent interception before router dispatch and preserve/report inbound request information honestly across all four adapters.

**Architecture:** One optional Arc-backed non-Send pre-dispatch hook on the existing RouterService. Adapter-owned bounded RequestIngress metadata preserves runtime-visible target/origin information and declares header fidelity without duplicating sensitive headers. Narrow converter fixes and actual local runtime fixtures prove each supported claim.

**Tech Stack:** Rust 1.95.0, edition 2024, existing async-trait/futures/http facade, Fastly 0.12.1, worker 0.8.3, Axum/Hyper, spin-sdk 6.0.0, Viceroy 0.17.0, wasm-bindgen-test, Wasmtime and VitePress.

**Source:** [Corresponding spec](../specs/2026-10-05-pre-dispatch-request-fidelity-design.md).
**Baseline:** `98930917d96cb665c7255f36ca8bbd61fc7539e1`.
**Branch:** `spec/pre-dispatch-request-fidelity`.
**Status:** Implementation authorized after independent review. Toolkit work is implemented; verification and external consumer gates are recorded below.

---

## Execution boundary and dependency order

The user authorized implementation after independent review with no material findings. Work remains on this EdgeZero branch. The downstream approved spec is unchanged; no push, publication or release approval is implied.

Read AGENTS.md and CLAUDE.md before executing. Keep the feature generic. Do not add trace namespace checks, cookie names, report schemas, authentication policy or consumer finalization into EdgeZero. New handlers use `#[action]` and HTTP aliases come from `edgezero_core::http`. Preserve non-Send futures and avoid new runtime dependencies in core. Apply strict alphabetical/lint/public-documentation conventions at changed sites only.

Order: P0 capability decisions → P1 ingress contract and P2 hook (independent core work) → P3 Cloudflare / P4 Axum / P5 Spin (independent implementations with central integration) → P6 Fastly after the SDK prerequisite → P7 real ingress fixtures → P8 CI/docs/release evidence.

Pure core/Cloudflare/Axum/Spin work can proceed while the Fastly SDK getter is unavailable. That is progress on infrastructure, not completed downstream F0. Record every environment prerequisite explicitly; zero selected tests, host-only WASM checks, parser rejection and unavailable runtimes are not passing ingress evidence.

Default no-hook behavior must remain identical. Cloudflare method/header conversion fixes are intentional correctness changes even without a hook: update existing tests that codify information loss and document the behavior change.

## File map

| Action                                  | Exact path                                                                                                                                                                                                                 | Responsibility                                                                        |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| Modify                                  | `crates/edgezero-core/src/router.rs`                                                                                                                                                                                       | Hook trait/alias, optional builder/inner field, dispatch ordering and colocated tests |
| Create                                  | `crates/edgezero-core/src/request.rs`                                                                                                                                                                                      | Bounded RequestIngress types/constructors/accessors and colocated tests               |
| Modify                                  | `crates/edgezero-core/src/lib.rs`                                                                                                                                                                                          | Export request module                                                                 |
| Modify                                  | `crates/edgezero-adapter-cloudflare/src/request.rs`                                                                                                                                                                        | Runtime method/URL capture and Worker UTF-8 header conversion                         |
| Modify                                  | `crates/edgezero-adapter-cloudflare/tests/contract.rs`                                                                                                                                                                     | Executed browser converter regressions                                                |
| Modify if required                      | `crates/edgezero-adapter-cloudflare/Cargo.toml`                                                                                                                                                                            | Explicit test Web RequestInit features                                                |
| Modify                                  | `crates/edgezero-adapter-axum/src/request.rs`, `crates/edgezero-adapter-axum/src/service.rs`, `crates/edgezero-adapter-axum/src/dev_server.rs`                                                                             | Runtime parts metadata, trusted transport binding, real native parser/hook tests      |
| Modify                                  | `crates/edgezero-adapter-spin/src/request.rs`, `crates/edgezero-adapter-spin/tests/contract.rs`                                                                                                                            | Runtime parts metadata and converter/dispatch regression seams                        |
| Modify                                  | `crates/edgezero-adapter-fastly/src/request.rs`, `crates/edgezero-adapter-fastly/src/lib.rs`, `crates/edgezero-adapter-fastly/tests/contract.rs`                                                                           | Early safe capture, pre-captured metadata path, runners and regression coverage       |
| Modify only after approved SDK revision | `Cargo.toml`, `Cargo.lock`                                                                                                                                                                                                 | Reviewed dependency pin; no imaginary API or version                                  |
| Create                                  | `tests/fixtures/request-fidelity/Cargo.toml`, `tests/fixtures/request-fidelity/Cargo.lock`, `tests/fixtures/request-fidelity/src/lib.rs`                                                                                   | Isolated test-only fixture workspace and shared store-free router                     |
| Create                                  | `tests/fixtures/request-fidelity/src/bin/fastly.rs`, `tests/fixtures/request-fidelity/src/spin.rs`, `tests/fixtures/request-fidelity/src/cloudflare.rs`                                                                    | Actual runtime entry points and generic early-shortcut fixture                        |
| Create                                  | `tests/fixtures/request-fidelity/fastly.toml`, `tests/fixtures/request-fidelity/spin.toml`, `tests/fixtures/request-fidelity/wrangler.toml`                                                                                | Loopback-only local runtime configuration                                             |
| Create                                  | `tests/fixtures/request-fidelity/package.json`, `tests/fixtures/request-fidelity/package-lock.json`                                                                                                                        | Exact verified Wrangler/tooling pins and scripts                                      |
| Create                                  | `scripts/test-request-fidelity.sh`, `tests/fixtures/request-fidelity-runner/{Cargo.toml,Cargo.lock,src/main.rs}`                                                                                                           | Runtime startup/build/readiness/cleanup and raw socket probes                         |
| Modify                                  | `.github/workflows/test.yml`                                                                                                                                                                                               | Executed WASM unit/contract and local runtime transport gates                         |
| Modify                                  | `docs/guide/routing.md`, `docs/guide/middleware.md`, `docs/guide/adapters/overview.md`, `docs/guide/adapters/fastly.md`, `docs/guide/adapters/cloudflare.md`, `docs/guide/adapters/axum.md`, `docs/guide/adapters/spin.md` | Hook API, ordering limits, platform matrix and consumer handoff                       |

The fixture is a test-only separate workspace (`[workspace]` in its manifest); do not add it to production workspace members or inherit root workspace dependencies incorrectly. Use explicit local path dependencies and optional platform features; each build selects only its runtime. Any needed fixture-path exclusions/package publication guards must be explicit. Root all-feature clippy must not compile a multi-runtime fixture accidentally.

## P0: Establish capability decisions before promising downstream compliance

**Files:** source investigation only, spec capability matrix; dependency files only after an approved pin exists.

- [x] Reconfirm clean implementation baseline, `git rev-parse HEAD`, `git status --short --branch` and `cargo metadata --format-version 1 --no-deps`. Re-read consumer section 8's explicit pre-buffering allowance.
- [x] Verify pinned SDK/runtime sources from Cargo.lock. Record worker 0.8.3 versus the consumer's 0.8.5, wasm-bindgen 0.2.122, Fastly 0.12.1, Hyper 1.10.1 and Spin SDK 6.0.0. Do not rely on floating package documentation.
- [x] For Fastly, investigate a safe borrowed raw-runtime-URI accessor. Existing get_url/get_path normalize; into_handles/from_handles loses client provenance, and clone_without_body forces lazy URL parsing. Rule out those workarounds. A proposed accessor name is not an existing SDK API.
- [ ] Prepare the external SDK change requirements: bounded native URI acquisition before Url parsing; no request/body consumption or cloning; no second from_client acquisition; client IP/TLS/original-header provenance retained; errors bounded; no fabricated original-wire promise. Obtain a reviewed immutable compatible revision/version before P6's complete-target implementation.
- [x] Record per-field runtime limitations, including Cloudflare Headers combining and Hyper identical Content-Length folding. Decide what the generic metadata can truthfully expose.
- [x] Separate adapter conversion guarantees from original wire guarantees. Where consumer requirements are impossible with the runtime, record an explicit downstream design-resolution item; do not edit or approve that spec from this branch.
- [x] Select the recommended hook plus metadata design over middleware-only or per-adapter policy wrappers; record scope/CI impact. External SDK or contract gates cannot be replaced by synthetic fixture success.
- [x] Checkpoint: approved upstream design before code; unresolved external SDK work may remain pending while independent tasks proceed.

## P1: Define bounded adapter-owned RequestIngress

**Files:** create `crates/edgezero-core/src/request.rs`; modify `crates/edgezero-core/src/lib.rs`.

- [x] Add red tests named `ingress_target_bound_is_atomic`, `ingress_fidelity_defaults_are_unknown`, `ingress_header_override_is_field_specific` and `ingress_origin_rejects_untrusted_or_invalid_input`.
- [x] Run `cargo test -p edgezero-core ingress`. Expected red for missing types/behavior, not an unrelated compile/setup failure; make the initial test harness compile before judging behavioral red.
- [x] Implement the spec's closed enums/types. Exact minimum API: `RequestIngress::new(target, origin, common_header_fidelity, overrides)`, `target()`, `origin()` and `header_fidelity(&HeaderName)`. The CompleteTarget payload is public with private fields and borrowed getters; only CapturedTarget::capture constructs it. Add a compile-fail doctest proving external callers cannot bypass its bound with a struct literal. Use an owned small override list/map and reject duplicate override keys at construction rather than ambiguous selection. No copied header values.
- [x] Add `CapturedTarget::capture(value, source, fidelity)` applying the 16 KiB complete-or-unavailable bound. Add `InboundOrigin::parse(scheme, authority, source)` with a 1 KiB authority bound, HTTP(S)-only scheme and no user-info/path/query/fragment. Include the spec’s OriginSource enum/borrowed source accessor. Parsing validates input; adapter callers must still prove its trusted source. Origin-form requests without transport binding remain unavailable.
- [x] Define header-fidelity axes against original received field values/multiplicity/order, excluding HTTP framing/name spelling. Runtime-to-core copying alone cannot set Preserved. Tests/docs separately report converter-copy guarantees versus pinned parser/runtime/transport evidence, including trimming/coalescing loss.
- [x] Define private fields/borrowed getters and Clone-only value-bearing metadata. Use existing Arc/HTTP facade types. No Serialize or secret-bearing Debug/Display/logging.
- [x] Test exact cap and one byte over, non-ASCII UTF-8 byte counting, read-failure/unavailable states, same-name versus global order, Content-Length override, missing extension and IPv6/default-port authority. Never store a prefix as a complete target.
- [x] Run `cargo test -p edgezero-core ingress`, `cargo test -p edgezero-core` and scoped clippy. Expected green; all tests selected.
- [ ] Planned commit after checks: `Add bounded inbound request metadata and fidelity contracts`.

## P2: Add the optional pre-dispatch hook to the concrete router

**Files:** `crates/edgezero-core/src/router.rs`.

- [x] Add behavioral tests for terminal response on unknown route and `EXAMPLE-METHOD`; continuation that changes method/URI; hook error; no-hook equivalence; absent router state/introspection in hook; state-clone count; cloned-router shared hook; streamed-body inspection; and Rc retained across await.
- [x] Run `cargo test -p edgezero-core pre_dispatch`. Expected meaningful red for route-first ordering, then implement the trait/alias and builder method from spec section 5.
- [x] Add one optional Arc field to builder/inner, thread it through build/new and invoke it before method/path copies. Document repeated registration replacing the existing hook.
- [x] Test mutations of header/extensions/body in addition to method/URI. Continued dispatch uses the remaining mutated body exactly; the hook cannot consume a stream and silently restore it.
- [x] Distinguish error APIs: direct `Service::call` propagates EdgeError; existing `RouterService::oneshot` renders it through IntoResponse. Both stop before normal dispatch. A consumer requiring custom challenge/hardening returns `Some(Response)` with those exact headers.
- [x] Test a terminal custom status/body/Allow/WWW-Authenticate is preserved by core. Document that outer adapter/application response finalizers may still operate.
- [x] Verify every existing core router/introspection/state/middleware test, plus manual `Hooks::routes` build compatibility. Keep macros/App/runner return types unchanged.
- [x] Run `cargo test -p edgezero-core` and `cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings`.
- [ ] Planned commit: `Add method-independent router pre-dispatch interception`.

## P3: Correct Cloudflare runtime method and header conversion

**Files:** `crates/edgezero-adapter-cloudflare/src/request.rs`, `crates/edgezero-adapter-cloudflare/tests/contract.rs`, optional test feature changes in its Cargo.toml.

- [x] Add raw Web Request fixtures using `web_sys::RequestInit::set_method`, `Request::new_with_str_and_init` and `CfRequest::from(raw)`. Worker enum-only RequestInit cannot create the regression. Cover uppercase and lowercase extension tokens and existing supported methods.
- [x] Add runtime URL metadata equality tests independently of parsed core URI equality. Use dot/encoded-dot/encoded-separator/repeated-slash/query cases, comparing what Web Request actually exposes rather than requiring it to preserve the input string.
- [x] Add browser converter tests for ASCII/high-byte runtime strings. Assert UTF-8 string copying; browser ByteString semantics are not Worker wire evidence. The incorrect helper-only lib test is removed; actual workerd tests cover valid C3 A9/C4 80 and invalid FF transformation.
- [x] Run the browser contract command in Shared verification. Red/green covers extension-token preservation and the corrected UTF-8 runtime-string convention.
- [x] Replace inbound method enum conversion with `CoreMethod::from_bytes(req.inner().method().as_bytes())` and bounded bad-request errors. Capture `req.inner().url()` before additional parsing and insert RequestIngress after core request construction.
- [x] Populate InboundOrigin from the runtime URL’s HTTP(S) scheme and validated authority with RuntimeUri provenance, after target capture. Never use Origin or forwarding headers for this fact. Add invalid/non-HTTP scheme, user-info/authority bounds and spoofed Origin/forwarding-header tests; missing or untrustworthy facts stay unavailable. Runtime URL parsing for origin must not replace the captured target.
- [x] Construct HeaderValue with `HeaderValue::from_bytes(value.as_bytes())`, following pinned workerd's UTF-8 header convention. Reject invalid runtime HeaderValues with bounded errors; preserve append behavior and validated names. Do not single-byte-cast Unicode scalars, silently omit fields or comma-split combined values.
- [x] Verify exact real Worker wire C3 A9 and C4 80 preservation, and distinguish invalid FF replacement from original-byte availability. Update the baseline only after semantic assertions pass.
- [x] Locate all `into_core_method` uses. Replace/update the old unknown-method→GET test; remove the helper only if unused, retaining supported-method tests through actual inbound conversion.
- [x] Keep buffering, context/proxy/store paths unchanged. Add hook-through-CloudflareService coverage, then run contract tests plus target clippy/check.
- [ ] Planned commit: `Preserve Cloudflare runtime methods and header octets`.

## P4: Preserve Axum parts and declare parser limitations

**Files:** Axum `src/request.rs`, `src/service.rs`, `src/dev_server.rs`.

- [x] Add `ingress` converter tests for method/URI/bytes/repeated same-name values/extensions, origin-form and absolute-form targets, and missing trusted origin.
- [x] Run `cargo test -p edgezero-adapter-axum --features axum ingress`. Expected red for absent metadata. Add metadata around existing parts/from_parts; retain JSON buffering and non-JSON streaming.
- [x] If the standard dev server supplies an HTTP transport binding, insert only that adapter-owned scheme fact before conversion and combine it with exactly one validated inbound authority. Reject contradictory URI/Host origin facts as unavailable metadata; never guess HTTPS from forwarded headers. Generic converter callers without binding get unavailable origin.
- [x] Declare global wire order unavailable. Add a Content-Length-specific multiplicity limitation grounded in the pinned parser; preserve known same-name runtime values without claiming all original controls survived.
- [x] Extend service tests to show early hook response, continuation, extension method and router error behavior. Capability registries constructed before the hook are not ordinary consumer KV operations.
- [x] Add raw TCP cases to the existing dev-server listener harness: dot spellings, Cookie/Origin/control duplicates, identical/conflicting lengths, non-UTF-8 bytes and TE/CL combinations. Assert whether Hyper rejects or the hook sees the request.
- [x] Run `cargo test -p edgezero-adapter-axum --features axum` and native target clippy.
- [ ] Planned commit: `Expose Axum ingress fidelity without changing body behavior`.

## P5: Capture Spin runtime parts and test the actual seam

**Files:** Spin `src/request.rs` and `tests/contract.rs`.

- [x] Add parts-conversion helper tests, or actual runtime tests where SDK incoming-body construction permits it, for runtime method/URI/header metadata.
- [x] Run host tests for host-covered helpers and the WASM contract/lib commands for runtime code. Existing core-request-only contract success is not converter coverage.
- [x] Capture runtime parts before rebuilding core Request. Preserve byte HeaderValues/method and source URI scheme/authority. Do not grant trust to spin-full-url/spin-client-addr solely because those headers exist.
- [x] Preserve existing buffering and store registry setup. Document earlier capability setup separately from ordinary app lifecycle work.
- [x] Verify extension `Method::Other` conversion, same-name values, unavailable global wire order, missing origin/facts, and metadata through actual dispatch.
- [x] Use P7's Spin invocation to establish URI/header behavior and spoofed special-header outcomes. If a synthetic shared-parts helper was used, explicitly identify which claims still need runtime evidence.
- [x] Run affected host tests and wasm32-wasip2 check/clippy/contract. Full WASM lib execution remains unavailable because plain Wasmtime lacks Spin-specific imports; actual Spin HTTP fixture covers the converter instead.
- [ ] Planned commit: `Expose Spin runtime ingress provenance and conversion tests`.

## P6: Add safe Fastly capture and pre-captured metadata integration

**Files:** Fastly `src/request.rs`, `src/lib.rs`, `tests/contract.rs`; dependency files only after P0's reviewed SDK pin.

- [x] Checkpoint the external SDK accessor. If unavailable, implement honest unavailable/runtime-normalized metadata and finish byte-path regression work, but leave original-target/client-provenance acceptance pending.
- [ ] Add red target-cap/capture-failure/client-provenance tests before changing converters. Execution: bounded supplied snapshots, unavailable SDK target and actual HTTP client preservation are tested; raw acquisition failure and TLS coverage await the SDK accessor. Synthetic `FastlyRequest::new` already parses URLs and does not establish raw incoming preservation.
- [ ] Implement `capture_request_ingress(&FastlyRequest) -> RequestIngress` using the reviewed safe borrowed accessor before any URL accessor. It cannot consume/clone/reacquire the request. Bounded capture errors create unavailable metadata without raw error strings.
- [x] Add `into_core_request_with_ingress(req, ingress)`; existing `into_core_request(req)` captures once and delegates. A consumer that captures immediately after from_client can carry that exact snapshot through later conversion, avoiding recapture of mutated native state.
- [x] Add public `dispatch_with_registries_and_ingress(app, req, stores, env, ingress, extend)` with the existing return type/generic callback. The existing `dispatch_with_registries` captures and delegates; preserve per-id registries, EnvConfig/default-key selectors and required-store errors, rather than replacing it with legacy bare-handle defaults. Thread captured facts through FastlyService and registry/request-extensions dispatch with one internal conversion/store-wiring path. Add `FastlyService::with_request_ingress` only if required by that dispatch API. Ordinary runners capture before their first native URL access/callback; custom early-entry callers use the explicit snapshot path.
- [x] Ensure application scratch extension merging cannot accidentally replace adapter-captured ingress facts; define adapter snapshot insertion after generic scratch merge. Test collision, same snapshot retention and unchanged device/client extensions. Do not alter normal service/store selection.
- [x] Preserve existing byte-oriented header append path. Add all-values/high-byte/same-name-order tests, explicitly excluding unproved global wire order/original-name reconstruction.
- [x] Run Fastly contract and lib WASM/Viceroy suites, no-feature/CLI-feature and WASM clippy. Real ingress and native-shortcut evidence is recorded in P7.
- [ ] Planned commit: `Add safe early Fastly ingress capture and snapshot handoff`.

## P7: Prove actual local transport behavior

**Files:** proposed test fixture workspace/runtime configs and `scripts/test-request-fidelity.sh` and `tests/fixtures/request-fidelity-runner/`.

- [x] Build a store-free generic fixture router with one early hook and an ordinary route/middleware counter. Test namespace is `/reserved`, not Trusted Server policy. New handlers use #[action].
- [x] Expose only synthetic fixture verdicts: method equality, captured-target equality/availability, expected header-byte equality, per-field counts/fidelity and ordinary-handler-called. Use fixed fictional fixtures; do not build a reusable unrestricted header/cookie/URI echo endpoint.
- [x] Add Fastly entry fixture that captures before a generic native shortcut and carries the snapshot into conversion. Test reserved dot spellings remain visible when the SDK/runtime supports them. Raw target unavailable is an explicit unsupported verdict, not a passed bypass test.
- [x] Add Cloudflare Worker fixture selecting the cloudflare feature and matching worker-build/Wrangler tooling. Pin Wrangler as an exact npm dev dependency with package-lock. Resolve/pin worker-build as a Rust CLI compatible with the fixture’s locked worker version, installed with cargo --version/--locked; npm locking does not pin it. Use lock-matched wasm-bindgen tooling. Verify exact compatible tool versions at implementation; do not copy floating ^4.38.0 or invent a release.
- [x] Add a feature-gated #[http_service] entry in src/spin.rs, included by the fixture library, targeting wasm32-wasip2 as a cdylib HTTP component; do not build Spin as a main/bin WASI command. For package name request-fidelity, spin.toml references target/spin/wasm32-wasip2/release/request_fidelity.wasm, built with cargo build --manifest-path tests/fixtures/request-fidelity/Cargo.toml --target-dir tests/fixtures/request-fidelity/target/spin --target wasm32-wasip2 --features spin --lib --release --locked. Resolve that path relative to spin.toml. Fastly selects its required-feature binary; Cloudflare selects its feature-gated library via worker-build. Reuse existing runtime build conventions; new fixture dependency pins/lockfile must be reproducible.
- [x] The shell runner builds the host Rust helper and accepts `fastly`, `cloudflare`, `spin` or `all`, allocates an available loopback port, builds the selected fixture, starts the runtime, waits with a bounded readiness deadline, runs raw socket probes, and always stops its process group on failure/signal/success. No platform credentials, remote services or publisher requests.
- [x] Native Rust socket client replaces the original Python test helper and sends exact HTTP/1.1 bytes with Host example.com. Include extension methods, origin/absolute forms where supported, literal/encoded dot paths, repeated/encoded separators, non-UTF-8 Cookie octets, interleaved/case-varied repeated fields, duplicate controls and identical/conflicting length cases.
- [x] Record transport rejection versus runtime transformation versus EdgeZero conversion versus hook response. A browser Fetch/reqwest-normalized input cannot replace a raw request probe. Local Wrangler/workerd/Viceroy/Spin results cannot prove all production-edge behavior.
- [x] Test the Rust replacement’s cleanup after failed readiness/probe/build, parallel port isolation and no lingering process/artifact mutation of baseline demo config.
- [x] Run the Rust-backed `./scripts/test-request-fidelity.sh fastly`, `cloudflare` and `spin` with the required tools installed. Each supported verdict is asserted; unresolved guarantees fail the compliance claim or remain a specifically listed pending gate, not a blanket skip/pass.
- [ ] Planned commit after checks: `Add local runtime probes for inbound request fidelity`.

## P8: Connect CI, document capabilities and hand off the reviewed revision

**Files:** .github/workflows/test.yml, guide paths in file map, dependency/fixture locks if changed.

- [x] Cloudflare converter regressions execute in the browser contract target; the incorrect scalar helper and its helper-only lib gate are removed. Spin adjustment: host helper tests, WASM contract/check/lint and actual Spin HTTP fixture cover the converter; its full WASM lib artifact requires unavailable Spin host interfaces and is not reported as passing. Fastly already has contract/lib execution; host workspace tests do not run gated WASM conversion code.
- [x] Provision browser/WebDriver for Cloudflare run_in_browser tests, matching wasm-bindgen-cli from Cargo.lock. Reuse current version-resolution workflow rather than independently pinning a drifting runner.
- [x] Add local fixture transport jobs with exact installed tool versions/locks and deterministic cleanup. Spin CLI availability must be explicit; Wasmtime contract execution alone is not Spin HTTP ingress.
- [x] Document core API example, hook result/error/state/finalizer semantics, buffering limitation, origin trust rules, per-field fidelity and every runtime limitation. Separate capability availability from consumer approval.
- [x] Build the final platform evidence matrix with source pin, exact fixture, observed outcome and status for each requirement. Record local versus production verification limits.
- [x] Run all Shared verification gates and confirm tests selected the new modules. Scope red/green at tasks; full gates at implementation handoff.
- [x] Independent reviewers check spec/plan/implementation alignment, correctness, minimum scope, supported target coverage and unresolved gates. No upstream/downstream compliance approval by inference.
- [ ] Handoff an immutable reviewed EdgeZero commit/tag only after release approval. Record it for Trusted Server to pin all EdgeZero dependencies consistently. Downstream changes stay on its existing branch and require its own acceptance checks.
- [ ] Planned commit: `Document and verify pre-dispatch ingress contracts across adapters`.

## Shared verification

Focused commands run from repository root with the relevant features/targets. Require nonzero selected tests and a passing intended red/green cycle. New code must not be committed before tests required by CLAUDE.md pass.

```bash
cargo test -p edgezero-core ingress
cargo test -p edgezero-core pre_dispatch
cargo test -p edgezero-core
cargo test -p edgezero-adapter-axum --features axum
cargo test -p edgezero-adapter-spin
CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --test contract
CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --lib
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --test contract
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --lib
CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y,http=y" cargo test -p edgezero-adapter-spin --features spin --target wasm32-wasip2 --test contract
# Full Spin WASM lib needs Spin-specific host imports; use host helper tests
# plus the actual Spin HTTP fixture instead of claiming plain Wasmtime execution.
```

Fastly feature tests require the WASM/Viceroy runner; a host feature test may fail at linking and cannot substitute. Cloudflare contract uses a real headless browser/WebDriver and the lock-resolved wasm-bindgen-cli (currently 0.2.122). Existing matrix provisions Viceroy and Wasmtime; new local runtime fixtures may require additional CLI/tool installation.

Full implementation handoff gates:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p edgezero-adapter-fastly --features cli --all-targets -- -D warnings
cargo clippy -p edgezero-adapter-fastly --no-default-features --lib -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1
cargo check -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown
cargo check -p edgezero-adapter-spin --features spin --target wasm32-wasip2
cargo clippy -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --all-targets -- -D warnings
cargo clippy -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo clippy -p edgezero-adapter-spin --features spin --target wasm32-wasip2 --all-targets -- -D warnings
cargo doc -p edgezero-core --no-deps --all-features
cargo test --locked --manifest-path examples/app-demo/Cargo.toml --workspace --all-targets
cargo fmt --manifest-path examples/app-demo/Cargo.toml --all -- --check
cargo clippy --manifest-path examples/app-demo/Cargo.toml --workspace --all-targets --all-features -- -D warnings
./scripts/test-request-fidelity.sh all
npm --prefix docs run lint
npm --prefix docs run format
npm --prefix docs run build
git diff --check
```

Expected: exit 0 for all available required environments, unchanged baseline behavior except documented Cloudflare conversion corrections, and no unexecuted suite reported as a pass. No runtime/CI pass is claimed by this planning document.

## Spec-to-task acceptance map

| Spec requirement                                                     | Tasks          | Evidence                                                                    |
| -------------------------------------------------------------------- | -------------- | --------------------------------------------------------------------------- |
| Generic hook and no-hook compatibility                               | P2             | Ordering, error/response/continuation/state/clone/non-Send regressions      |
| Bounded confidential metadata and origin provenance                  | P1/P3–P6       | Target caps, borrowed getters, missing facts, no logged/serialized values   |
| Correct runtime method/header conversion                             | P3–P6          | Actual converters with extension tokens and high bytes                      |
| Fastly safe early capture and consumer shortcut seam                 | P0/P6/P7       | Reviewed SDK accessor, client provenance, actual Viceroy wire paths         |
| Honest multiplicity/normalization limits                             | P0/P1/P4/P7/P8 | Hyper/Fetch/runtime rejection/transformation matrix                         |
| Buffered-body and outer-finalizer limits preserved                   | P2–P6/P8       | Existing body/store/response contracts and accurate docs                    |
| All-adapter runtime evidence and target gates                        | P7/P8          | Native parser, WASM contract/lib and local runtime fixture results          |
| Downstream F0 readiness distinguished from infrastructure completion | P0/P8          | Explicit immutable pin and separately approved consumer-contract resolution |

## Independent document review

Three independent read-only reviewers assessed both complete documents against main 98930917 and the approved consumer requirements. They proposed minimal approaches during investigation, reviewed the selected hook/metadata design, and reread the corrected documents. This document review completed before the user authorized implementation.

| Scope                                                                                            | Spec result                                  | Plan result                                     |
| ------------------------------------------------------------------------------------------------ | -------------------------------------------- | ----------------------------------------------- |
| Core router/API, Axum/Spin conversion, body/bootstrap limits and execution feasibility           | Approved                                     | Approved after fixture/origin corrections       |
| Cloudflare method/ByteString/provenance, bounded public API and browser/runtime tooling          | Approved after bounded-payload correction    | Approved after matching API/tooling corrections |
| Fastly native ownership, SDK dependency, snapshot/registry integration and Viceroy wire evidence | Approved after header-fidelity clarification | Approved after matching clarification           |

Corrections incorporated: private bounded CompleteTarget payload and compile-fail boundary test; explicit origin provenance/Cloudflare origin construction; Spin HTTP-component library rather than command binary; distinct npm/Cargo tool locks; original-field versus runtime-copy fidelity semantics; and named metadata-aware registry dispatch preserving EnvConfig/store-selector behavior.

The safe borrowed Fastly SDK accessor remains external work, and original-wire properties require runtime evidence or separately approved consumer-contract resolution. The implementation evidence below is separate from these initial document reviews; neither review nor green infrastructure checks imply completion of Trusted Server F0.

## Executor handoff

Use this instruction with EdgeZero's Codex after maintainer approval:

> Work in EdgeZero on branch spec/pre-dispatch-request-fidelity. Read AGENTS.md and CLAUDE.md, then the corresponding spec and this plan. Implement P0–P8 using the approved API and task verification. Keep RouterService concrete and non-Send-compatible; keep the hook generic. Preserve runtime-visible facts without inventing raw wire guarantees. Resolve the external safe Fastly SDK accessor before claiming original-target support. Verify actual method/header/target conversion and run local wire fixtures. Treat missing runtime facts or test environments as explicit pending gates, never success. Do not change Trusted Server's spec, implement trace policy, patch Cargo caches, push or publish without separate authorization. Report focused/full checks, the final capability matrix, and the immutable dependency revision ready for consumer review.

Only the spec and this matching plan are planning artifacts for this feature; independent review results are recorded here after corrections.

## Implementation and verification record

The fresh pre-implementation review used three independent scopes: core/Axum/Spin; Cloudflare/provenance; Fastly/SDK/native ownership. All three approved both the spec and plan before code work. Subsequent independent implementation reviews found no production correctness defects. Coverage and lint findings were corrected centrally: IPv6/ASCII-port validation, matched-route lifecycle testing, scratch/client extension assertions, transport-specific Content-Length fidelity, executable Spin helper placement, and process-group cleanup races. Final review also corrected pinned-browser selection in CI, exact FF cookie equality and mandatory runtime-observation comparison.

### Implemented surfaces

- Core: bounded confidential ingress API, validated origin provenance, field-specific fidelity overrides and sealed target payload; optional shared non-Send hook before method/path/state/introspection/context/middleware.
- Cloudflare: runtime method tokens, checked UTF-8 runtime HeaderValues, runtime URL/origin capture and real browser converter/service regressions.
- Axum: runtime URI/HeaderMap retention, private HTTP binding, consistent Host origin, Content-Length parser limitation and exact TCP converter/service probes.
- Spin: runtime-parts snapshot and byte/method conversion retained; host-executed URI metadata tests and actual SDK6 HTTP component fixture.
- Fastly: safe unavailable-target capture, exact supplied-snapshot retention, registry-aware dispatch, service builder and capture-before-callback runner path; no unsafe handles, URL-cloning workaround or SDK dependency substitution.
- Verification: isolated locked multi-runtime fixtures, fixed verdicts, pinned observations, bounded loopback readiness and process-group cleanup for builds and runtimes; browser/fixture CI gates and public API/capability documentation.

### Evidence and limits

| Surface             | Executed evidence                                                                                                                                           | Outcome                                                                                          |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Core                | 484 unit tests, integration test and compile-fail bounded-payload doctest                                                                                   | Pass; existing ignored docs unchanged                                                            |
| Native workspace    | Workspace/all-target test run                                                                                                                               | 1,440 passed, 0 failed, 1 existing ignored; 20 suites                                            |
| Axum                | 115 full tests including the cap regression                                                                                                                 | Pass; raw TCP includes duplicates, FF, dots, identical/conflicting CL, TE/CL, continuation/error |
| Cloudflare          | 11 browser contract tests including non-HTTP origin; explicit Chrome binary capabilities; removed incorrect scalar helper/lib-only test                     | Pass; converter and service hooks execute                                                        |
| Fastly              | Viceroy 0.17.0 contract 6 and WASM library 91                                                                                                               | Pass; byte/header/snapshot/collision/client/proxy regressions execute                            |
| Spin                | Host 31 and Wasmtime 44.0.1 contract 12 with async/P3/HTTP flags                                                                                            | Pass; host contract's zero selected tests do not count as runtime evidence                       |
| CLI/audits/scaffold | Fastly CLI-feature 287; nested AppConfig checker 18; placeholder/legacy/nested audits; generated project build test                                         | Pass; generated native/WASM adapters compile                                                     |
| Excluded demo       | Locked workspace 33 tests, formatting and strict all-feature clippy                                                                                         | Pass; six host adapter targets select zero tests                                                 |
| Build/lint/docs     | Root/adapter/fixture formatting; workspace, adapter and all three fixture target clippy; feature checks, core rustdoc; docs ESLint/Prettier/VitePress build | Pass; three existing warnings in untouched rustdoc files remain                                  |

Spin plan adjustment: the new pure helper/tests live in `src/context.rs`, which already has a host-safe test gate. Plain Wasmtime cannot supply `spin:key-value/key-value@3.0.0` to the full WASM library test artifact. That artifact is not reported as passing. Host helper execution, existing WASM contracts with explicit flags, target lint/check and the actual Spin HTTP component establish the implemented seam without inventing a Spin-aware unit runner. CI retains this distinction.

Local fixture pins: Fastly 0.12.1/Viceroy 0.17.0; worker 0.8.3/wasm-bindgen 0.2.122/worker-build 0.8.3/Wrangler 4.83.0/Miniflare 4.20260415.0/workerd 1.20260415.1; Spin SDK 6.0.0/Spin 4.0.0; Rust 1.95.0/Node 24.12.0. Full fixed observations are recorded in `tests/fixtures/request-fidelity/observations.json`; they are regression evidence, not compliance approval. All 47 cases were executed with the native Rust client on the three pinned local runtimes. Failure probes covered a failed build, interrupted build with child process, forced readiness failure and forced probe failure; parallel Fastly runs used distinct ports and left no live owned processes. CI is configured with the locally verified commands; remote GitHub Actions execution remains unrun.

### Remaining upstream and consumer gates

1. Fastly's safe borrowed raw target accessor is absent. Capture reports NotExposed. Actual Viceroy ingress demonstrates `/reserved/../native` can reach a preceding normalized `/native` shortcut. The hook cannot repair earlier native routing. A reviewed SDK revision and fresh raw-target/client/TLS proof are still required.
2. Local workerd's fixed KJ method parser returns 501 for extension methods before Worker execution. Fetch also normalizes literal/encoded dot paths and coalesces Cookie/Origin/control fields. Fresh review corrected the adapter scalar-to-byte bug: valid UTF-8 now copies without casting. Raw FF already appears as U+FFFD in Web Headers before Rust and becomes runtime UTF-8 EF BF BD in core; it cannot be recovered as original FF. Common octet/multiplicity fidelity is Unknown, since the observed transforming path does not prove every particular field changed. Absolute-form and duplicate CL requests also return 500 without a hook verdict. Browser-created method/ByteString success does not establish original wire support.
3. Spin 4.0 requires UTF-8 while preparing incoming headers and returns 500 for FF before the component runs. This requires an upstream runtime fix. Valid runtime values continue through EdgeZero's byte path.
4. General original-field octets/multiplicity/order are not universally proved. Metadata keeps unknown axes conservative and global order unavailable; local fixtures do not prove production edge behavior. Consumer-required original paths/fields need an explicitly approved supported contract where runtime recovery is impossible.

Pinned primary source: [Spin header preparation](https://github.com/spinframework/spin/blob/v4.0.0/crates/trigger-http/src/headers.rs#L96-L106); [workerd KJ dependency](https://github.com/cloudflare/workerd/blob/v1.20260415.1/build/deps/gen/deps.MODULE.bazel#L27-L35); [KJ method rejection](https://github.com/capnproto/capnproto/blob/d0e9605b288ac4d8b86ee3201a23c43c017a6b61/c%2B%2B/src/kj/compat/http.c%2B%2B#L961-L999); [workerd target rewriting](https://github.com/cloudflare/workerd/blob/v1.20260415.1/src/workerd/server/server.c%2B%2B#L451-L471).

The toolkit implementation can be reviewed independently of these gates. Trusted Server F0 remains unresolved; this branch does not change or weaken its approved spec. Planned commits and immutable consumer dependency/release handoff remain pending; no commit, push or publication is performed by this execution record.

### Earlier independent implementation review

| Reviewer scope                                                                        | Final result                            |
| ------------------------------------------------------------------------------------- | --------------------------------------- |
| Core bounded API/router and Fastly native ownership/registry/snapshot paths           | No remaining findings                   |
| Cloudflare/Axum/Spin conversion, public API docs and adapter CI feasibility           | No remaining findings                   |
| Locked runtime fixtures, exact observations, process cleanup and CI browser selection | No remaining findings after corrections |

Final corrections were independently rereviewed. The exact FF cookie probe and mandatory full-profile comparison were rebuilt and executed across all three local runtimes, with strict fixture clippy passing on each target. The explicit Chrome binary capability configuration was executed through the real wasm-bindgen browser runner. No production behavior changed during this final review cycle. The results above do not close the listed external SDK/runtime, production provenance or downstream consumer gates.

### Subsequent skeptical review — supersedes the earlier readiness assessment

The user requested fresh independent reviewers without relying on the prior review conclusions. Architecture/core/adapters, consumer-contract alignment and runtime/CI/fixture scopes were reviewed independently. The optional router hook and bounded extension metadata remain the smallest appropriate architecture. No additional hook ordering, ownership, body/error propagation, process cleanup, pinning or CI-wiring defect was found. The Cloudflare correction and user-requested Rust tooling replacement are implemented and independently reviewed; the final executed checks below distinguish toolkit correctness from unresolved consumer requirements.

- [x] **P1 Cloudflare UTF-8 conversion:** The browser ByteString assumption is incorrect for pinned workerd. Actual wire C3 A9 produces core E9; valid C4 80 returns 500 before the hook. Root independently reproduced both with the existing real local Worker fixture. Correct conversion to preserve Worker UTF-8 runtime strings, add exact real-workerd assertions for both cases, revise browser-only expectations and update observations only after those assertions pass. Keep invalid UTF-8 replacement as a distinct upstream capability limitation. The corrected baseline is collected only after exact semantic assertions pass and is compared in normal runs. All 11 browser contracts, all 47 real runtime probes and strict target clippy pass. The invalid FF case exposes runtime replacement rather than inventing original bytes.
- [x] **Origin provenance wording:** Clarify TransportBinding as trusted transport scheme plus a single validated inbound authority. Host is request authority, not operator ownership/approval. No configured host allowlist is required by the consumer spec. Public core documentation and this spec now agree with the Axum implementation.
- [x] **Spin runner documentation:** Public test commands now include the same async/P3/HTTP flags as verified CI. Wasmtime contracts and actual Spin incoming conversion evidence remain distinct.
- [x] **Framing coverage:** Actual TE+CL and standalone empty chunked requests retain one Transfer-Encoding: chunked field on each tested local runtime. The consumer can inspect it and must still implement its rejection semantics.
- [ ] **Consumer semantic coverage:** Add exact repeated reserved-cookie and real action/Fetch Metadata control probes in Trusted Server's integration harness. Strict complete-value comparison may reject coalesced controls safely; do not comma-split or require every metadata fidelity axis to say Preserved without checking the actual feature requirement.

F0 is a consumer dependency/behavior gate, not a synonym for every header's perfect preservation. Global order across different header names is unnecessary. Cookie rules depend on exact reserved-name counts, bounds and non-UTF-8 detection, with order-independent outcomes. The path requirement concerns original pathname classification; unrelated query preservation is unnecessary. Avoid interpreting a 16 KiB whole-target metadata cap as proof that a short pathname was uninspectable.

| Resolution work                                                                    | Owner/location                                                                                 | Required proof                                                                                                                                               |
| ---------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Fix valid Worker UTF-8 conversion                                                  | EdgeZero Cloudflare converter/tests/fixture                                                    | C3 A9 and C4 80 retained through real Worker dispatch; no ordinary-request regression                                                                        |
| Recover canonical Fastly origin separately                                         | EdgeZero conversion or Trusted Server native-origin seam                                       | After raw capture attempt and before mutation, validate HTTP(S), one Host, URL authority agreement; preserve native provenance and unavailable target status |
| Safe borrowed original-target accessor                                             | Fastly Rust SDK                                                                                | Bounded host URI access before Url parsing; no consume/clone/reacquire; preserved client/TLS metadata and actual reserved-dot-path tests                     |
| Early native shortcut exclusion                                                    | Trusted Server Fastly entry point                                                              | Raw reserved spellings cannot reach health/JA4 shortcuts; configured authentication and trace response hardening remain first                                |
| Hidden Cloudflare raw paths and pre-Worker method rejection                        | Cloudflare/runtime, or explicitly approved consumer amendment                                  | Original reserved-path ambiguity and authenticated/hardened response guarantees, not only no cookie mutation                                                 |
| Invalid UTF-8 headers rejected before Spin invocation / replaced in Worker headers | Spin/Worker runtime, or explicitly approved consumer amendment                                 | Original cookie uninspectability is available to the consumer or the transport-boundary contract explicitly changes                                          |
| Coalesced Cookie semantics                                                         | EdgeZero/consumer exact transport tests, then runtime work or an explicitly approved amendment | Reserved-name counts, duplicate precedence, bounds and original non-UTF-8 observability; valid diagnostics cookie plus unrelated FF must never activate      |
| Dependency integration                                                             | Reviewed EdgeZero immutable commit/release and Trusted Server Cargo.toml/Cargo.lock            | Consistent immutable pin, target compilation and actual consumer integration tests                                                                           |

Preserving the approved consumer contract requires upstream capability work followed by real ingress verification. An alternative runtime-boundary contract must explicitly define normalized-path classification, transport-level rejection and unavailable-cookie reporting; it requires user approval because it changes the approved feature semantics. Disabling feature support on selected adapters also changes the approved all-four-adapter scope. This review does not silently choose either alternative.

Pinned primary source for the newly identified encoding assumption: [workerd UTF-8 header strings](https://github.com/cloudflare/workerd/blob/v1.20260415.1/src/workerd/api/headers.c%2B%2B#L276-L292). The reproduction report is `/tmp/edgezero-fresh-review-encoding.json`; only fixed statuses and boolean verdicts are recorded. Production conversion now preserves Worker UTF-8 strings; exact real Worker verification and the Python-to-Rust tooling replacement are complete.

### User language constraint

The user rejects shipping Python. Replace the test orchestration/client with an isolated native Rust harness and a small shell entry point; use Node for existing Worker browser capability JSON and shell extraction for the controlled Spin CLI pin. Native-only Tokio cancellation does not enter core or WASM adapters. Retain mandatory observations comparison, exact raw TCP bytes, pinned builds, bounded deadlines, failure/interruption cleanup and concurrent port isolation. Independently review the replacement before reporting completion.

### Final Rust replacement evidence

The Python probe file is removed and no Python snippets remain in the new CI or shell entry point. The native-only helper uses the exact existing Tokio 1.52.3, serde_json 1.0.150, tempfile 3.27.0 and TOML 1.1.2 pins in its own locked test workspace. Its eight tests cover exact request bytes, HTTP chunk/length/EOF framing, header/body bounds, semantic rejection of the UTF-8 regressions and owned process cancellation/reaping. Native formatting and strict clippy pass; CI checks this separate workspace explicitly.

A full locked-build run passes all 47 fixed raw TCP cases against the saved observations: Fastly 17, Cloudflare 15 and Spin 15. Valid UTF-8 C3 A9 and C4 80 remain exact on all three; both framing probes retain Transfer-Encoding: chunked. Workerd FF produces runtime replacement, explicitly unsupported as original preservation; Spin FF never reaches the component. Root also verified shell-bootstrap SIGTERM, Rust build SIGTERM, failed build descendants, failed readiness, failed baseline/probes and concurrent runs. Distinct concurrent ports close and no live owned descendants remain.

The independent architecture reviewer reproduced an initial shell-bootstrap cancellation leak. Job control now gives bootstrap Cargo its own group, with signal/exit cleanup and reaping before entering the Rust supervisor. Root verified the exact shim reproduction. Real execution also corrected Spin manifest parsing and worker-build's bare-version output. These corrections leave no extra runtime dependency in core or the adapters.

After the Cloudflare production change, workspace tests again pass 1,440 tests, workspace strict clippy and feature checks pass, and all 11 browser contracts pass. Subsequent reviewer rereads confirm the UTF-8/framing and CI-workspace checks are resolved. External SDK/runtime capability and Trusted Server integration/consumer acceptance gates above remain open. Remote GitHub Actions is configured but not executed locally; no commit or push is performed.

## Independent Claude audit follow-up

The external audit prompted three independent read-only scopes: runner/CI, adapters/provenance and consumer-spec alignment. Implement centrally after reproducing the concrete defects; retain the approved Trusted Server contract rather than accepting suggested runtime exceptions.

- [x] **Blocked-read cancellation:** The reviewer reproduced SIGTERM leaving the native runner spinning at 91.6% CPU. `read_until` and `read_exact` retry `Interrupted`, so socket cancellation now returns a terminal `Other` error. A bounded silent-server regression covers header and exact-body reads and failed before the fix. SIGHUP joins SIGINT/SIGTERM on the cancellation-and-await path; the shell bootstrap explicitly handles HUP. Independent post-fix probes cover all six signal/read combinations, exit within 297–308 ms and leave no runtime/descendant alive.
- [x] **Fastly canonical origin:** Record original-target `NotExposed` before any URL access, then recover origin from a native client runtime URI and exactly one validated Host with authority/default-port agreement. Validate the full runtime authority, including user-info and port syntax. Origin uses `RuntimeUri`, never a guessed HTTP scheme from absent TLS. Three regression tests cover matching, missing/duplicate/conflicting Host and invalid runtime authority; the invalid-authority test failed before the final correction. An actual Viceroy probe changes the native URL and Host after capture and proves explicit-snapshot dispatch retains original origin and client metadata. SDK URL parsing may normalize paths and can panic; public docs now state that behavior without claiming raw-path support.
- [x] **Honest fidelity and caller assertions:** Cloudflare common octet/multiplicity metadata is Unknown; a potentially transforming runtime does not prove each field changed. Public docs agree. Metadata constructors and Fastly snapshot overloads state caller responsibility for provenance/fidelity; the useful snapshot APIs are not hidden.
- [x] **Exact cookie evidence:** Add fixed comma/semicolon join verdicts and compound invalid-FF versus original-valid-EF-BF-BD probes. Actual Worker repeated Cookies are comma-joined, and both compound inputs produce identical verdicts. This proves ambiguity, not an implemented Trusted Server activation defect. A replacement-character fallback would reject some original valid UTF-8 and cannot recover original reserved-name counts or byte bounds; it remains an unapproved consumer decision.
- [x] **CI coverage:** Add locked feature/target-matched clippy for the isolated WASM fixture workspace; install clippy/rustfmt in both test jobs. Independent reread confirms coverage of the Fastly enabled binary/library and Cloudflare/Spin libraries. Remote Actions execution remains pending.
- [x] **Existing consumer plan:** Update F0/F4 against `RequestIngress` and explicit-snapshot conversion, require hardened terminal responses rather than generic hook errors, remove stale ByteString guidance and record dependency-graph re-verification. F5 no longer demands transport-level 413 for identical duplicate zero lengths. No approved consumer spec, runtime-boundary exception, replacement-character rule or platform exclusion is changed.

Final local evidence executes 54 exact wire cases: Fastly 20, Cloudflare 17 and Spin 17. Normal locked-build runs compare the complete checked-in baseline; Node uses 24.12.0. Fastly origin is available and consistent in every hook verdict, and the original-origin snapshot probe passes. The native harness has nine passing tests, the browser converter has eleven, and all 1,440 workspace tests pass. Strict workspace/isolated fixture lint and feature checks remain required. Original Fastly path access, Worker normalization/replacement/pre-invocation rejection and Spin invalid-UTF-8 rejection remain external consumer gates. No production deployment, commit, push or release is performed.

## Approved consumer amendment and dependency handoff

On 2026-10-05, the user accepted the independently reviewed runtime-boundary
amendment to the existing Trusted Server trace spec and single plan and
authorized implementation. This supersedes the historical original-path/
original-wire consumer gates and unapproved-fallback statements above.
EdgeZero retains honest capability metadata and does not reconstruct erased
information. Trusted Server owns visible-path classification and conservative
Cookie ambiguity suppression under its amended contract.

The pre-dispatch hook and converter implementation passed fresh required
workspace gates, adapter/browser/WASM contracts and 54 source-built local
runtime probes before the dependency handoff. Independent PR-readiness
review found one CI-shell assignment/export lint issue, corrected centrally;
the remaining two actionlint warnings reproduce against the baseline.

A reviewed immutable dependency revision, consumer repinning and tests on
Trusted Server's resolved graph remain required. Do not claim consumer F0 or
the full trace feature complete from toolkit checks. Follow the repository
PR-creator workflow, including its linked-issue requirement, for publication.

### Approved external-review corrections — 2026-10-06

- [x] Prove canonical Cloudflare URLs and Axum absent/single/repeated visible
      Content-Length fields do not claim per-request `Transformed` merely because
      the runtime can normalize or fold them; use `Unknown`.
- [x] Retain actual parser folding and conversion behavior; rerun real browser
      contract tests, the native TCP hook and the fixed raw observation matrix.
- [x] Document Fastly identical Content-Length folding and conflicting-length
      rejection. Verify the required full gates and independent review, then update
      the existing PR and Trusted Server consumer pin. No Python is introduced.

Verification: all five required workspace gates and docs lint/format/build pass.
Native Axum contract tests pass 116/0, real-browser Cloudflare 11/0, Fastly
Viceroy 6/0 and Spin Wasmtime 12/0. The rebuilt raw matrix retains all 54
committed observations (20 Fastly, 17 Cloudflare, 17 Spin). Independent review
approved the metadata correction without changing request conversion or claims
about original wire bytes.

## Latest independent review follow-up — 2026-10-06

The earlier 47-case records above describe earlier execution checkpoints. The
final baseline and dependency-handoff evidence contain 54 cases: Fastly 20,
Cloudflare 17 and Spin 17. Those final counts supersede earlier probe totals.

Clarify the Axum guide: common field multiplicity remains `Unknown`, including
the Content-Length override. `InboundOrigin::parse` validates URI authorities,
not DNS-label-only names. Commas, asterisks and semicolons are permitted
registered-name characters under [RFC 3986 section 3.2.2](https://www.rfc-editor.org/rfc/rfc3986.html#section-3.2.2),
and a trailing dot is valid. The proposed DNS-only rejection would narrow the
approved contract and reject valid authorities, so no parser change is made.
These clarifications do not change the consumer dependency API or runtime code.
