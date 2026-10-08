# Opt-in reusable application lifecycle

- **Status:** Historical lifecycle design; reconciled with PR #275's outbound hard-cut.
- **Date:** 2026-09-15
- **Source baseline:** EdgeZero `593fc9282a1c56e12bae15f91eef2162f4b6a1b7`.
- **Scope:** Core application ownership and the Fastly, Cloudflare, Spin, and Axum adapter surfaces, including custom dispatch, examples, documentation, and validation.
- **Evidence:** Host contracts and local provider fixtures; deployed lifetimes and application performance remain unverified.

The original private-helper design in §§8/9.1 is superseded by
[the custom serving lifecycle plan](../plans/2026-09-17-custom-serving-lifecycle.md).
The implemented `lifecycle::{Sandbox, serve_custom, run_custom}` surface, including
its observation counters, is public. No provider-independent scheduler is added.

PR #275 supersedes the original assembly, raw-conversion, response-returning, and
buffered-delivery choices below. The
[outbound HTTP design](2026-05-21-outbound-http-design.md) is authoritative for
admission, outbound execution, response egress, and capability semantics. The API
sections here describe the current production interfaces; historical verification
records do not establish that the reconciled implementation has been reverified.

### Included compatibility and CLI changes

The following focused changes accompany retention and apply to existing users:

- Cloudflare preserves repeated response headers, including every `Set-Cookie`;
  its first application value replaces a body-generated default.
- `config push` and `config diff` limit adapter-specific checks to the selected
  adapter, case-insensitively. Shared schema and secret-presence checks remain;
  `config validate --strict` checks portability across declared registered adapters.
- Spin CLI secret-reference validation reports fields and rules without including
  secret-reference values. This is one of the CLI diagnostic scope exceptions
  listed under Non-goals (§3) and does not change runtime error policy.

## 1. Objective

Allow applications to amortize app/router construction across requests when the host supports reuse. Separate retained initialization from request setup without changing existing entry-point defaults or moving application policy into the framework.

There are two independent operations:

1. A host permits an instance to receive another request.
2. Application code retains initialized objects rather than rebuilding them when that request arrives.

Enabling the first does not automatically implement the second. Retaining the router also does not eliminate configuration reads, request conversion, registry construction, signing, parsing, or other work that application handlers still perform explicitly.

Correctness must tolerate fresh initialization on any request. Retention is an optimization within one process, sandbox, isolate, or component instance, not durable storage or a routing-affinity guarantee.

## 2. Verified baseline

The source baseline above records the historical investigation. These API descriptions
are reconciled with the current production implementations rather than that baseline.

| Surface                  | Current behavior                                                                                                                                       | Reference                                                                          |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| Core construction        | `App::build::<A>(platform)` installs platform metadata, constructs routes, and invokes fallible `Hooks::configure`; only success yields an app.          | `crates/edgezero-core/src/app.rs`                                                  |
| Core dispatch            | `RouterService` owns `Arc<RouterInner>` and supports repeated `oneshot(&self, request)`.                                                               | `crates/edgezero-core/src/router.rs:298`, `:349`                                   |
| State                    | Registered state is cloned into each request; shared interiors remain shared. App state overwrites extensions of the same type.                        | `crates/edgezero-core/src/router.rs:212`, `:251`                                   |
| Macro                    | `app!` generates `Hooks` and router construction. Its state expression executes when the router is built. It does not generate the provider lifecycle. | `crates/edgezero-macros/src/app.rs:214`, `:229`                                    |
| Fastly                   | `run_app_with_hooks` builds per invocation; `serve_app_with_hooks` retains only successful construction through `Sandbox<App>`.                           | `crates/edgezero-adapter-fastly/src/lib.rs`                                        |
| Fastly logger            | Installing the process-global logger a second time returns an error.                                                                                   | `crates/edgezero-adapter-fastly/src/logger.rs:52`                                  |
| Fastly prebuilt dispatch | `request::send_request_with_registries_and_hooks` performs canonical admission and owned egress; no native response is returned for another sender.        | `crates/edgezero-adapter-fastly/src/request.rs`                                    |
| Cloudflare               | `run_app` builds per fetch and delegates to public `dispatch_app::<A>(&app, req, env, ctx)`.                                                               | `crates/edgezero-adapter-cloudflare/src/lib.rs`                                    |
| Spin                     | `run_app` builds per invocation and delegates to public `dispatch_app::<A>(&app, req)`, returning raw `SpinResponse`.                                      | `crates/edgezero-adapter-spin/src/lib.rs`                                          |
| Cloudflare/Spin logging  | Both adapter logger initializers are currently no-ops. Their existing entry points honor `owns_logging`.                                               | Cloudflare `src/lib.rs:30`; Spin `src/lib.rs:65`                                   |
| Axum                     | `run_app` builds the app once before starting the server, then shares the router through cloned services.                                              | `crates/edgezero-adapter-axum/src/dev_server.rs:352`, `:302`; `src/service.rs:132` |

### 2.1 Host lifecycle distinctions

- **Fastly:** reuse is opt-in through an ordinary `main` and SDK `Serve`. A sandbox processes requests sequentially. Lifecycle limits are checked between requests; the platform may end a sandbox earlier.
- **Cloudflare:** an isolate can handle overlapping requests on its event loop. No application loop requests the next fetch. Retention must tolerate eviction and must not associate the first request's bindings or context with subsequent requests.
- **Spin:** the current pinned SDK 7 HTTP macro exports WASI P3. The Rust `wasm32-wasip2` build target does not establish a P2 HTTP lifecycle. Verify the emitted interface and host together; retained state must support overlapping invocations when the host permits them.
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
- Preserve canonical admission and adapter-owned lazy response egress for both default and retained dispatch.
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
- Retention introduces no response buffer or stream-collection cap. Standard and custom admitted dispatch use the authoritative outbound design's lazy response-egress contract. The header-preservation and CLI diagnostic corrections listed above remain historical scope exceptions.
- No durable cache, automatic configuration refresh, parsed-key cache, or application-specific finalization hook.
- No deployment, publishing, or external issue/PR activity as part of implementing this spec.

## 4. API design

### 4.1 Preserve core ownership

Use `edgezero_core::app::{App, Hooks, StoresMetadata}` and core `Extensions`.
`App::build::<A>(PLATFORM) -> Result<App, EdgeError>` is the canonical assembly
path. It installs target metadata before fallible `Hooks::configure`, propagates
its typed failure, and rejects a successful configuration that changes platform
metadata. Retain only a completed successful app; initialization errors are never
cached. Lifecycle counters and store metadata do not belong in `App`.

`App` is structurally `Send + Sync`: handlers and middleware require these bounds, registered state requires them, and the router is reference-counted. Dispatch futures may remain non-`Send`. Add a compile-time assertion protecting the retained object's bounds without imposing `Send` on WASM futures.

### 4.2 Fastly standard lifecycle

Add these exports in `edgezero-adapter-fastly`, under its existing `fastly` gate:

```rust,ignore
pub use fastly::http::serve::{Serve, ServeSummary};

pub fn serve_app<A: Hooks>(serve: Serve)
    -> ServeSummary<fastly::Error>;

pub fn serve_app_with_hooks<A, Prepare, Finalize, State>(
    serve: Serve,
    prepare: Prepare,
    finalize: Finalize,
) -> ServeSummary<fastly::Error>
where
    A: Hooks,
    Prepare: FnMut(&mut fastly::Request, &mut Extensions),
    Finalize: FnMut(&mut edgezero_core::Response) -> State;
```

`serve_app` delegates with empty preparation and finalization callbacks. Both helpers:

1. Capture static `A::stores()` metadata.
2. Enter the SDK callback before performing host-dependent initialization.
3. Read runtime configuration for the current request.
4. Complete first-snapshot logging setup through `Sandbox::setup_once`, honoring `A::owns_logging()`, then initialize with `App::build::<A>(FASTLY_PLATFORM)` through `Sandbox::initialize`.
5. Retain the successfully constructed app for subsequent callbacks.
6. Use the first runtime configuration read for the first dispatch; do not read it twice.
7. On every request, use `request::send_request_with_registries_and_hooks` with fresh registries, admission, and response-egress resources. Preparation borrows the native request before admission; finalization borrows routed responses before egress policy and framing. Detached responses skip finalization.
8. Return the SDK summary unchanged. Do not hide handler errors or synthesize a guaranteed request count.

The logger's enablement, level, endpoint, and stdout policy are a first-initialization snapshot, even if runtime selectors later change. Store selector reads remain per request. Preserve the existing `FastlyLogging::from(&EnvConfig)` mapping: it derives enablement from endpoint presence and currently fixes `echo_stdout` to true rather than applying the corresponding runtime override. This feature does not change that mapping. `owns_logging` retains its existing meaning: skip adapter logger installation. An application adopting retained construction must arrange its own logger installation before construction if construction needs logging.

**Degraded-first-read policy:** preserve the existing optional-store fallback and freeze the resulting logging decision for this sandbox. `runtime_env_config` does not distinguish an absent optional store from another store-open failure; both produce empty configuration. That disables named-endpoint logging for the sandbox even if a later runtime read succeeds. The `echo_stdout` value does not provide a fallback when no logger is installed. Later reads can restore store selectors but must not silently reconfigure the global logger. Test this explicitly. Applications requiring stricter logging availability must use the custom lifecycle with their own configuration-read and initialization policy. Adding a distinguishable read status, deferred logging initialization, or retry policy would require a separately reviewed API/ordering change; it is not implied by these helpers.

Capture preparation and finalization callbacks in the serving closure and pass
fresh mutable reborrows on every request. Do not move either callback out on the
first invocation or retain request-owned values in its captures.

An initialization `Err` leaves `Sandbox` empty. Standard serving propagates that
error and terminates; custom callbacks may handle a recoverable failure and retry
on a later request. Configuration is fallible, and panics remain sandbox failures.
Partial global setup side effects are not rolled back, so custom retry policy must
make setup safe to repeat.

Example entry point after implementation:

```rust,ignore
use edgezero_adapter_fastly::{Serve, serve_app};

fn main() -> Result<(), fastly::Error> {
    serve_app::<MyApp>(Serve::new().with_max_requests(10)).into_result()
}
```

The example deliberately uses ordinary `main`, not `#[fastly::main]`. Existing generated entry points remain unchanged.

### 4.3 Fastly custom lifecycle

Use `lifecycle::serve_custom(Serve, callback)` with
`FnMut(fastly::Request, &mut Sandbox<T>) -> Result<(), E>`. The callback owns
terminal delivery. The wrapper records that result through the SDK without asking
the SDK to send a response or synthesize an error response.

A callback receives an owned native request and may use
`request::send_request_with_registries_and_hooks` with a retained app, current
runtime configuration, and request-local hooks. Preparation can mutate the native
request and populate extensions; finalization can inspect or mutate routed core
responses before policy/framing. The adapter retains transmission ownership and
streams lazily through terminal delivery; no public raw-conversion or
response-returning dispatch helper is part of this path.

Applications may capture retained state in a closure or pass it through `run_with_context`. Host-dependent or fallible initialization belongs inside the callback. A lightweight health response may bypass expensive initialization; retaining state must not require eager construction before every callback can run.

Resolve `runtime_env_config` per callback, initialize logging before emitting its
deferred boot diagnostics, and pass fresh store registries through the canonical
send-owning path. Do not manually route a converted request and collect its body;
that would bypass admission or duplicate response-egress ownership.

The custom lifecycle uses `lifecycle::Sandbox<T>`: an application-owned payload in a framework-owned successful-only slot, attempted-callback and initialization counters, and an independent successful setup guard. `initialize` returns errors unchanged and leaves the slot empty so a later call can retry; applications decide whether to respond and continue. `run_custom` creates fresh state for a single received request without entering `Serve`. Both wrappers record terminal delivery exactly once without SDK resends. State decision logic is feature-independent for host tests; SDK wrappers require `fastly`. Default generated entry points remain unchanged.

### 4.4 Cloudflare and Spin prebuilt dispatch

Public root-level dispatch helpers select metadata from the caller's `Hooks` type:

```rust,ignore
// edgezero-adapter-cloudflare
pub async fn dispatch_app<A: Hooks>(
    app: &App,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error>;

// edgezero-adapter-spin
pub async fn dispatch_app<A: Hooks>(
    app: &App,
    req: spin_sdk::http::Request,
) -> anyhow::Result<edgezero_adapter_spin::SpinResponse>;
```

Use the same feature and target gates as each adapter's existing `run_app`.

These functions do not build an app, install logging, or cache anything. They resolve runtime configuration for that invocation, build registries using `A::stores()`, and run fresh admission and response egress through the internal registry-aware dispatcher. Callers must supply an app constructed from `A` with `CLOUDFLARE_PLATFORM` or the selected Spin platform; `App` erases its type, so the function cannot verify provenance. Spin returns the raw WASI response; its coordinator retains the body writer and transmission-result observation.

For Cloudflare, derive `EnvConfig` before moving `env` into the dispatcher and keep the configuration alive through the awaited call that borrows it in `RegistryInputs`. For Spin, unpack `stores.config`, `stores.kv`, and `stores.secrets` into the existing dispatcher's separate metadata arguments. These are adapter wrappers, not changes to internal dispatcher signatures.

The explicit `Hooks` type avoids silently losing store wiring without pretending `&App` contains its construction metadata. Logging is excluded intentionally: a dispatcher must be safe to call repeatedly, and the caller controls initialization before constructing the app.

Existing `run_app` delegates its dispatch portion to the new function, preserving logger-before-build ordering, environment-resolution behavior, error behavior, and exactly one build per call.

Do not route these helpers through the manual Cloudflare builder or Spin `AppExt::dispatch`: those paths do not preserve all metadata-aware behavior.

### 4.5 Concrete retention on Cloudflare and Spin

Document a static owned by the concrete application entry-point module:

```rust,ignore
use edgezero_adapter_cloudflare::CLOUDFLARE_PLATFORM;
use edgezero_core::{app::App, EdgeError};
use std::sync::OnceLock;

static APP: OnceLock<App> = OnceLock::new();

fn retained_app() -> Result<&'static App, EdgeError> {
    if let Some(app) = APP.get() {
        return Ok(app);
    }
    let app = App::build::<MyApp>(CLOUDFLARE_PLATFORM)?;
    Ok(APP.get_or_init(|| app))
}
```

The fetch callback obtains this reference synchronously, maps initialization
failure to a category-safe Worker error, and passes the successful app and current
request arguments to `dispatch_app::<MyApp>`. For Spin, use `SPIN_PLATFORM` or
metadata for the selected host and return `anyhow::Result<SpinResponse>` from the
HTTP callback. A failed build leaves the cache empty. Concurrent attempts may
construct multiple successful candidates; only one is retained.

Requirements for the example and API documentation:

- The cache belongs to one concrete application. Never put an unkeyed static inside a generic `cached<A>()`; such statics are shared across type instantiations.
- Perform any application-owned logging initialization before `App::build` if construction logs. Adapter logging is a no-op on these two targets; do not invent a new logger implementation in this change.
- The initializer is synchronous and must not recursively access the same cache. Do not block the event loop waiting on an async initializer or hold mutex/borrow guards across dispatch awaits.
- Retain only the app, not a future, native request, Worker `Env`/`Context`, or store registry.
- Binding/open failures occur during dispatch and are retried on a later request through normal request setup; they must not poison the app cache.
- The example propagates fallible synchronous construction without caching errors. Applications needing asynchronous initialization, serialized construction attempts, or refresh manage that owner and publish a complete successful snapshot before sharing it. No generic framework failure cache is added.
- App middleware and shared state now survive across requests and can be accessed by overlapping invocations. The application must consent to that changed lifetime by explicitly adopting the example.

### 4.6 Axum

Keep the existing construction and service ownership. Include Axum in the shared-state contract tests and documentation matrix, but add no reuse flag or alternate lifecycle helper.

This does not claim that all Axum per-request costs have already been optimized;
request-local outbound wiring and admission/egress resources remain necessary.

## 5. Fastly limits and termination

Expose SDK limit configuration without translating or duplicating it:

| SDK setting                   | Semantics to document                                                                                                                                             |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `with_max_requests(usize)`    | Positive values bound requests; zero means no SDK request-count limit. The example uses an explicit finite value.                                                 |
| `with_max_lifetime(Duration)` | Checked between requests, measured from `Serve` construction. Not an interrupt deadline for a running handler.                                                    |
| `with_max_memory(u32)`        | MiB snapshot threshold checked between requests; zero disables this SDK threshold. An unavailable measurement stops further reuse conservatively when configured. |
| `with_timeout(Duration)`      | Bounds waiting for another request, subject to platform limits. Does not set application/backend request timeout.                                                 |

The implementation must follow the pinned SDK's exact comparisons rather than introducing alternative threshold semantics. The platform may enforce additional limits, and request CPU/runtime limits still apply per request.

The pinned SDK stops after a handler error. EdgeZero's serving wrappers use a
delivery-result adapter that records `Result<(), E>` without invoking another
send; an escaping error terminates serving without an SDK-generated 500. Direct
SDK result handlers have a different sending contract and are not the canonical
EdgeZero entry path.

Distinguish a terminal delivery error from an application error rendered as an
HTTP response. Canonical app dispatch renders routing/handler errors through the
configured error renderer, and an HTTP 4xx/5xx alone does not stop reuse. Admission
refusals and bounded precommit fallbacks follow the authoritative outbound design;
they are not escaping SDK errors merely because they report failure status. An
error that escapes owned delivery terminates standard serving without a second
response attempt. Test rendered errors, admission aborts, and terminal delivery
failures separately. Custom callbacks may handle a recoverable initialization
failure before commitment; recovery after an escaping error requires a fresh
sandbox. Cloudflare/Spin resolve bindings again on later requests without caching
initialization failures.

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
| Fastly standard | Duplicate headers are appended; the adapter owns commitment, lazy chunk writes, terminal observation, and cleanup.                             |
| Fastly custom   | Request preparation and routed-response finalization remain possible through borrowing hooks; canonical delivery stays adapter-owned.           |
| Cloudflare      | A request-local JavaScript stream coordinator registered with `Context::wait_until` owns backpressure, deadline/abort handling, and close.       |
| Spin            | A raw `SpinResponse` is backed by an owned WASI body/transmission coordinator; no whole-response collection or historical 16 MiB stream cap.      |
| Axum            | A connection-local Hyper body preserves lazy production and terminal response-egress observation.                                               |

Inbound bodies are consumed only after canonical admission, under the selected
limits and absolute deadline. Lazy downstream delivery does not imply unbounded
uploads, remove provider queues, or eliminate per-request body costs.

### 6.2 Explicit-send error ordering

Custom Fastly callbacks return terminal `Result<(), E>` after owned delivery.
They must finish or abandon guest-owned streaming resources before returning.
The lifecycle wrapper records that result without another send.

Canonical response egress decides the bounded fallback before commitment and
never replaces a committed response. After commitment, a body failure follows
adapter-owned cleanup and terminal reporting. An escaping `Err` from an EdgeZero
lifecycle callback terminates serving without an SDK resend. Applications must
not hand a native response to a second SDK sender or bypass this path with raw
conversion/manual body collection.

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

Audit result: core's recursion depth is thread-local and guarded by `SecretFieldsRecursionGuard`. Tests verify normal scope cleanup and host unwinding followed by fresh entry; host unwinding does not prove cleanup after a terminating WASM trap. Canonical-form instrumentation and environment locks are test-only. Fastly's missing-store warning sets are bounded, and their eviction tests cover repeated names. No request data belongs in these caches.

**Dynamic backends:** Fastly outbound execution registers backends using
URI-derived identity and adapter policy; the outbound design owns naming,
timeout, and dispatch rules. Distinct origins can increase registration
diversity. Do not model service-wide capacity as a counter reset by app or
sandbox reconstruction. Use bounded authorized origin sets and measure repeated
and distinct origins, registration failures/waits, and memory. A between-request
lifecycle limit cannot preempt a blocked hostcall or bound fan-out within one
request. See [registration scope](https://www.fastly.com/documentation/guides/integrations/non-fastly-services/developer-guide-backends/)
and [dynamic-backend limit behavior](https://www.fastly.com/documentation/reference/compute/errors).

Fastly environment variables describe the sandbox: `FASTLY_TRACE_ID` must not be treated as a unique request identifier in reusable mode. Capture `Request::get_client_request_id()` separately for request correlation, with a documented fallback when unavailable. Do not retain request correlation fields in the global logger or app state.

## 7. Compatibility and unresolved platform evidence

- Existing Fastly, Cloudflare, and Spin `run_app` calls continue building each invocation; generated templates continue calling them.
- PR #275 supersedes historical raw conversion and response-returning custom dispatch. Fastly uses `Sandbox`, `serve_app`, `serve_app_with_hooks`, and canonical send-owning request hooks; no compatibility aliases restore removed APIs.
- `Hooks::configure` is fallible and `App::build::<A>(PLATFORM)` is the canonical constructor. Retention does not change macro syntax, registered-state precedence, or provider feature gates.
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

- `crates/edgezero-adapter-fastly/src/lib.rs`: SDK exports and opt-in standard lifecycle helpers; public feature-independent `lifecycle::Sandbox<T>` plus `serve_custom`/`run_custom` wrappers live in `src/lifecycle.rs` with host tests (superseding the original private helper).
- `crates/edgezero-adapter-cloudflare/src/lib.rs`: `Hooks`-typed prebuilt dispatcher shared with `run_app`.
- `crates/edgezero-adapter-cloudflare/src/response.rs`: the duplicate-header correction described in §6.1.
- `crates/edgezero-adapter-spin/src/lib.rs`: `Hooks`-typed prebuilt dispatcher shared with `run_app`.
- Fastly `src/request.rs`: canonical admitted, send-owning dispatch with request preparation and routed-response finalization hooks.
- CLI `src/config.rs` and Spin `src/cli.rs`: selected-adapter validation and diagnostic redaction listed above.
- `crates/edgezero-core/src/app.rs`: fallible platform-aware assembly and canonical admission/egress; retained-object bounds and shared-state contract coverage.

Documentation changes during implementation:

- `docs/guide/adapters/overview.md`: lifecycle matrix, retained versus request state, common opt-in contract.
- `docs/guide/adapters/fastly.md`: default and reusable `main`, SDK limits, summary/error handling, standard and custom streaming paths, evidence limitations.
- `docs/guide/adapters/cloudflare.md`: concrete app cache, fresh `Env`/`Context`, concurrent fetch isolation, cold initialization.
- `docs/guide/adapters/spin.md`: SDK7/P3 distinction, successful-only concrete app cache, host reuse controls and concurrency, raw response-egress ownership.
- `docs/guide/adapters/axum.md`: existing retained-router behavior and concurrent shared state.
- `examples/app-demo/crates/app-demo-core/src/lib.rs`: qualify fresh-Fastly-instance language as default behavior.

Keep generated templates and default demo entry points unchanged. Add opt-in examples as dedicated fixtures under `tests/fixtures/reusable-app/` with a standalone Cargo workspace, per-provider packages/configuration, and no production credentials. Add `scripts/smoke_test_reusable_app.sh` to build/run selected local providers, collect results, and report explicit skips. Record exact fixture paths in the implementation plan after confirming provider build-tool requirements.

The listed surfaces are implemented; the original design-step authorization checkpoint has been superseded by implementation approval.

## 9. Validation design

### 9.1 Framework tests

#### Host logic versus provider execution

The current Fastly crate does not link native host tests when its `fastly` feature is enabled. Verification with `cargo test -p edgezero-adapter-fastly --features fastly --lib --offline` failed on arm64 with unresolved Fastly hostcalls, including `_uri_get`. This is pre-existing: the default workspace test gate omits that feature, while the feature-enabled `cargo check` gate type-checks without linking. Do not require native feature-enabled tests or add fake hostcall symbols to make them pass.

The original private-helper plan is superseded by [the custom serving lifecycle plan](../plans/2026-09-17-custom-serving-lifecycle.md) and PR #275's outbound hard-cut. Public `lifecycle::Sandbox<T>` contains feature-independent retention state with no Fastly SDK types or hostcalls; standard serving uses its independent setup and successful-app guards. The production wrapper supplies fresh runtime configuration, fallible platform-aware assembly, and canonical send-owning dispatch. Host tests must exercise those same transitions, not a duplicate model. Keep these mechanics local to Fastly; no cross-provider lifecycle abstraction is introduced.

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

Observed with the pinned standalone Viceroy 0.17.0: a guest admits at most six requests (one initial request plus five `next_request` accepts), so raising the SDK request limit or the fixture's `--max-requests` cannot extend that local ceiling.

- **Fastly:** run A/B/C below, stream completion, duplicate cookies, repeated logging, limit/idle termination, and restart isolation.
- **Cloudflare:** use a local Workers runtime to compare per-fetch build versus retained concrete app; force overlapping requests with distinct inputs and a controlled await barrier. Restart to validate cold initialization. Record availability/version rather than assuming the runtime exists.
- **Spin:** run the SDK7/P3 fixture on a compatible local host, with both sequential and concurrent instance reuse. Exercise supported reuse-count/concurrency/idle controls and record chosen values.
- **Axum:** verify one build per server start and independent overlapping requests against the shared router. Use it as a retained-app reference, not as a proxy for WASM performance.

A sequential request sequence alone does not prove reuse: observe a stable instance identifier and an increasing request ordinal. A runtime process ID alone is insufficient because one host process can create many guest instances.

### 9.3 Fastly A/B/C experiment

| Variant | Serving lifecycle                   | Initialization                                                    |
| ------- | ----------------------------------- | ----------------------------------------------------------------- |
| A       | Existing single-request entry point | Current per-request app construction and logging policy           |
| B       | SDK `Serve`                         | App construction on every callback; global logging installed once |
| C       | SDK `Serve`                         | App construction and global logging once per sandbox              |

B's logging qualification is mandatory: blindly wrapping the enabled-logger `run_app` would fail on its second installation and confound the comparison. Test that failure separately as a negative control. Keep dispatch, configuration, lazy delivery, and workloads identical between variants. Do not compare historical buffered helpers against current streamed delivery.

Implement B through `lifecycle::serve_custom`: a successful once-only logging
decision, per-callback `runtime_env_config` and `App::build::<A>(FASTLY_PLATFORM)`,
then `request::send_request_with_registries_and_hooks`. Do not use retaining
`serve_app` for B. Apply the same first-read logging policy in B and C so degraded
configuration does not introduce a second experimental variable.

Collect:

- Sandbox start count, request ordinal, app/configure initialization count, and separately instrumented application initialization phases.
- Attempted requests per sandbox, including one-request sandboxes and early exits. Record response commitment, guest-side completion, errors, and client-observed completion separately; do not equate the SDK attempt count with successful responses. Emit per-request records because crashes may prevent a final summary.
- Client time to first byte and completion latency, with cold versus reused distributions and p50/p95/p99 plus sample counts.
- Initialization/handler wall time and CPU observations. SDK summary wall time is not CPU time; current Fastly callbacks include adapter-owned terminal delivery rather than handing a response to another sender.
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

Also build and run the dedicated provider fixtures. Run documentation formatting/lint and the VitePress build when public guide changes are made. Implementation verification is recorded in the plans and PR; unsupported runtime evidence is kept explicit.

## 10. Acceptance and delivery

The implementation is acceptable when:

- Existing generated/default entry points retain their lifecycle defaults, with the separately listed compatibility and CLI corrections.
- Fastly standard apps can opt into a bounded SDK serving lifecycle with exactly one successful app initialization per sandbox.
- Custom Fastly callbacks can retain successful app state, prepare raw requests, inspect/finalize routed response extensions through hooks, and use canonical lazy delivery without another sender.
- Cloudflare and Spin can use concrete retained apps with full metadata-aware per-request dispatch and tested overlapping-request isolation.
- Axum's existing retained-router behavior remains intact.
- Fresh initialization, failure recovery, bounded state, duplicate headers, and existing response semantics are validated.
- Native handle and logger support claims are limited to authoritative evidence and tested environments.
- A/B/C measurements distinguish lifecycle reuse from retained application initialization and do not claim automatic removal of every per-request cost.
- All required build/test/documentation checks pass, or unavailable runtime evidence is explicitly reported without claiming that acceptance criterion passed.

Suggested implementation sequence: public prebuilt dispatch and core contracts; Fastly lifecycle; provider fixtures and experiments; guides and examples. Each stage preserves defaults. No cross-provider cache abstraction is required to deliver this design.

Rollback is an entry-point choice: return to the existing `run_app` path and remove the concrete retained-app cache. On Fastly, also restore the single-request entry point. The default Fastly A fixture exercises the original path; test it rather than assuming a lower request limit recreates every aspect of the old initialization path. No persisted data migration is part of this feature.

## 11. Review decisions

Independent reviews checked the adapter/core design and a custom-dispatch integration. The selected approach incorporates these corrections:

- App ownership is portable, but scheduling and concurrency remain provider-specific.
- Prebuilt dispatch derives store metadata from an explicit `Hooks` type because `App` does not contain it.
- Concrete application-owned caches avoid generic-static cross-application contamination.
- Mutable native-request handling and response-extension finalization require the raw callback path.
- Progressive client streaming differs from collecting a stream into a response body.
- The current Spin SDK's exported HTTP interface determines reuse semantics, not the Rust target name.

A subsequent source review clarified degraded logging snapshots, terminal adapter errors versus rendered application errors, service-wide backend pressure, callback reborrowing, and runtime measurement preflights. These clarifications retain the original opt-in and error-policy boundaries; no blanket error swallowing or automatic logging retry was added.

Rejected alternatives: documentation-only Fastly lifecycle repeats fragile logger ordering in every standard app; a universal server builder duplicates provider controls; adding metadata/cache management to core expands scope; silently caching inside existing helpers breaks lifetime compatibility.
