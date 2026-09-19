# Reusable Application Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkboxes for tracking; exceptions and evidence limits are recorded in the execution checkpoint.

**Goal:** Add explicit application retention for Fastly, Cloudflare, and Spin while preserving existing defaults and Axum's retained-router behavior.

**Architecture:** Keep ownership in the existing core `App`. Add a private, host-testable initialization helper behind Fastly's new `Serve` conveniences and explicit-metadata dispatch wrappers for Cloudflare/Spin. Concrete application caches and provider HTTP fixtures demonstrate retention without introducing a common scheduler or generic singleton.

**Tech Stack:** Rust, Fastly SDK 0.12.1/Viceroy, worker 0.8/worker-build/Wrangler, Spin SDK 6/P3, Axum, shell, VitePress/Prettier.

---

## Authority and constraints

- Source baseline: `593fc9282a1c56e12bae15f91eef2162f4b6a1b7`.
- Governing design: [Reusable application lifecycle spec](../specs/2026-09-15-reusable-app-lifecycle-design.md), including the host/WASM test split and attempted-request accounting added during review.
- This document plans implementation. Before executing code changes, obtain plan approval as required by `CLAUDE.md`. Planning does not authorize deployment, publishing, external comments, or committing unrelated work.
- Preserve existing entry points, generated templates, macro grammar, logging ownership, error translation, buffering, stream caps, and defaults. No new Tokio dependency in core/WASM adapters, no provider-independent scheduler, and no external consumer identifiers in documents or fixtures.
- Re-read current `CLAUDE.md` and compare source with the baseline before execution. Use `superpowers:using-git-worktrees` when starting approved code work. This planning-only document remains in the current workspace.
- Run scoped `cargo test` after each code increment. Provider fixture changes also need the real-target check/build. No host stub proves provider behavior.
- The commit messages below are implementation checkpoints, not commands to commit during planning. Commit only focused, verified changes within the approved execution scope.

## Delivery structure

| Stage                           | Tasks | Exit evidence                                         |
| ------------------------------- | ----- | ----------------------------------------------------- |
| Core contracts and adapter APIs | 1–4   | Host lifecycle tests and provider type checks         |
| Provider fixtures and harness   | 5–9   | Real HTTP assertions and explicit unsupported results |
| Runtime evidence                | 10    | A/B/C accounting, performance, and isolation outcomes |
| Guides and final verification   | 11–12 | Checked examples, repository gates, review            |

Tasks 3 and 4 can run independently after Task 2. Provider fixture work can run independently once Task 5 fixes the common contract. Assign each file to one worker; do not concurrently edit Fastly `lib.rs`.

## File map

Production and existing tests:

- `crates/edgezero-core/src/app.rs`: retained-object bounds test.
- `crates/edgezero-core/src/router.rs`: forced interleaving/isolation test.
- `crates/edgezero-core/src/app_config.rs`: audit recursion-guard cleanup and extend colocated tests only where coverage is missing.
- `crates/edgezero-adapter-fastly/src/request.rs`: audit bounded warning-cache behavior; preserve production behavior.
- `crates/edgezero-adapter-fastly/src/lib.rs`: private retained state, colocated host tests, two public serving helpers, SDK exports.
- `crates/edgezero-adapter-cloudflare/src/lib.rs`: additive `dispatch_app`.
- `crates/edgezero-adapter-spin/src/lib.rs`: additive `dispatch_app`.
- `.github/workflows/test.yml`: native harness tests and Fastly HTTP smoke using existing Viceroy setup.
- `scripts/smoke_test_reusable_app.sh`: explicit build/tool/run orchestration.

New standalone fixture workspace:

```text
tests/fixtures/reusable-app/
  Cargo.toml
  Cargo.lock
  .gitignore
  README.md
  crates/
    fixture-harness/{Cargo.toml,src/{main,evidence,net,runners}.rs}
    fixture-core/{Cargo.toml,src/lib.rs}
    fixture-fastly/
      Cargo.toml
      fastly.toml
      src/lib.rs
      src/bin/single_request.rs
      src/bin/rebuild_per_request.rs
      src/bin/retained_app.rs
      src/bin/custom_single_request.rs
      src/bin/custom_rebuild_per_request.rs
      src/bin/custom_retained_app.rs
      src/bin/logger_negative.rs
    fixture-cloudflare/{Cargo.toml,wrangler.toml,src/lib.rs}
    fixture-spin/{Cargo.toml,spin.toml,runtime-config.toml,src/lib.rs}
    fixture-axum/{Cargo.toml,src/main.rs}
```

Documentation: `docs/guide/adapters/{overview,fastly,cloudflare,spin,axum}.md`, fixture README, and the fresh-Fastly-instance comment in `examples/app-demo/crates/app-demo-core/src/lib.rs`.

No production request/response module changes are expected. Reuse their dispatchers; add no core lifecycle module or manifest setting.

## Task 1: Protect retained ownership and request separation

**Files:** core `src/app.rs`, `src/router.rs`, and, where needed, `src/app_config.rs` test modules. Audit Fastly `src/request.rs` warning caches; record findings in the fixture README from Task 11.

- [x] **1.1 Read the existing tests** near `router.rs:865` and injection at `:251`. The current two-future test does not force both handlers to remain pending; extend coverage rather than copying it.
- [x] **1.2 Add the bounds assertion** to the app test module:

```rust
#[test]
fn app_can_be_retained_in_a_shared_owner() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<App>();
}
```

- [x] **1.3 Run `cargo test -p edgezero-core app_can_be_retained_in_a_shared_owner`.** Expected: pass against existing code. This protects an existing property; no production edit is justified.
- [x] **1.4 Add `retained_router_separates_overlapping_requests`.** Use test middleware with two `futures::channel::oneshot` receivers. Take a receiver under a mutex, release the guard, then await it. Poll both router futures and assert both are pending before releasing either sender.
- [x] **1.5 Give each request distinct** `/probe/{id}` path values, body, `x-request-token`, and a request-only extension. Also inject a conflicting value of the same type as registered app state; assert existing app-state precedence. Keep a separate `Arc<AtomicUsize>` counter as intentionally shared state.
- [x] **1.6 Add a `#[action]` probe** that reads its request-local values after the barrier and returns them plus shared state. Assert two distinct correct outputs and shared count two. Never put request tokens in app state.
- [x] **1.6a Audit persistent process/thread state.** Search the core and Fastly runtime modules for `static`, `OnceLock`, and `thread_local!`. Classify each value as intentionally retained, bounded suppression/cache state, or request-scoped state needing cleanup. Inspect `SecretFieldsRecursionGuard` in `crates/edgezero-core/src/app_config.rs:96–145` and the warning caches in `crates/edgezero-adapter-fastly/src/request.rs:563–586`. Do not infer leakage merely from a static or guard.
- [x] **1.6b Verify supported cleanup paths.** Reuse existing tests where sufficient; otherwise add colocated guard tests for normal scope exit and host panic unwinding, followed by a fresh successful invocation proving depth reset. Do not claim host unwinding proves cleanup after a terminating WASM trap; that case requires the fresh-instance fixture in Task 7.8. Confirm warning caches remain bounded and document that eviction permits repeated warnings, while suppression means warning counts are not failure counts. Run scoped tests after any test-code increment.
- [x] **1.7 Run `cargo test -p edgezero-core`.** Expected: pass without router production changes. Checkpoint: `test(core): cover retained app ownership and request isolation`.

## Task 2: Host-testable Fastly production initialization

**File:** `crates/edgezero-adapter-fastly/src/lib.rs`.

Native `cargo test --features fastly` has a reproduced pre-existing unresolved-hostcall linker failure. Compile shared initialization logic under `cfg(any(feature = "fastly", test))`, as this file already does for `FastlyLogging`/`EnvConfig`. Do not introduce fake hostcalls.

- [x] **2.1 Write tests referring to private `RetainedApp` first.** Use counted closures and real core `App` values. Run `cargo test -p edgezero-adapter-fastly --lib retained_app`; expected initial failure: missing helper.
- [x] **2.2 Add this private production state** adjacent to the entry points:

```rust
#[cfg(any(feature = "fastly", test))]
#[derive(Default)]
struct RetainedApp {
    app: Option<edgezero_core::app::App>,
}

#[cfg(any(feature = "fastly", test))]
impl RetainedApp {
    fn get_or_init<E>(
        &mut self,
        env: &edgezero_core::env_config::EnvConfig,
        owns_logging: impl FnOnce() -> bool,
        install_logger: impl FnOnce(&str, log::LevelFilter, bool) -> Result<(), E>,
        build: impl FnOnce() -> edgezero_core::app::App,
    ) -> Result<&edgezero_core::app::App, E> {
        if self.app.is_none() {
            let logging = FastlyLogging::from(env);
            if logging.use_fastly_logger && !owns_logging() {
                install_logger(
                    logging.endpoint.as_deref().unwrap_or("stdout"),
                    logging.level,
                    logging.echo_stdout,
                )?;
            }
        }
        Ok(self.app.get_or_insert_with(build))
    }
}
```

Apply existing documentation/lint conventions. The helper stores successful initialization only; it does not cache errors or emulate SDK termination. Task 3 uses this exact state in production.

- [x] **2.3 Add this degraded-snapshot regression**, importing `App`, `RouterService`, and `EnvConfig` from core in the colocated module:

```rust
#[test]
fn retained_app_keeps_degraded_first_logging_decision() {
    let mut retained = RetainedApp::default();
    let installs = std::cell::Cell::new(0);
    let builds = std::cell::Cell::new(0);
    let first = EnvConfig::from_vars(std::iter::empty::<(String, String)>());
    let later = EnvConfig::from_vars([
        ("EDGEZERO__LOGGING__ENDPOINT", "fixture-logs"),
        ("EDGEZERO__LOGGING__LEVEL", "debug"),
    ]);
    for env in [&first, &later] {
        let app = retained.get_or_init(
            env,
            || false,
            |_, _, _| {
                installs.set(installs.get() + 1);
                Ok::<(), &'static str>(())
            },
            || {
                builds.set(builds.get() + 1);
                App::with_name(RouterService::builder().build(), "retained")
            },
        ).expect("initialization");
        assert_eq!(app.name(), "retained");
    }
    assert_eq!(builds.get(), 1);
    assert_eq!(installs.get(), 0);
}
```

- [x] **2.4 Complete these cases:**

| Case               | Assertion                                                                                      |
| ------------------ | ---------------------------------------------------------------------------------------------- |
| Configured logging | `logger`, then `build`, once across two calls; endpoint/level from first env; stdout true      |
| Owned logging      | Zero adapter installs, one build                                                               |
| Logger error       | Exact injected error, zero builds, no stored app; do not present same-owner retry as supported |
| Fresh owner        | Two state values initialize independently, no shared static                                    |
| Retained app       | Two dispatches use the same configured router/state                                            |

- [x] **2.5 Run `cargo test -p edgezero-adapter-fastly --lib` after each increment.** Expected: default-feature tests pass without SDK linkage. Checkpoint: `feat(fastly): separate retained initialization from host calls`.

## Task 3: Fastly opt-in serving APIs

**File:** `crates/edgezero-adapter-fastly/src/lib.rs`.

- [x] **3.1 Add these public APIs:**

```rust
#[cfg(feature = "fastly")]
pub use fastly::http::serve::{Serve, ServeSummary};

#[cfg(feature = "fastly")]
#[inline]
pub fn serve_app<A: Hooks>(serve: Serve) -> ServeSummary<fastly::Error> {
    serve_app_with_request_extensions::<A, _>(serve, |_request, _extensions| {})
}

#[cfg(feature = "fastly")]
#[inline]
pub fn serve_app_with_request_extensions<A, F>(
    serve: Serve,
    mut extend: F,
) -> ServeSummary<fastly::Error>
where
    A: Hooks,
    F: FnMut(&fastly::Request, &mut Extensions),
{
    let stores = A::stores();
    let mut retained = RetainedApp::default();
    serve.run(move |req| -> Result<fastly::Response, fastly::Error> {
        let env = runtime_env_config(stores);
        let app = retained.get_or_init(&env, A::owns_logging, init_logger, A::build_app)?;
        request::dispatch_with_registries(app, req, stores, &env, &mut extend)
    })
}
```

- [x] **3.2 Add API docs** for ordinary `main`, first-callback initialization, degraded logging snapshot, fresh registries/extensions, `owns_logging`, limits, terminal SDK callback errors, attempted counts, and `.into_result()`. Do not promise progressive streaming on this standard path.
- [x] **3.3 Verify:**

```sh
cargo test -p edgezero-adapter-fastly --lib
cargo check -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1
cargo check -p edgezero-adapter-fastly --features fastly --all-targets
```

Expected: host logic passes and SDK wrappers type-check. HTTP execution remains Task 7.

- [x] **3.4 Confirm all existing helpers/logger/templates are unchanged.** Checkpoint: `feat(fastly): add opt-in retained app serving`.

## Task 4: Cloudflare and Spin prebuilt dispatch

**Files:** `crates/edgezero-adapter-cloudflare/src/lib.rs`, `crates/edgezero-adapter-spin/src/lib.rs`.

- [x] **4.1 Add the Cloudflare wrapper:**

```rust
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[inline]
pub async fn dispatch_app(
    app: &edgezero_core::app::App,
    stores: StoresMetadata,
    req: Request,
    env: Env,
    ctx: Context,
) -> Result<Response, WorkerError> {
    let env_config = env_config_from_worker(&env, stores);
    request::dispatch_with_registries(
        app, req, env, ctx,
        request::RegistryInputs {
            config_meta: stores.config,
            kv_meta: stores.kv,
            secret_meta: stores.secrets,
            env_config: &env_config,
        },
    ).await
}
```

- [x] **4.2 Run `cargo test -p edgezero-adapter-cloudflare` and `cargo check -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown`.** Binding/runtime assertions come in Task 8.
- [x] **4.3 Add the Spin wrapper:**

```rust
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub async fn dispatch_app(
    app: &App,
    stores: edgezero_core::app::StoresMetadata,
    req: SpinRequest,
) -> anyhow::Result<SpinFullResponse> {
    let env = EnvConfig::from_env();
    request::dispatch_with_registries(
        app, req, stores.config, stores.kv, stores.secrets, &env,
    ).await
}
```

- [x] **4.4 Run `cargo test -p edgezero-adapter-spin` and `cargo check -p edgezero-adapter-spin --features spin --target wasm32-wasip2`.** Add public docs for explicit metadata, caller-owned app/logging, fresh request setup, and overlapping invocations.
- [x] **4.5 Leave both `run_app` bodies unchanged.** Delegating them directly would move configuration reads after app construction. Small additive wrappers preserve exact ordering without extra plumbing. Checkpoint: `feat(adapters): dispatch prebuilt Cloudflare and Spin apps`.

## Task 5: Build the portable probe fixture

**Files:** `tests/fixtures/reusable-app/{Cargo.toml,Cargo.lock,.gitignore}` and `crates/fixture-core/{Cargo.toml,src/lib.rs}` beneath that directory.

- [x] **5.1 Create an isolated workspace.** Use resolver 2, edition 2024, `publish = false`, and `default-members = ["crates/fixture-core"]`. Workspace EdgeZero paths are `../../../crates/<crate>`. Provider packages enable their own features; core uses `default-features = false`. Match the repository's SDK versions. Keep Tokio confined to the native Axum fixture. Ignore `target`, `.runs`, `.wrangler`, `.spin`, and generated worker output. Generate and commit the fixture lockfile; all subsequent builds use `--locked`.
- [x] **5.2 Add counted `FixtureApp` and a distinct `OtherApp`.** Count `build_app` and `configure` independently. Supply explicit config/KV/secret metadata with fixture-only names. Issue a fresh unique synthetic candidate boot token on every incoming request; each guest latches only its first candidate as its instance marker and increments a guest-local request ordinal. Never share a candidate token across a run or runtime process: fresh guests must report different markers. This test marker must never contain real request data or appear as a production caching example.
- [x] **5.3 Add a failing probe contract test.** Two requests must return their own path, header, body, and request-extension tokens; an intentionally shared `Arc` counter must increment. Run `cargo test --manifest-path tests/fixtures/reusable-app/Cargo.toml -p fixture-core`; expect the missing probe behavior to fail.
- [x] **5.4 Implement the probe with `#[action]` handlers and core HTTP imports.** Keep shared state limited to counters and the controlled fixture identity. Return JSON records containing instance, ordinal, build/configure counts, and echoed synthetic request fields.
- [x] **5.5 Add `/cookies` and `/rendered-error`.** Append two distinct `Set-Cookie` values; return an ordinary renderable application error on the latter route. Add assertions for both and rerun the scoped tests.
- [x] **5.6 Add `/stream`, `/stream-error`, and `/overlap/{id}`.** Use a controlled loopback backend through the injected proxy. The overlap route captures request values before awaiting a barrier and returns them afterward. An in-flight guard increments/decrements atomics without keeping a mutex guard across an await. Return the observed maximum in-flight count.
- [x] **5.7 Add `/bindings` and `/origin/{id}`.** Report only binding presence and nonsecret fixture markers. Origins come from a finite harness-owned allowlist. Test unknown-origin rejection and both concrete apps' different route identities.
- [x] **5.8 Run the locked core fixture tests.** Expected: all portable assertions pass without provider calls. Checkpoint: `test(lifecycle): add portable retained-app probes`.

## Task 6: Build the local HTTP evidence harness

**Files:** `tests/fixtures/reusable-app/crates/fixture-harness/src/main.rs` and `scripts/smoke_test_reusable_app.sh`.

- [x] **6.1 Define the runner interface.** Accept `--adapter fastly|cloudflare|spin|axum|all`, `--suite smoke|benchmark`, `--output <directory>`, and `--require-runtime`. Resolve `VICEROY_BIN`, `WORKER_BUILD_BIN`, `WRANGLER_BIN`, and `SPIN_BIN` explicitly to absolute executables and record versions. Never install or deploy automatically.
- [x] **6.2 Write failing Rust tests for evidence accounting.** Cover a failed attempted request, a missing final summary, two duplicate headers, unavailable CPU measurements, and invalid overlap evidence. Run `cargo test --manifest-path tests/fixtures/reusable-app/Cargo.toml -p fixture-harness`; expect failures before implementing the parser/accounting.
- [x] **6.3 Implement JSONL accounting.** Record run/tool/build identity, request start, response commitment, guest completion, client completion, errors, SDK summary, metrics, and capability/skip events. Attempted requests and successful completions are different fields. Missing crash summaries are unknown, not zero. Every metric has source, units, and observed/injected/unsupported/unverified status.
- [x] **6.4 Implement a Rust loopback backend using `TcpListener` and scoped worker threads.** Provide a bounded two-arrival barrier, delayed chunked response with an explicit release signal, and abrupt connection closure. Flush the first chunk before waiting. Bind only loopback addresses and use allocated ports.
- [x] **6.5 Implement a bounded Rust HTTP client for the loopback fixtures.** Preserve response headers as a list; do not collapse duplicate cookies into a dictionary. Capture header/first-byte/full-response timing with `Instant`, partial-body failures, and concurrent requests using Rust threads.
- [x] **6.6 Test the streaming and overlap oracles.** Equal final bodies alone must fail the progressive-stream assertion. Two different instance markers or a maximum in-flight count of one must fail same-instance overlap. First-byte delivery must precede the controlled backend's final-chunk release.
- [x] **6.7 Implement process orchestration.** Copy configs into unique ignored `.runs/<run-id>` directories, resolve copied paths, enforce deadlines, and clean up only child processes started by this invocation. Exit 0 means all requested assertions passed; 1 means build/assertion failure; 2 means required runtime/evidence is unavailable. Optional skips remain an explicitly incomplete result and must never hide assertion failures.
- [x] **6.8 Run the Rust harness tests and `bash -n scripts/smoke_test_reusable_app.sh`.** Expected: passing tests and valid shell syntax. Checkpoint: `test(lifecycle): add local HTTP evidence harness`.

## Task 7: Exercise Fastly default, rebuilt, and retained applications

**Files:** `tests/fixtures/reusable-app/crates/fixture-fastly/{Cargo.toml,fastly.toml,src/lib.rs,src/bin/single_request.rs,src/bin/rebuild_per_request.rs,src/bin/retained_app.rs,src/bin/custom_single_request.rs,src/bin/custom_rebuild_per_request.rs,src/bin/custom_retained_app.rs,src/bin/logger_negative.rs}`; extend harness Fastly orchestration.

- [x] **7.1 Declare explicit binary names.** Use `fixture-fastly-a`, `fixture-fastly-b`, `fixture-fastly-c`, `fixture-fastly-custom-a`, `fixture-fastly-custom-b`, `fixture-fastly-custom-c`, and `fixture-fastly-logger-negative`. Pin SDK 0.12.1 and build on `wasm32-wasip1`.
- [x] **7.2 Implement standard A with the existing single-request macro entry point.** Call `run_app_with_request_extensions`, adding only fixture observations. Preserve its current logging and construction behavior.
- [x] **7.3 Implement standard B using raw `Serve::run`.** Read runtime configuration each callback, make the logging decision once from the first snapshot, build the app each callback, and call `dispatch_with_registries`. Use the same logging policy as C, including a degraded first snapshot. Do not call retaining `serve_app` for B.
- [x] **7.4 Implement standard C with `serve_app_with_request_extensions`.** Start with a finite request limit of 10 and a bounded idle wait. Record SDK attempted-request summaries separately from client success. Compare A/B/C with identical payloads, dispatch, and instrumentation.
- [x] **7.5 Implement custom A/B/C around a shared raw dispatch function.** A owns one native request; B builds each callback; C captures an ordinary `Option<App>` and initializes it lazily. Mutate a synthetic native header, record native metadata, convert into core, route, recover response extensions, and apply a fixture finalizer. A health request before C's first application request must leave its build count at zero.
- [x] **7.5a Verify request correlation independently of sandbox identity.** Capture `Request::get_client_request_id()` on each callback and record `FASTLY_TRACE_ID` only as sandbox metadata. Never use the latter as a unique request ID. When the native ID is unavailable, use the harness-issued unique per-request token as an explicitly labeled fixture fallback; record availability and do not present fallback evidence as native-ID support. Across callbacks in the same observed guest, verify that correlation is derived afresh from each request and does not persist in the global logger or retained app. Keep correlation in request-local extensions or explicit per-event log fields. Exercise the fallback through a supported runtime case or a labeled injected accessor result.
- [x] **7.6 Implement manual progressive response sending in the custom function.** Append every header value. For a core stream, create the native response, commit using `stream_to_client`, pump chunks, and finish the writable body before callback return. Handle errors after commitment locally with explicit logging/cleanup; do not return an error that causes the SDK to attempt another response. Complete instrumented post-send backend work within the callback and prove the next callback starts afterward.
- [x] **7.7 Configure local backends and stores.** Derive runtime configuration's service-scoped keys from the guest's actual service ID, following `scripts/smoke_test_config_key_override.sh`. Keep config, KV, and secret fixtures isolated. Capture a named logging endpoint distinctly from echoed stdout; if the selected runtime cannot expose endpoint-specific evidence, report that assertion unavailable.
- [x] **7.8 Add terminal and continuing cases.** Rendered handler errors should allow later callbacks in the same instance. Required-KV open errors, inbound conversion errors, response collection errors, and error-rendering failure are separate escaping paths. Test controlled reachable failures; label injected/unavailable cases honestly. Add constructor panic, pre-commit failure, post-commit stream failure, and a later fresh-owner initialization case.
- [x] **7.9 Add the logger negative control.** Naively wrapping enabled-logger `run_app` in `Serve` should expose the second-installation failure. Keep this out of B's performance samples. Add the degraded-first-read/recovered-selector case where runtime controls permit; otherwise retain host injection evidence and mark the provider scenario unverified.
- [x] **7.10 Build and run the fixtures.** From the fixture workspace run `cargo build --locked --release -p fixture-fastly --bins --target wasm32-wasip1`. Launch the selected executable with `viceroy serve --addr 127.0.0.1:<port> --config <run-config> <absolute-wasm-path>`. Account for readiness callbacks or restart before measuring. Run `./scripts/smoke_test_reusable_app.sh --adapter fastly --suite smoke --require-runtime`.
- [x] **7.11 Assert actual reuse before accepting comparisons.** Stable guest marker plus increasing ordinal is required. A builds once per fresh invocation; B builds each reused callback; C builds once per retained owner. Check cookies, buffering versus progressive streaming, request isolation, limit 1/10, idle exit, and restart. Compare standard A/B/C and custom A/B/C separately.
- [x] **7.12 Run existing Fastly tests on their proper runners.** Run default host `cargo test -p edgezero-adapter-fastly --lib`. With the selected Viceroy on PATH, run `CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run" cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --test contract`, and repeat with `--lib`. Native feature-enabled linking is not a required test route. Checkpoint: `test(fastly): verify reusable sandbox lifecycle over HTTP`.

## Task 8: Exercise Cloudflare and Spin retained ownership

**Files:** fixture Cloudflare and Spin package files listed in the file map; provider sections of the harness.

- [x] **8.1 Add the Cloudflare fetch fixture.** Use a `cdylib`/`rlib` and `#[event(fetch)]`. A launch-time fixture mode selects unchanged `run_app` or a concrete `static APP: OnceLock<App>` with `dispatch_app` and full metadata. Add a separate concrete `OTHER_APP`; never place an unkeyed static inside a generic function. Do not retain `Env`, `Context`, bodies, or binding handles.
- [x] **8.2 Configure local Worker builds and bindings.** Use `main = "build/worker/shim.mjs"`, record a fixed compatibility date, and build with `worker-build --release . -- --locked` from the package. Seed only local KV using the same persistence directory supplied to Wrangler. No remote commands or account credentials are needed.
- [x] **8.3 Add the Spin HTTP fixture.** Use SDK 6's `#[http_service]`, concrete app statics, and explicit store metadata. Configure component permissions, fixture config/secret variables, and a local KV store in `runtime-config.toml`. Keep incoming/outgoing bodies and pending futures request-owned.
- [x] **8.4 Build and boot Spin.** Run `cargo build --locked --release -p fixture-spin --target wasm32-wasip2` from the fixture workspace. The original manifest component source is `../../target/wasm32-wasip2/release/fixture_spin.wasm`; resolve it to an absolute path in copied run manifests. Use the selected `spin up --listen <address> --runtime-config-file <config> --from <manifest>`. Verify the emitted interface by successful SDK6/P3 host boot; record independent component inspection as unavailable if no inspection tool is installed. The target name alone is insufficient.
- [x] **8.5 Detect Spin controls from the selected runtime.** The investigated Spin 4.0 help does not expose newer reuse/concurrency controls. Exercise controls only when advertised, record their values, and otherwise observe default behavior while marking those controls unavailable. Never blindly pass flags copied from newer documentation.
- [x] **8.6 Run sequential ownership and binding assertions on both providers.** Compare per-request versus retained builds, verify different concrete apps return different routes, and check named/default metadata, cookies, bodies, and rendered errors. Restart to prove cold initialization remains correct.
- [ ] **8.7 Force overlapping requests on the same guest.** Hold two distinct requests at the backend barrier, verify both arrivals, release them, and assert stable guest identity, maximum in-flight count at least two, and unchanged per-request fields after awaiting. Different instances or serialized delivery are incomplete evidence, not a pass; retry only within a bounded deadline. Retained Spin and both Cloudflare modes passed; the latest Spin per-request control selected separate guests in all three attempts, so this full matrix criterion remains unverified.
- [x] **8.8 Verify recoverable binding failures where controllable.** A relaunch with changed bindings proves fresh initialization, not same-instance refresh. Record injected versus observed transitions and unavailable host failure modes separately.
- [x] **8.9 Run scoped host tests, actual-target checks, and each HTTP smoke command.** Use `cargo test -p edgezero-adapter-cloudflare`, `cargo test -p edgezero-adapter-spin`, and the Task 12 target checks. Run the script once with `--adapter cloudflare` and once with `--adapter spin`, both `--suite smoke --require-runtime`. Checkpoint: `test(adapters): verify retained app isolation on Workers and Spin`.

## Task 9: Verify Axum and integrate runnable CI evidence

**Files:** `tests/fixtures/reusable-app/crates/fixture-axum/{Cargo.toml,src/main.rs}`, harness Axum orchestration, `.github/workflows/test.yml`.

- [x] **9.1 Add the native reference fixture.** Use existing `dev_server::run_app::<FixtureApp>()` with an allocated host/port and isolated local-store configuration. Add no new Axum lifecycle API. Run the same probe and overlap workload, expecting one build per server start and another after restart.
- [x] **9.2 Run the native fixture.** Build `fixture-axum` with the fixture manifest and `--locked`; run `cargo test -p edgezero-adapter-axum` and `./scripts/smoke_test_reusable_app.sh --adapter axum --suite smoke --require-runtime`. Axum timings do not stand in for WASM results.
- [x] **9.3 Add Rust harness unit tests and portable fixture tests to CI.** The root workspace excludes this fixture workspace, so explicitly invoke its locked `fixture-core` tests. Preserve all existing root tests.
- [x] **9.4 Add the Fastly HTTP smoke step to the existing Fastly WASM matrix job.** Reuse its Viceroy installation and version from `.tool-versions`; do not add a competing download. Require the runtime and actual multi-request evidence. Keep existing WASM contract and library tests: they cover different behavior from a live multi-request fixture.
- [x] **9.5 Check CI command parity locally.** Cloudflare/Spin HTTP runtime provisioning is not currently supplied by the existing contract matrix; retain explicit manual commands and report unavailable evidence. Do not label their WASM contract success as overlapping live-request validation. Checkpoint: `ci: run reusable Fastly HTTP smoke fixtures`.

## Task 10: Produce controlled lifecycle measurements

**Files:** benchmark/accounting sections of `tests/fixtures/reusable-app/crates/fixture-harness/src/main.rs` and fixture instrumentation; reports go under ignored `.runs/`.

- [x] **10.1 Add measurement validation tests.** Reject missing metrics represented as zero, negative phase deltas, mismatched tool/build identities, and SDK wall time mislabeled as CPU. Run the Rust harness tests before and after implementing the report functions.
- [x] **10.2 Preflight guest CPU and memory capabilities.** Before enabling `with_max_memory`, require a successful guest heap snapshot. If unsupported, omit that optional limit in the comparison and report the metric unsupported. Separately test conservative SDK termination only if the selected host can produce the unsupported-snapshot failure naturally or exposes a documented hostcall fault-injection facility. Record that mechanism and label injected evidence. Otherwise mark this runtime case unsupported/unverified; do not patch the SDK or substitute a decision-model test as provider evidence. Record precision and source for every memory/CPU observation.
- [x] **10.2a Exercise all four independent SDK limits.** Test `with_max_requests` with 1 and 10, `with_timeout` with a controlled idle gap, `with_max_lifetime` by crossing a positive elapsed deadline during controlled initialization before the next callback, and `with_max_memory` after successful heap preflight with a threshold below the measured fixture baseline. Check summary/next-instance behavior without assuming exact eviction timing or guaranteed reuse. Keep limit-boundary tests separate from performance runs so one limit cannot mask another.
- [x] **10.3 Instrument phases.** Capture before/after initialization and request cleanup memory; build/configure counts; initialization and handler wall time; supported SDK CPU phase deltas; request commitment, guest completion, and client completion. Returned-response sending may occur outside callback timing. Keep SDK attempted counts separate from completions.
- [x] **10.4 Run repeated matched A/B/C workloads.** Default to three repetitions of 100 sequential requests per variant, finite limit 10, randomized variant order, release builds, and identical instrumentation/backend/payloads. Record configurable sample counts and seeds. Include cheap and deliberately expensive construction, delayed/large streams, failures, idle gaps, and a longer bounded run with a larger finite limit for memory growth.
- [x] **10.5 Exercise finite proxy-origin pressure.** Compare one repeated origin with many distinct loopback origins owned by the harness. Bound count and deadlines, and record waits/failures/completions. Local behavior cannot establish the deployed service-wide dynamic-backend capacity limit, which may block registrations pending capacity.
- [x] **10.6 Exercise malformed input and interrupted bodies.** Distinguish host rejection before dispatch from observed adapter conversion errors; label fault injection explicitly. Do not infer a demonstrated exploit from a hypothetical request.
- [x] **10.7 Summarize raw evidence.** Report sample counts and documented p50/p95/p99 calculations for cold/reused first-byte and completion latency; attempts/completions per guest; build/configure counts; supported CPU observations; memory peak, slope, and plateau over actual ordinals. Distinguish guest linear-memory high-water marks, host-inclusive snapshots, and process RSS. Rounded MiB observations cannot rule out small leaks.
- [x] **10.8 Classify each result.** Use pass/fail/unsupported/unverified. Require a reproducible workload benefit and bounded retained growth before recommending adoption; set no invented speedup target. SDK vCPU readings alone do not justify cross-run performance claims. Deployed reuse frequency, eviction, logging endpoint validity, resource accounting, and performance remain unverified until separately authorized measurements.
- [x] **10.9 Rerun Rust harness and affected scoped tests.** Keep raw generated reports ignored unless explicitly approved for publication and scrubbed. Checkpoint: `test(lifecycle): measure initialization and reuse separately`.

## Task 11: Document supported ownership and adoption

**Files:** `docs/guide/adapters/{overview,fastly,cloudflare,spin,axum}.md`, `tests/fixtures/reusable-app/README.md`, and the relevant comment in `examples/app-demo/crates/app-demo-core/src/lib.rs`.

- [x] **11.1 Update the adapter overview.** Explain portable ownership versus provider scheduling; safe retained app/configuration values versus request-owned handles/extensions/bodies/pending work; intentional shared state; and caller-owned refresh, rotation, isolation checks, and benchmarks.
- [x] **11.2 Update Fastly's guide.** Show the compiled standard helper and raw callback examples. Cover all four SDK limits, first-snapshot logging, `FnMut` reborrowing, explicit sends/streams, post-send completion, attempted summaries, terminal errors including failed error rendering, memory preflight, and possible fresh initialization at any request. State that retained apps do not automatically remove explicitly repeated construction or parsing elsewhere. Explicitly distinguish sandbox `FASTLY_TRACE_ID` from per-request `Request::get_client_request_id()`. Document an application-generated per-request correlation fallback when native IDs are unavailable, identify its source, and keep it out of retained logger/app state. Explain persistent warning suppression, bounded-cache eviction, and why warning frequency does not measure failure frequency.
- [x] **11.3 Update Cloudflare and Spin guides.** Show concrete app-owned caches with explicit metadata and request-local dispatch. Explain caller logging before initialization, concurrent access, P3 interface verification, and runtime-specific capability detection. Avoid generic-static examples.
- [x] **11.4 Update Axum's guide and qualify the demo comment.** Document its existing retained router; describe the Fastly comment as applying to the default entry point. Keep default generated templates and macro settings unchanged.
- [x] **11.5 Write fixture instructions from commands actually exercised.** Include tool selection, build targets, local store setup, smoke and benchmark commands, evidence statuses, and default-entry-point rollback. Restore A to test rollback; limit one alone does not recreate all original initialization behavior. Mark unmeasured results explicitly.
- [x] **11.6 Compile every example through the matching fixture target.** Keep guide snippets synchronized with the compiled fixtures rather than adding a second untested example family. Run scoped Cargo tests if the demo source comment changes, per repository convention.
- [x] **11.7 Run docs checks.** From `docs`, run `npm run format`, `npm run lint`, and `npm run build`. Check all new documentation for prohibited external consumer identifiers and local machine paths. Checkpoint: `docs: explain opt-in application retention across adapters`.

## Task 12: Final verification and review

**Files:** no planned new implementation files; fix only findings in the files above.

- [x] **12.1 Review the final diff against the spec.** Confirm unchanged default entry points and generation, core traits and macro grammar, request/response conversion semantics (with the documented Cloudflare duplicate-header correction), header duplication, existing stream caps, and error policy. Production changes remain limited to the three adapter library files and the focused Cloudflare response-header fix, plus tests/docs.
- [x] **12.2 Run all repository gates.** Expected: each command exits successfully; record unavailable targets/tools separately rather than claiming success.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

- [x] **12.3 Verify the standalone workspace explicitly.** Run its format check, locked core tests, each provider's target build, native Axum build, Rust harness tests, and all available HTTP smoke suites. Do not run native all-feature tests across its Fastly members. Root Cargo success does not cover this nested workspace.
- [x] **12.4 Request independent review using `superpowers:requesting-code-review`.** Supply this plan, the governing spec, the diff, and actual test/evidence results. Review architecture/default compatibility and runtime/build evidence as separate scopes. Use `superpowers:verification-before-completion` before claiming implementation completion.
- [x] **12.5 Resolve findings and rerun affected checks.** Broaden repeat testing only when changes or unresolved concerns justify it. Do not silently weaken an evidence assertion because a local runtime cannot satisfy it.
- [x] **12.6 Report delivery status.** List APIs shipped, default compatibility, passed gates, observed provider behavior, unsupported/unverified acceptance items, and required application-owned adoption work. No deployment, PR/issue comment, or performance claim beyond the evidence is authorized by this plan.

## Coverage and decision record

| Requirement                                                    | Implementation / evidence                       |
| -------------------------------------------------------------- | ----------------------------------------------- |
| Portable ownership, fresh request state                        | Tasks 1, 4, 5, 8, 9                             |
| Fastly opt-in lifecycle, all supported limits                  | Tasks 2, 3, 7, 10, 11                           |
| Host-testable production initialization                        | Task 2; provider assertions remain in Tasks 7–8 |
| Custom mutable dispatch, streaming, finalization, pending work | Task 7                                          |
| Metadata and logging snapshots/recovery                        | Tasks 2–4, 7–8                                  |
| Attempted versus completed requests and terminal failures      | Tasks 6, 7, 10                                  |
| Same-instance overlapping-request isolation                    | Tasks 1, 6, 8, 9                                |
| A/B/C cost separation and bounded memory                       | Task 10                                         |
| Generated/default compatibility and rollback                   | Tasks 3–4, 7, 11–12                             |
| Persistent-state audit and supported cleanup                   | Tasks 1.6a–1.6b, 7.8, 11.2                      |
| Sandbox identity versus request correlation                    | Tasks 7.5a, 11.2                                |
| Local versus deployed evidence                                 | Tasks 6–12                                      |

The planning review compared two focused proposals: a private Fastly state helper with thin wrappers, and a standalone provider fixture workspace that reuses existing CI runtime setup. The selected approach keeps SDK calls out of host unit tests while testing the same production initialization state. It preserves existing dispatcher ordering and avoids a new public lifecycle abstraction. Independent complete-plan review approved this plan after two corrections: unique candidate instance tokens on every request, and explicit unsupported/unverified status when no SDK heap-hostcall failure mechanism is available. Implementation outcomes are checked below; these marks do not assert that a suggested commit was made or that unavailable provider evidence passed.

A subsequent independent alignment review found two verification omissions from spec §6.3. Tasks 1.6a–1.6b now schedule the persistent-state audit and supported cleanup checks; Tasks 7.5a and 11.2 explicitly cover request correlation and its fallback. These are verification/documentation additions, not evidence of a production defect or a change to lifecycle ownership.

## Execution checkpoint

Implementation is on `feat/reusable-app-lifecycle` in the original checkout.
Changes remain uncommitted. Existing default entry points, generated templates,
macro grammar, and lifecycle error policy are unchanged. One targeted compatibility
fix is included: Cloudflare now preserves repeated application response headers,
including `Set-Cookie`, while still replacing generated defaults with the first
application value. Live tests reproduced the original loss and pass with the fix.

Tasks 1–9 and 11–12 have implementation and local verification, subject to the
explicit full-matrix exception in 8.7. Task 10 has the measurement implementation
and controlled runs recorded in the fixture README. Checkmarks denote completed
implementation/verification work, including an explicit capability disposition;
they do not turn unsupported behavior into a passing provider assertion.

The final Rust driver has 17 unit tests; the portable fixture has two. Root
workspace tests, strict all-feature Clippy, formatting, feature checks, and
Fastly WASM library/contract tests pass. Provider target builds and live HTTP
checks exercise all four adapters. Fastly's expanded fault cases, Cloudflare's
both-mode overlap, and Cloudflare/Spin retained binding recovery pass. Spin's
retained overlap passes; the latest all-provider invocation exits 2 because
its per-request control did not observe same-guest overlap after three attempts.
No assertion failure is hidden by that status.

Remaining evidence is environmental or outside this implementation:

- No local control was found for transient failure of the fixed optional runtime
  store or an unavailable heap hostcall. Host snapshot injection and real required
  binding recovery are separate, labeled checks.
- No reachable public input triggers an error in the current error renderer.
  A trap is not evidence of a returned rendering error.
- No component-inspection executable is installed; successful Spin SDK6 component
  boot supplies runtime compatibility evidence.
- Standard returned-response helpers expose conversion observations, not a
  post-send callback; unavailable CPU/cleanup phases remain unknown.
- Deployed reuse frequency, resource accounting, capacity, and application workload
  benefit require separately authorized deployment and application-owned adoption.
- The 300-probe run with limit 100 still observed at most six requests per guest.
  Rounded heap samples were stable at 2 MiB; longer per-instance growth remains
  unverified because increasing the configured limit did not extend host reuse.
  Cached Viceroy 0.17.0 source confirms a hardcoded five additional requests
  (`NEXT_REQ_ACCEPT_MAX`); the installed CLI has no override.

The bounded lifetime test crosses a 50-ms deadline during a 100-ms initialization;
it proves the next callback is not admitted after expiration, not interruption of
an active handler. The memory-limit test uses a threshold below the observed
fixture baseline after successful preflight. Neither requires an unbounded load.

The HTTP driver is Rust. Source files use descriptive names; A/B/C are experiment
labels only. Raw evidence stays under ignored `.runs/` directories. Independent
measurement and diff reviews led to explicit unknown metrics, consistent request
correlation, and phase-specific CPU/memory reporting; the final review found no
further actionable issue. No deployment, commit, or external comment was made.

The follow-up risk review added a core cancellation regression: dropping a
suspended request releases its extension resources while retaining the router,
and a subsequent request has no stale request extension. The adapter overview
now gives concrete mitigations for stale snapshots, cache growth, concurrent
mutation, logging ownership, failures, and partial streams. These protect and
explain existing behavior; application refresh policies and provider lifecycle
limits remain outside the framework's ownership.
