# Opt-in reusable application lifecycle

- **Status:** Proposed; implementation requires approval.
- **Date:** 2026-09-15
- **Source baseline:** EdgeZero `593fc9282a1c56e12bae15f91eef2162f4b6a1b7`.
- **Scope:** Core application ownership and the Fastly, Cloudflare, Spin, and Axum adapter surfaces, including custom dispatch, examples, documentation, and validation.
- **Evidence:** Source inspection and provider documentation. No lifecycle experiment or performance result is claimed by this spec.

## 1. Objective

Allow applications to amortize app/router construction across requests when the host supports reuse. Separate retained initialization from request setup without changing existing entry-point defaults or moving application policy into the framework.

There are two independent operations:

1. A host permits an instance to receive another request.
2. Application code retains initialized objects rather than rebuilding them when that request arrives.

Enabling the first does not automatically implement the second. Retaining the router also does not eliminate configuration reads, request conversion, registry construction, signing, parsing, or other work that application handlers still perform explicitly.

Correctness must tolerate fresh initialization on any request. Retention is an optimization within one process, sandbox, isolate, or component instance, not durable storage or a routing-affinity guarantee.

## 2. Verified baseline

Paths and line numbers below refer to the source baseline, not future implementation locations.

| Surface                  | Current behavior                                                                                                                                       | Reference                                                                          |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| Core construction        | `Hooks::build_app` constructs routes and calls `configure`. `App` contains a name and router, not store metadata or logging policy.                    | `crates/edgezero-core/src/app.rs:14`, `:109`                                       |
| Core dispatch            | `RouterService` owns `Arc<RouterInner>` and supports repeated `oneshot(&self, request)`.                                                               | `crates/edgezero-core/src/router.rs:298`, `:349`                                   |
| State                    | Registered state is cloned into each request; shared interiors remain shared. App state overwrites extensions of the same type.                        | `crates/edgezero-core/src/router.rs:212`, `:251`                                   |
| Macro                    | `app!` generates `Hooks` and router construction. Its state expression executes when the router is built. It does not generate the provider lifecycle. | `crates/edgezero-macros/src/app.rs:214`, `:229`                                    |
| Fastly                   | Each `run_app_with_request_extensions` call reads runtime configuration, conditionally installs logging, builds the app, and dispatches.               | `crates/edgezero-adapter-fastly/src/lib.rs:171`                                    |
| Fastly logger            | Installing the process-global logger a second time returns an error.                                                                                   | `crates/edgezero-adapter-fastly/src/logger.rs:52`                                  |
| Fastly prebuilt dispatch | Public `request::dispatch_with_registries` accepts `&App`, metadata, runtime configuration, and a raw-request extensions callback.                     | `crates/edgezero-adapter-fastly/src/request.rs:337`                                |
| Cloudflare               | `run_app` rebuilds the app on every fetch. Registry-aware dispatch already takes `&App` but is private.                                                | `crates/edgezero-adapter-cloudflare/src/lib.rs:98`, `src/request.rs:298`           |
| Spin                     | `run_app` rebuilds the app on every handler invocation. Registry-aware dispatch already takes `&App` but is private.                                   | `crates/edgezero-adapter-spin/src/lib.rs:111`, `src/request.rs:183`                |
| Cloudflare/Spin logging  | Both adapter logger initializers are currently no-ops. Their existing entry points honor `owns_logging`.                                               | Cloudflare `src/lib.rs:30`; Spin `src/lib.rs:65`                                   |
| Axum                     | `run_app` builds the app once before starting the server, then shares the router through cloned services.                                              | `crates/edgezero-adapter-axum/src/dev_server.rs:352`, `:302`; `src/service.rs:132` |

### 2.1 Host lifecycle distinctions

- **Fastly:** reuse is opt-in through an ordinary `main` and SDK `Serve`. A sandbox processes requests sequentially. Lifecycle limits are checked between requests; the platform may end a sandbox earlier.
- **Cloudflare:** an isolate can handle overlapping requests on its event loop. No application loop requests the next fetch. Retention must tolerate eviction and must not associate the first request's bindings or context with subsequent requests.
- **Spin:** the pinned SDK 6 HTTP macro exports WASI P3. The cached `spin-macro-6.0.0/src/lib.rs:131` explicitly exports `wasip3::http::service`. The Rust `wasm32-wasip2` build target does not establish a P2 HTTP lifecycle. Current Spin documentation describes P3 instance reuse, including concurrent invocation; P2 components have different behavior.
- **Axum:** the existing long-running service already retains its router and permits concurrent requests.

Provider references:

- [Fastly sandbox lifecycle](https://www.fastly.com/documentation/guides/compute/developer-guides/sandbox-lifecycle/)
- [Fastly 0.12.1 serving API](https://docs.rs/fastly/0.12.1/fastly/http/serve/index.html)
- [Cloudflare runtime model](https://developers.cloudflare.com/workers/reference/how-workers-works/)
- [Spin instance reuse](https://spinframework.dev/v4/http-trigger#controlling-instance-reuse)

These distinctions prohibit a universal sequential serving loop in core.

## 3. Scope and ownership

### Framework responsibilities

- Support dispatch against a prebuilt application with complete store-metadata wiring.
- Provide the Fastly opt-in lifecycle and retained standard-app convenience.
- Document concrete application-owned retention for Cloudflare and Spin.
- Preserve fresh request conversion, contexts, extensions, and registry resolution.
- Preserve existing response translation and custom-dispatch escape hatches.
- Document lifecycle, concurrency, failure, refresh, and resource boundaries.
- Provide framework contract tests and local lifecycle fixtures.

### Application responsibilities

- Decide which settings, parsed objects, registries, clients, middleware caches, and other objects to retain.
- Define refresh, invalidation, key rotation, bounded cache growth, and initialization-failure policies.
- Keep request-derived data out of shared initialization unless explicitly designed for safe sharing.
- Own raw-request mutation, application response finalization, and post-send application work.
- Prove application-specific isolation and measure representative workloads.

### Non-goals

- No automatic caching inside existing `run_app` functions.
- No new core `Hooks` methods, macro arguments, manifest keys, CLI reuse switch, or provider-independent scheduler.
- No framework singleton registry keyed by application type.
- No SDK upgrade solely for this feature; Fastly 0.12.1 already provides `Serve`.
- No changes to body buffering, stream caps, error redaction, or response translation semantics.
- No durable cache, automatic configuration refresh, parsed-key cache, or application-specific finalization hook.
- No deployment, publishing, or external issue/PR activity as part of implementing this spec.

## 4. API design

### 4.1 Preserve core ownership

Use the existing `edgezero_core::app::{App, Hooks, StoresMetadata}` and `edgezero_core::http::Extensions`. Do not add metadata or lifecycle state to `App`.

`App` is structurally `Send + Sync`: handlers and middleware require these bounds, registered state requires them, and the router is reference-counted. Dispatch futures may remain non-`Send`. Add a compile-time assertion protecting the retained object's bounds without imposing `Send` on WASM futures.

### 4.2 Fastly standard lifecycle

Add these exports in `edgezero-adapter-fastly`, under its existing `fastly` gate:

```rust,ignore
pub use fastly::http::serve::{Serve, ServeSummary};

pub fn serve_app<A: Hooks>(serve: Serve)
    -> ServeSummary<fastly::Error>;

pub fn serve_app_with_request_extensions<A, F>(
    serve: Serve,
    extend: F,
) -> ServeSummary<fastly::Error>
where
    A: Hooks,
    F: FnMut(&fastly::Request, &mut Extensions);
```

`serve_app` delegates with an empty extensions callback. Both helpers:

1. Capture static `A::stores()` metadata.
2. Enter the SDK callback before performing host-dependent initialization.
3. Read runtime configuration for the current request.
4. On the first callback only, resolve logging policy, honor `A::owns_logging()`, initialize adapter logging if enabled, and call `A::build_app()`.
5. Retain the successfully constructed app for subsequent callbacks.
6. Use the first runtime configuration read for the first dispatch; do not read it twice.
7. On every request, use existing `request::dispatch_with_registries` with fresh registries and the current runtime configuration.
8. Return the SDK summary unchanged. Do not hide handler errors or synthesize a guaranteed request count.

The logger's enablement, level, endpoint, and stdout policy are a first-initialization snapshot, even if runtime selectors later change. Store selector reads remain per request. Preserve the existing `FastlyLogging::from(&EnvConfig)` mapping: it derives enablement from endpoint presence and currently fixes `echo_stdout` to true rather than applying the corresponding runtime override. This feature does not change that mapping. `owns_logging` retains its existing meaning: skip adapter logger installation. An application adopting retained construction must arrange its own logger installation before construction if construction needs logging.

**Degraded-first-read policy:** preserve the existing optional-store fallback and freeze the resulting logging decision for this sandbox. `runtime_env_config` does not distinguish an absent optional store from another store-open failure; both produce empty configuration. That disables named-endpoint logging for the sandbox even if a later runtime read succeeds. The `echo_stdout` value does not provide a fallback when no logger is installed. Later reads can restore store selectors but must not silently reconfigure the global logger. Test this explicitly. Applications requiring stricter logging availability must use the custom lifecycle with their own configuration-read and initialization policy. Adding a distinguishable read status, deferred logging initialization, or retry policy would require a separately reviewed API/ordering change; it is not implied by these helpers.

Capture the mutable extensions callback in the outer serving closure and pass a fresh `&mut extend` reborrow to each `dispatch_with_registries` invocation. Its `FnOnce` parameter can call that borrowed `FnMut`; do not move the callback out of the serving closure on the first request.

An initialization `Err` ends the SDK loop. `Hooks::build_app` is infallible by signature: a panic remains a sandbox failure, not a fabricated initialization `Result`. Do not retry after partially completed global initialization.

Example entry point after implementation:

```rust,ignore
use edgezero_adapter_fastly::{Serve, serve_app};

fn main() -> Result<(), fastly::Error> {
    serve_app::<MyApp>(Serve::new().with_max_requests(10)).into_result()
}
```

The example deliberately uses ordinary `main`, not `#[fastly::main]`. Existing generated entry points remain unchanged.

### 4.3 Fastly custom lifecycle

Use re-exported `Serve::run` or `run_with_context` directly. Preserve access to the SDK's `HandlerResult` trait through its existing SDK path; a new framework callback trait is unnecessary.

A callback receives an owned native request and may mutate it, capture metadata, convert it with `request::into_core_request`, dispatch through a retained router, inspect core response extensions, finalize the response, send or stream it explicitly, and finish request-owned post-send work before returning.

Applications may capture retained state in a closure or pass it through `run_with_context`. Host-dependent or fallible initialization belongs inside the callback. A lightweight health response may bypass expensive initialization; retaining state must not require eager construction before every callback can run.

Existing public `runtime_env_config` and `request::dispatch_with_registries` remain the complete prebuilt-app path when standard response translation is appropriate. The immutable extensions callback is not a substitute for mutable raw-request custom dispatch.

No framework-owned application-state object, teardown hook, or response-finalization pipeline is introduced.

### 4.4 Cloudflare and Spin prebuilt dispatch

Add public root-level dispatch helpers that take metadata explicitly:

```rust,ignore
// edgezero-adapter-cloudflare
pub async fn dispatch_app(
    app: &App,
    stores: StoresMetadata,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error>;

// edgezero-adapter-spin
pub async fn dispatch_app(
    app: &App,
    stores: StoresMetadata,
    req: spin_sdk::http::Request,
) -> anyhow::Result<SpinFullResponse>;
```

Use the same feature and target gates as each adapter's existing `run_app`.

These functions do not build an app, install logging, or cache anything. They resolve runtime configuration for that invocation, build registries using the supplied metadata, and call the existing internal registry-aware dispatcher. Metadata must describe the supplied app's intended store bindings; normal callers pass `MyApp::stores()`.

For Cloudflare, derive `EnvConfig` before moving `env` into the dispatcher and keep the configuration alive through the awaited call that borrows it in `RegistryInputs`. For Spin, unpack `stores.config`, `stores.kv`, and `stores.secrets` into the existing dispatcher's separate metadata arguments. These are adapter wrappers, not changes to internal dispatcher signatures.

Explicit metadata avoids silently losing store wiring and avoids pretending `&App` contains a `Hooks` implementation. Logging is excluded intentionally: a dispatcher must be safe to call repeatedly, and the caller controls initialization before constructing the app.

Existing `run_app` may delegate its dispatch portion to the new function if doing so preserves logger-before-build ordering, environment-resolution behavior, error behavior, and exactly one build per call. Do not refactor unrelated code for symmetry.

Do not route these helpers through the manual Cloudflare builder or Spin `AppExt::dispatch`: those paths do not preserve all metadata-aware behavior.

### 4.5 Concrete retention on Cloudflare and Spin

Document a static owned by the concrete application entry-point module:

```rust,ignore
use edgezero_core::app::{App, Hooks};
use std::sync::OnceLock;

static APP: OnceLock<App> = OnceLock::new();

fn retained_app() -> &'static App {
    APP.get_or_init(MyApp::build_app)
}
```

The fetch/HTTP callback obtains this reference synchronously and passes it, `MyApp::stores()`, and the current request arguments to `dispatch_app`.

Requirements for the example and API documentation:

- The cache belongs to one concrete application. Never put an unkeyed static inside a generic `cached<A>()`; such statics are shared across type instantiations.
- Perform any application-owned logging initialization before `get_or_init` if construction logs. Adapter logging is currently a no-op on these two targets; do not invent a new logger implementation in this change.
- The initializer is synchronous and must not recursively access the same cache. Do not block the event loop waiting on an async initializer or hold mutex/borrow guards across dispatch awaits.
- Retain only the app, not a future, native request, Worker `Env`/`Context`, or store registry.
- Binding/open failures occur during dispatch and are retried on a later request through normal request setup; they must not poison the app cache.
- The simple example does not define fallible/async initialization or refresh. Applications needing these manage an initialized-state owner and publish a complete successful snapshot before sharing it. No generic framework failure cache is added.
- App middleware and shared state now survive across requests and can be accessed by overlapping invocations. The application must consent to that changed lifetime by explicitly adopting the example.

### 4.6 Axum

Keep the existing construction and service ownership. Include Axum in the shared-state contract tests and documentation matrix, but add no reuse flag or alternate lifecycle helper.

This does not claim that all Axum per-request costs have already been optimized; for example, request conversion still creates proxy wiring. Such work is outside this app-retention change.

## 5. Fastly limits and termination

Expose SDK limit configuration without translating or duplicating it:

| SDK setting                   | Semantics to document                                                                                                                                             |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `with_max_requests(usize)`    | Positive values bound requests; zero means no SDK request-count limit. The example uses an explicit finite value.                                                 |
| `with_max_lifetime(Duration)` | Checked between requests, measured from `Serve` construction. Not an interrupt deadline for a running handler.                                                    |
| `with_max_memory(u32)`        | MiB snapshot threshold checked between requests; zero disables this SDK threshold. An unavailable measurement stops further reuse conservatively when configured. |
| `with_timeout(Duration)`      | Bounds waiting for another request, subject to platform limits. Does not set application/backend request timeout.                                                 |

The implementation must follow the pinned SDK's exact comparisons rather than introducing alternative threshold semantics. The platform may enforce additional limits, and request CPU/runtime limits still apply per request.

The pinned SDK stops after a handler error. It attempts a 500 containing the error's display text for `Result` handler returns. This is existing SDK behavior, not a new sanitization policy.

Distinguish an SDK callback error from an application error rendered as an HTTP response. `RouterService::oneshot` already renders ordinary handler errors, including propagated proxy errors, through `IntoResponse`; an HTTP 4xx/5xx response alone does not stop reuse. If rendering an application error itself returns `Err`, `RouterService::oneshot` propagates that failure too. Errors that escape the adapter boundary, such as error-rendering failure, required-KV opening, inbound conversion/body reads, or response stream collection failures, reach the SDK as `Err` and terminate that sandbox. Recovery then occurs through fresh initialization in another sandbox. The later-request binding retry described in §4.5 applies to Cloudflare/Spin. Do not add a catch-all response conversion to `serve_app`: it would change termination/summary semantics, and the current dispatcher has already erased `EdgeError` into `fastly::Error` text. Test rendered application errors and escaping adapter errors separately. No client-triggerable reuse-denial scenario or performance regression is established without a reproducible workload.

`ServeSummary::requests()` counts attempted callback invocations, not successfully completed responses: the SDK increments it before invoking the handler, including the callback that returns an error. A panic may prevent a summary from being returned at all. `ServeSummary` also reports handler wall time, wait wall time, and handler error. It does not expose all reasons for termination: a next-request wait error can end the loop with no summary handler error, and promise registration can panic. `into_result() == Ok(())` alone is not proof of healthy reuse or reaching the requested limit.

## 6. Response and resource contract

### 6.1 Preserve response behavior

Implementation review identified one focused compatibility correction: Cloudflare's
converter previously overwrote repeated application headers. Preserve each value,
including duplicate `Set-Cookie`, by replacing the generated default with the first
application value and appending the rest. This correction applies to both existing
and retained entry points; stream translation and response ownership stay unchanged.

| Adapter/path    | Existing behavior retained                                                                                                                     |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Fastly standard | Duplicate headers are appended. A core stream is consumed into a Fastly body before the response is returned.                                  |
| Fastly custom   | Explicit header commitment, chunk pumping, stream finish/drop, response-extension inspection, and application finalization remain possible.    |
| Cloudflare      | Preserve current stream translation and native response ownership.                                                                             |
| Spin            | Preserve fully buffered output and the current 16 MiB cap on streamed response collection; already buffered bodies do not use that stream cap. |
| Axum            | Preserve current streaming response conversion.                                                                                                |

Fastly and Spin inbound conversions buffer bodies. This feature must not be described as providing end-to-end streaming or removing body costs.

### 6.2 Explicit-send error ordering

Fastly callbacks returning `()` or `Ok(())` must have sent a response themselves. They must finish or abandon guest-owned streaming resources before returning. Successful return must not cause a second send.

Before committing headers, a callback may return an SDK-supported error. After committing a final response, returning `Err` through the built-in `HandlerResult` attempts another send and can panic. Custom code must handle post-commit errors explicitly: finish/drop the stream, log safely, and either return a completed outcome after cleanup or deliberately terminate the sandbox if state cannot safely continue. Do not imply `Result<(), E>` automatically distinguishes pre-commit and post-commit errors.

Complete required guest-owned post-send work within the Fastly callback. Finishing a guest response does not mean the peer has received every network byte; do not wait for network delivery acknowledgements. Cloudflare's documented request `wait_until` work remains tied to its own context, not to the retained app.

### 6.3 Retained versus request-owned state

| Retain only by deliberate policy                               | Keep fresh and request-owned                                                                 |
| -------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| App/router/routes, middleware instances, static store metadata | Raw/core requests and responses, headers, URI, route parameters                              |
| Immutable settings snapshots, parsed ordinary-memory objects   | Client metadata, authentication/session data, request IDs, request extension bags            |
| Bounded caches with explicit keys, eviction, and invalidation  | Native store wrappers/registries, body streams, pending backend operations, response effects |

Cloning a wrapper or satisfying `Send + Sync` is not proof of host-resource validity or request isolation. Persistent `Arc` interiors are intentionally shared; the framework does not clear their contents between requests. Existing same-type extension overwrite behavior must be tested and documented, not changed here.

Keep runtime configuration reads and store registry construction per request. Retained app state is a separate snapshot and will not automatically follow selector changes. A refresh must publish a coherent snapshot; in-flight requests keep the snapshot they acquired. Applications may choose not to refresh within an instance, but must document the resulting staleness policy.

Audit all process-global and thread-local state, including `OnceLock` caches, warning suppression, and recursion guards. Static warning suppression can reduce log volume across requests; warning rate must not be interpreted as failure rate. The current bounded recent-name set can evict entries, so suppression is not an absolute once-per-sandbox guarantee. Verify guard cleanup on supported exit/error paths; the presence of a thread-local guard alone does not demonstrate leakage.

**Dynamic backends:** `proxy::ensure_backend` creates names from scheme, host, and port, so requests targeting many distinct origins exercise growing registration diversity. Current Fastly documentation describes node-level registration reuse even without sandbox reuse, and a service-wide concurrent dynamic-backend limit that blocks awaiting capacity rather than necessarily returning an immediate registration error. Do not model this as a per-sandbox counter that resets all capacity on every request. Use authorized, bounded origin sets and consider static backends for predictable origins. Test repeated and distinct origins, registration conflicts, and time waiting for capacity. Tune sandbox request limits using measured behavior; `with_max_requests` alone cannot bound service-wide registrations, fan-out within a request, or time blocked inside a request. The between-request lifetime check cannot interrupt a blocked registration. See [registration scope](https://www.fastly.com/documentation/guides/integrations/non-fastly-services/developer-guide-backends/) and [dynamic-backend limit behavior](https://www.fastly.com/documentation/reference/compute/errors).

Fastly environment variables describe the sandbox: `FASTLY_TRACE_ID` must not be treated as a unique request identifier in reusable mode. Capture `Request::get_client_request_id()` separately for request correlation, with a documented fallback when unavailable. Do not retain request correlation fields in the global logger or app state.

## 7. Compatibility and unresolved platform evidence

- Existing Fastly, Cloudflare, and Spin `run_app` calls continue building each invocation; generated templates continue calling them.
- Existing Fastly `run_app_with_config`, manual service builders, raw conversion, and custom router dispatch retain their signatures and behavior.
- Existing `Hooks`, `app!` syntax, registered-state precedence, and provider feature gates remain unchanged.
- No Tokio dependency is added to core or WASM adapters. Examples import portable HTTP types from core and use `#[action]` for any new handlers.
- Reuse changes app-state lifetime only on explicit adoption. It must never silently become the default through a template or macro change.
- Adopting retention changes the frequency of construction-owned logging setup. `owns_logging` still only disables framework installation; applications that refresh logging policy during each construction must move that refresh to explicit request setup or accept an initialization snapshot. The framework does not invoke a new application logging hook.

### Platform verification gates

1. **Fastly logging endpoints:** the current logger retains native endpoint handles. Viceroy retains its endpoint table, but this is not a blanket deployed lifetime guarantee. Verify named-endpoint logging through multiple reused requests and obtain authoritative SDK/provider evidence for deployed validity before declaring full support. Assert actual receipt of distinct request-correlated records at the named endpoint after the first request; absence of returned errors or stdout output is insufficient. `log-fastly` ignores endpoint write errors, so invalid handles may manifest as missing logs. If invalid, a separately reviewed logger adjustment must retain configuration while reacquiring request-valid endpoints; do not simply ignore logger errors.
2. **Native handles:** the spec deliberately avoids caching store handles. No guarantee that all native resources survive request boundaries is assumed.
3. **Spin host/interface compatibility:** verify the emitted P3 component and installed runtime together, not only the Rust target triple or CLI version.
4. **Overlap:** verify Cloudflare and Spin callbacks actually overlap in the test, then prove isolation. A serialized test cannot establish concurrent safety.

These are validation gates, not permission to silently broaden implementation scope. Unsupported environments must be reported as unsupported or unverified rather than counted as passing.

## 8. Files and documentation surfaces

Expected production changes:

- `crates/edgezero-adapter-fastly/src/lib.rs`: SDK exports, two opt-in standard lifecycle helpers, and a small private feature-independent lifecycle state/helper with colocated host tests. This may remain in `lib.rs`; split a private module only if needed for clarity.
- `crates/edgezero-adapter-cloudflare/src/lib.rs`: explicit-metadata prebuilt dispatcher.
- `crates/edgezero-adapter-cloudflare/src/response.rs`: the duplicate-header correction described in §6.1.
- `crates/edgezero-adapter-spin/src/lib.rs`: explicit-metadata prebuilt dispatcher.
- Existing request modules: reuse registry logic; change only if necessary to share it without duplication.
- `crates/edgezero-core/src/app.rs` or existing core tests: compile-time retained-object bounds and shared-state contract coverage; no new core production abstraction.

Documentation changes during implementation:

- `docs/guide/adapters/overview.md`: lifecycle matrix, retained versus request state, common opt-in contract.
- `docs/guide/adapters/fastly.md`: default and reusable `main`, SDK limits, summary/error handling, standard and custom streaming paths, evidence limitations.
- `docs/guide/adapters/cloudflare.md`: concrete app cache, fresh `Env`/`Context`, concurrent fetch isolation, cold initialization.
- `docs/guide/adapters/spin.md`: SDK6/P3 distinction, concrete app cache, host reuse controls and concurrency, buffering behavior.
- `docs/guide/adapters/axum.md`: existing retained-router behavior and concurrent shared state.
- `examples/app-demo/crates/app-demo-core/src/lib.rs`: qualify fresh-Fastly-instance language as default behavior.

Keep generated templates and default demo entry points unchanged. Add opt-in examples as dedicated fixtures under `tests/fixtures/reusable-app/` with a standalone Cargo workspace, per-provider packages/configuration, and no production credentials. Add `scripts/smoke_test_reusable_app.sh` to build/run selected local providers, collect results, and report explicit skips. Record exact fixture paths in the implementation plan after confirming provider build-tool requirements.

The current spec is the only file authorized for this design step; the listed implementation files are proposed work.

## 9. Validation design

### 9.1 Framework tests

#### Host logic versus provider execution

The current Fastly crate does not link native host tests when its `fastly` feature is enabled. Verification with `cargo test -p edgezero-adapter-fastly --features fastly --lib --offline` failed on arm64 with unresolved Fastly hostcalls, including `_uri_get`. This is pre-existing: the default workspace test gate omits that feature, while the feature-enabled `cargo check` gate type-checks without linking. Do not require native feature-enabled tests or add fake hostcall symbols to make them pass.

Factor the initialization/retention state used by `serve_app` into a small private helper compiled under `cfg(any(feature = "fastly", test))`, with no dependency on Fastly SDK types or hostcalls. Its production wrapper supplies runtime configuration, logging/build operations, and SDK dispatch. Host tests must exercise the same state transitions used in production, with counted injected operations or a counted real core app builder; a separate model that merely repeats the expected decisions is insufficient. Keep this extraction private and local to the Fastly adapter; no public lifecycle abstraction is needed.

Assign verification explicitly:

- **Default host tests:** retained build/configure counts, first-initialization decisions, logger ownership/order through injected operations, degraded-then-successful configuration inputs preserving the first logging snapshot, terminal initialization errors, and fresh-owner reset. Pure core tests cover extension/state behavior without provider resources. Run `cargo test -p edgezero-adapter-fastly --lib` for the adapter logic.
- **Feature-enabled checks:** type-check SDK wrapper integration and callback bounds, including target-specific checks. These checks do not prove runtime behavior.
- **WASM/local provider fixtures (§9.2):** actual callback/build counts in reused instances, runtime-store failure/recovery where supported by controlled fixtures, real named-endpoint log delivery, native metadata/handles, stream completion, SDK summary counts/termination, and all other hostcall behavior. Fault injection proves only the injected case; unavailable provider failure modes must be reported separately.

The requirements below span these layers. They do not require provider calls inside native colocated tests. The warning about feature-disabled no-op stubs excludes claims about provider behavior, not valid tests of shared feature-independent production logic.

- Count builds/configure invocations: existing default paths build every invocation; retained paths build once per owner/instance.
- Prove a concrete cache cannot return another application's router. No generic unkeyed static may appear in production or examples.
- Verify fresh extensions, route parameters, headers, bodies, request IDs, and native metadata on successive requests.
- Include same-type app/request extension collisions and intentionally shared `Arc` mutation, distinguishing intended sharing from leakage.
- Verify store metadata, named/default bindings, runtime selectors, missing-store behavior, and recovery after a request-specific binding failure.
- Verify logger ownership, ordering, first-initialization policy, and Fastly named-endpoint output on subsequent requests. Include a degraded first runtime-store read followed by a successful read: selectors recover while the logging snapshot remains disabled, as specified in §4.2.
- Verify standard errors and custom send outcomes; initialization errors, constructor panic, pre-commit errors, post-commit stream errors, and later fresh initialization are separate cases.
- Keep existing duplicate `Set-Cookie` and body tests. Add actual delayed-chunk/first-byte tests for paths that support progressive client streaming.
- Compile examples against the locked SDKs on their actual targets. Host tests with feature-disabled no-op stubs do not validate provider behavior.

### 9.2 Local runtime matrix

At investigation time PATH Viceroy is 0.17.0; Fastly CLI 15.1.0 reports Viceroy 0.21.0; Spin is 4.0.0. Record the exact executable used, SDK lockfile, build target, emitted HTTP interface, and configuration for every result.

The fixture runner must select and report its Viceroy executable explicitly. `edgezero serve --adapter fastly` invokes `fastly compute serve`, which may select a different Viceroy. Record the CLI-managed runtime separately; do not attribute standalone fixture results to the CLI path without running it. Different versions are valid separate test environments, not interchangeable evidence.

Before enabling `with_max_memory` in a local comparison, verify that the actual guest can obtain a successful memory snapshot. Both investigated Viceroy versions implement `get_heap_mib`, but source support does not replace this runtime preflight. If unavailable, report the metric and memory-limit test as unsupported and omit that optional limit from the reuse-performance comparison. Otherwise the SDK may stop after request one while `into_result()` remains successful. Keep a separate unsupported-measurement test to verify the SDK's conservative termination behavior.

Viceroy's versioned upstream tests demonstrate a multi-request implementation: [0.17.0](https://github.com/fastly/Viceroy/blob/v0.17.0/cli/tests/integration/reusable_sessions.rs#L7) and [0.21.0](https://github.com/fastly/Viceroy/blob/v0.21.0/cli/tests/integration/reusable_sandboxes.rs#L7). This makes local experiments plausible; it is not evidence that these EdgeZero changes have run successfully.

- **Fastly:** run A/B/C below, stream completion, duplicate cookies, repeated logging, limit/idle termination, and restart isolation.
- **Cloudflare:** use a local Workers runtime to compare per-fetch build versus retained concrete app; force overlapping requests with distinct inputs and a controlled await barrier. Restart to validate cold initialization. Record availability/version rather than assuming the runtime exists.
- **Spin:** run the SDK6/P3 fixture on a compatible local host, with both sequential and concurrent instance reuse. Exercise supported reuse-count/concurrency/idle controls and record chosen values.
- **Axum:** verify one build per server start and independent overlapping requests against the shared router. Use it as a retained-app reference, not as a proxy for WASM performance.

A sequential request sequence alone does not prove reuse: observe a stable instance identifier and an increasing request ordinal. A runtime process ID alone is insufficient because one host process can create many guest instances.

### 9.3 Fastly A/B/C experiment

| Variant | Serving lifecycle                   | Initialization                                                    |
| ------- | ----------------------------------- | ----------------------------------------------------------------- |
| A       | Existing single-request entry point | Current per-request app construction and logging policy           |
| B       | SDK `Serve`                         | App construction on every callback; global logging installed once |
| C       | SDK `Serve`                         | App construction and global logging once per sandbox              |

B's logging qualification is mandatory: blindly wrapping the enabled-logger `run_app` would fail on its second installation and confound the comparison. Test that failure separately as a negative control. Keep all other dispatch, configuration, response, and workload choices identical between variants. Test standard dispatch and custom progressive streaming as separate comparable workloads; never compare buffered A against streaming C.

Implement B through the custom path in §4.3: `Serve::run`, a once-only logging initialization decision, per-callback `runtime_env_config` and `A::build_app()`, then `request::dispatch_with_registries`. Do not use retaining `serve_app` for B. Apply the same first-read logging policy in B and C so degraded configuration does not introduce a second experimental variable.

Collect:

- Sandbox start count, request ordinal, app/configure initialization count, and separately instrumented application initialization phases.
- Attempted requests per sandbox, including one-request sandboxes and early exits. Record response commitment, guest-side completion, errors, and client-observed completion separately; do not equate the SDK attempt count with successful responses. Emit per-request records because crashes may prevent a final summary.
- Client time to first byte and completion latency, with cold versus reused distributions and p50/p95/p99 plus sample counts.
- Initialization/handler wall time and CPU observations. SDK summary wall time is not CPU time; returned-response sending can occur outside callback timing.
- SDK CPU phase deltas where available. The pinned SDK warns against cross-run benchmarking with its vCPU clock; corroborate comparisons with controlled profiling or platform telemetry and report precision/unsupported metrics.
- Memory before initialization, after initialization, and after each request's cleanup; request-ordinal slope, peak, and plateau. Distinguish guest linear-memory high-water marks, SDK host-inclusive snapshots, and host process RSS. MiB rounding cannot prove absence of small leaks.

Use fixed release builds, deterministic payloads, controlled local backends, identical instrumentation, repeated runs, and randomized variant order where practical. Include small requests, expensive construction, delayed/large streams, failures, long sequences, idle gaps, and concurrency. Do not set a performance percentage target without baseline data. Adoption requires a reproducible benefit for the target workload with no correctness regression and bounded retained-state growth.

Include a repeated-origin control and a many-distinct-authorized-origin proxy workload, recording latency, registration failures or waits, attempted requests per sandbox, completion outcomes, and memory. Local runtime behavior does not establish deployed service-wide capacity behavior. Include malformed-request attempts and injected inbound/body-conversion failures; distinguish requests rejected by the host before dispatch from errors actually reaching the adapter. Verify the specified termination/recovery behavior without labelling a hypothetical malformed request as a demonstrated exploit.

### 9.4 Deployed evidence

Local results establish only the tested local runtime behavior. Platform reuse frequency, eviction, endpoint-handle validity, resource accounting, and performance require separately authorized deployed validation. Record these as unverified until measured. No configured request limit is a guaranteed reuse count.

### 9.5 Repository checks

After implementation, run scoped `cargo test` after code changes and all required gates:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
cargo check -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Also build and run the dedicated provider fixtures. Run documentation formatting/lint and the VitePress build when public guide changes are made. This design-document-only change does not claim any Rust test execution.

## 10. Acceptance and delivery

The implementation is acceptable when:

- Existing generated/default entry points retain their behavior.
- Fastly standard apps can opt into a bounded SDK serving lifecycle with exactly one successful app initialization per sandbox.
- Custom Fastly callbacks can retain their own state, mutate raw requests, preserve response extensions, explicitly stream, and complete post-send work.
- Cloudflare and Spin can use concrete retained apps with full metadata-aware per-request dispatch and tested overlapping-request isolation.
- Axum's existing retained-router behavior remains intact.
- Fresh initialization, failure recovery, bounded state, duplicate headers, and existing response semantics are validated.
- Native handle and logger support claims are limited to authoritative evidence and tested environments.
- A/B/C measurements distinguish lifecycle reuse from retained application initialization and do not claim automatic removal of every per-request cost.
- All required build/test/documentation checks pass, or unavailable runtime evidence is explicitly reported without claiming that acceptance criterion passed.

Suggested implementation sequence: public prebuilt dispatch and core contracts; Fastly lifecycle; provider fixtures and experiments; guides and examples. Each stage preserves defaults. No cross-provider cache abstraction is required to deliver this design.

Rollback is an entry-point choice: return to the existing `run_app` path and remove the concrete retained-app cache. On Fastly, also restore the single-request entry point. Test the rollback fixture rather than assuming a lower request limit recreates every aspect of the old initialization path. No persisted data migration is part of this feature.

## 11. Review decisions

Independent reviews checked the adapter/core design and a custom-dispatch integration. The selected approach incorporates these corrections:

- App ownership is portable, but scheduling and concurrency remain provider-specific.
- Prebuilt dispatch needs explicit store metadata because `App` does not contain it.
- Concrete application-owned caches avoid generic-static cross-application contamination.
- Mutable native-request handling and response-extension finalization require the raw callback path.
- Progressive client streaming differs from collecting a stream into a response body.
- The current Spin SDK's exported HTTP interface determines reuse semantics, not the Rust target name.

A subsequent source review clarified degraded logging snapshots, terminal adapter errors versus rendered application errors, service-wide backend pressure, callback reborrowing, and runtime measurement preflights. These clarifications retain the original opt-in and error-policy boundaries; no blanket error swallowing or automatic logging retry was added.

Rejected alternatives: documentation-only Fastly lifecycle repeats fragile logger ordering in every standard app; a universal server builder duplicates provider controls; adding metadata/cache management to core expands scope; silently caching inside existing helpers breaks lifetime compatibility.
