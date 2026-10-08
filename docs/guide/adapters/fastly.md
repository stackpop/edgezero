# Fastly Compute@Edge

Deploy EdgeZero applications to Fastly's Compute@Edge platform using WebAssembly.

## Prerequisites

- [Fastly CLI](https://developer.fastly.com/learning/compute/#install-the-fastly-cli)
- Rust `wasm32-wasip1` target: `rustup target add wasm32-wasip1`
- [Viceroy](https://github.com/fastly/Viceroy) for local execution and testing

## Project Setup

When scaffolding with `edgezero new my-app`, the Fastly adapter includes:

```
crates/my-app-adapter-fastly/
├── Cargo.toml
├── fastly.toml
└── src/
    └── main.rs
```

### fastly.toml

The Fastly manifest configures your service:

```toml
manifest_version = 3
name = "my-app"
language = "rust"
authors = ["you@example.com"]

[local_server]
  [local_server.backends]
    [local_server.backends."origin"]
    url = "https://your-origin.example.com"
```

`edgezero provision --adapter fastly` creates each declared store and writes a
`[setup.kv_stores]`, `[setup.secret_stores]` or `[setup.config_stores]` entry
into `fastly.toml`, keyed by the store's logical ID. It writes that entry only
when the physical store name equals the logical ID. A different physical name
needs a selected service (`service_id` in `fastly.toml` or `FASTLY_SERVICE_ID`),
and provision leaves `fastly.toml` unchanged for that store. Whenever a service
is selected, provision prints the `fastly service resource-link create` command
that links each new store under its logical ID, because an existing service does
not re-run `[setup]`. Provision leaves the Viceroy-only `[local_server.*]` tables
alone. The config-store stanzas there are written by your generated app CLI,
`<app-cli> config push --adapter fastly --local` (the bundled `edgezero` binary
has no typed app config and exits 2), and KV / secret local-server seeding is
hand-edited.

### Entrypoint

The Fastly entrypoint wires the adapter:

```rust
use my_app_core::App;

#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    edgezero_adapter_fastly::run_app::<App>(req)
}
```

`run_app` builds per-id `KV` / `Config` / `Secret` registries from the portable
store metadata baked into `App` by the `app!` macro. Logging settings are baked
in at the same time. A managed deployment resolves `EDGEZERO__*` store selectors
and links each selected Fastly resource under its logical ID; no
`edgezero.toml` or deployment environment is read by the runtime. See
[the migration guide](../manifest-store-migration.md).

For fully manual wiring, `FastlyService::new(&app)` builds a dispatcher one
store at a time: `.with_config(name)`, `.with_config_handle(handle)`,
`.with_kv(name)`, `.with_secrets()`, the matching `.require_kv()` /
`.require_secrets()` flags, and finally `.dispatch(req)`. Prefer `run_app` when
the entrypoint should use the store and logging metadata baked into the app.

### Migrating a custom entrypoint

Custom entrypoints previously loaded selectors from the
`edgezero_runtime_env` Config Store on every request and passed an `EnvConfig`
to `dispatch_with_registries`:

```rust
let stores = MyHooks::stores();
let env = edgezero_adapter_fastly::runtime_env_config(stores);
edgezero_adapter_fastly::request::dispatch_with_registries(
    &app,
    req,
    stores,
    &env,
    extend,
)
```

Remove the `runtime_env_config` call, any use of `RUNTIME_ENV_STORE_NAME`, and
the `env` argument. Initialize logging from the app's baked Fastly settings:

```rust
use edgezero_adapter_fastly::{FastlyLogging, init_logger};
use edgezero_core::app::Hooks as _;

let stores = MyHooks::stores();
let logging = FastlyLogging::from(MyHooks::logging_for("fastly"));
if logging.use_fastly_logger && !MyHooks::owns_logging() {
    let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
    init_logger(endpoint, logging.level, logging.echo_stdout)?;
}
edgezero_adapter_fastly::request::dispatch_with_registries(
    &app,
    req,
    stores,
    extend,
)
```

A hand-written `Hooks` impl inherits the default `stores()`, which is empty, so
`dispatch_with_registries` opens no stores. Such an impl must override
`stores()` or pass explicit `StoresMetadata`.

`EDGEZERO__STORES__<KIND>__<ID>__NAME` is now a deployment input. EdgeZero
resolves it before provider mutation and binds that physical resource to the
version under the stable logical `<ID>` alias. The runtime opens the alias
directly. Fastly Config Stores always use `<ID>` as the entry key, so remove
Fastly `__KEY` selectors as well.

Fastly logging endpoint, level, and stdout behavior now come from
`[adapters.fastly.logging]` when the immutable application is built. Runtime
`EDGEZERO__LOGGING__*` values cannot vary those settings between production and
staging for the same release.

### Capturing raw-request signals (JA4, H2 fingerprint)

`run_app` converts the `fastly::Request` into a neutral core request before
dispatch. The client IP is carried across automatically — read it via
`FastlyRequestContext` (see [Context Access](#context-access) below). Other
Fastly-only signals that are readable only on the raw request
(`get_tls_ja4()`, `get_client_h2_fingerprint()`) aren't reachable from handlers
by default. Use `run_app_with_request_extensions`, which
runs an app closure against a scratch `Extensions` **before** conversion and
merges the values into the core request — so a `State`/extractor or middleware
can read them:

```rust
#[derive(Clone)]
struct Ja4(String);

#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    edgezero_adapter_fastly::run_app_with_request_extensions::<App, _>(req, |raw, ext| {
        if let Some(ja4) = raw.get_tls_ja4() {
            ext.insert(Ja4(ja4.to_owned()));
        }
    })
}
```

`run_app` is exactly `run_app_with_request_extensions::<App, _>(req, |_, _| {})`.
The closure runs once per request; insert whatever typed values your handlers
need, then read them in a handler via a custom extractor or
`ctx.request().extensions().get::<Ja4>()`.

### Owning your own logging

By default `run_app` initializes the Fastly logger when the baked
`[adapters.fastly.logging]` table sets an `endpoint`. If your app already installs
a `log` backend, opt out with the platform-neutral `Hooks::owns_logging()` flag —
via the `app!` macro:

```rust
edgezero_core::app!("edgezero.toml", owns_logging = true);
```

or on a hand-written `Hooks` impl (`fn owns_logging() -> bool { true }`). Every
adapter's `run_app` honors it, so the app is responsible for logger setup.

## Building

Build for Fastly's Wasm target:

```bash
# Using the CLI
edgezero build --adapter fastly

# Or directly
fastly compute build -C crates/my-app-adapter-fastly
```

The compiled Wasm binary is placed in `target/wasm32-wasip1/release/`.

## Local Development

Run locally with Viceroy (Fastly's local simulator):

```bash
# Using the CLI
edgezero serve --adapter fastly

# Or directly
fastly compute serve -C crates/my-app-adapter-fastly
```

This starts a local server at `http://127.0.0.1:7676`.

## Deployment

Deploy a verified application release with the adapter-managed lifecycle:

```bash
EDGEZERO_MANIFEST="$RELEASE_ROOT/edgezero.toml" \
edgezero deploy --adapter fastly \
  --service-id "$FASTLY_SERVICE_ID" \
  --application-release "$RELEASE_ROOT"
```

The release fixes the package, `edgezero.toml`, and its referenced Fastly
manifest before runtime configuration is selected. A bare
`edgezero deploy --adapter fastly` remains only as store-free production
compatibility for an existing manifest command. Staging and any deployment that
declares a Config, KV, or Secret Store require the verified release-backed
managed command above.

## Backends

EdgeZero's Fastly proxy client uses **dynamic backends** derived from the target URI (host + scheme).
You do not need to predeclare backends in `fastly.toml` for EdgeZero proxying.

```rust
use edgezero_adapter_fastly::proxy::FastlyProxyClient;
use edgezero_core::proxy::ProxyService;

let client = FastlyProxyClient;
let response = ProxyService::new(client).forward(request).await?;
```

## Logging

Fastly uses endpoint-based logging. Configure logging in `edgezero.toml`:

```toml
[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true
```

To initialize logging manually, call `init_logger` with explicit settings:

```rust
use edgezero_adapter_fastly::init_logger;
use log::LevelFilter;

fn main() {
    init_logger("stdout", LevelFilter::Info, true).expect("init logger");
}
```

::: tip Logging status
Fastly logging is wired when you call `init_logger`, or when `run_app` finds an `endpoint` in the baked `[adapters.fastly.logging]` table and `owns_logging()` is `false`; otherwise no logger is installed.
:::

## Store selection and deployment

Fastly Compute applications open Config, KV, and Secret Stores by resource-link
name. EdgeZero bakes the logical IDs declared in `edgezero.toml` into the package.
A managed deployment resolves these optional deployment selectors:

```text
EDGEZERO__STORES__CONFIG__<ID>__NAME
EDGEZERO__STORES__KV__<ID>__NAME
EDGEZERO__STORES__SECRETS__<ID>__NAME
```

Each selected physical store is linked to the unpublished target version under
the stable logical `<ID>` alias. An absent `__NAME` defaults to `<ID>`; a present
blank or invalid value fails before provider mutation. The same logical ID can be
used independently by Config, KV, and Secret Stores because link identity is the
pair `(resource kind, logical ID)`.

Config keys are deterministic on Fastly: production, staging, and local Viceroy
all read `<ID>`. The selected Environment chooses the physical store through
`__NAME`; the same name shares config and different names isolate it. A
conflicting `__KEY` or `--key` fails before a write. Logging is resolved from
`[adapters.fastly.logging]` when the package is built.

Before publication, EdgeZero:

1. verifies the immutable release package and manifests;
2. resolves complete Config, KV, and Secret Store inventories;
3. selects the exact active, staged, or initialized-draft source;
4. uploads the verified package to an unreachable draft;
5. replaces declared links whose selected physical resource changed and creates
   missing declared links under their logical aliases;
6. preserves links not declared by the application;
7. re-reads the exact links, source state, draft state, and provider-visible
   package identity; and
8. stages or activates the prepared version without another EdgeZero mutation.

Production and staging can select different physical resources while deploying
identical package bytes. Secret Stores remain optional. Links the application
does not declare are preserved.

### Declaring and using stores

Declare portable logical IDs in `edgezero.toml`:

```toml
[stores.config]
ids = ["app_config"]

[stores.kv]
ids = ["cache"]

# Optional: omit this table when the app uses no Secret Store.
[stores.secrets]
ids = ["credentials"]
```

For local Viceroy tests, expose the Config Store under its logical ID and
write the production key under that store. Local Viceroy uses the production
key because it has no Fastly staging publication state:

```toml
[local_server.config_stores.app_config]
format = "inline-toml"

[local_server.config_stores.app_config.contents]
greeting = "hello from config store"
```

Handlers read values through the `Config` extractor or
`ctx.config_store(id)`:

```rust
async fn handler(config: Config) -> Result<Response, EdgeError> {
    let store = config
        .named("app_config")
        .ok_or_else(|| EdgeError::service_unavailable("no `app_config`"))?;
    let greeting = store.get("greeting").await?.unwrap_or_default();
    // …
}
```

See [the store migration guide](../manifest-store-migration.md) for store selection and [the GitHub Actions guide](../deploy-github-actions.md) for the
immutable-release workflow.

## Context Access

Access Fastly-specific APIs via the request context extensions:

```rust
use edgezero_core::context::RequestContext;
use edgezero_adapter_fastly::context::FastlyRequestContext;

async fn handler(ctx: RequestContext) -> Result<Response, EdgeError> {
    // Access Fastly context from extensions
    if let Some(fastly_ctx) = FastlyRequestContext::get(ctx.request()) {
        let client_ip = fastly_ctx.client_ip;
        // ...
    }

    // ...
}
```

## Streaming

Standard `run_app` and `serve_app` dispatch drain a `Body::Stream` response into
a `fastly::Body` before returning, so the full payload is materialised rather
than delivered chunk by chunk. Custom `lifecycle::serve_custom` callbacks can
instead commit a native streaming response and pump chunks progressively; see
[Custom dispatch and streaming](#custom-dispatch-and-streaming).

See the [Streaming guide](/guide/streaming) for examples and patterns.

## Testing

Run contract tests for the Fastly adapter:

```bash
cargo install viceroy --locked
export CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run"

# Run tests
cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --test contract
```

Fastly SDK-linked Wasm binaries require Viceroy for execution; plain Wasmtime
does not provide the `fastly_*` host imports needed by the adapter tests.

::: tip Local Execution
If Viceroy reports native certificate or keychain errors on macOS, use `--no-run`
locally and rely on Linux CI for execution.
:::

## Manifest Configuration

Configure the Fastly adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full manifest reference.

## Reusing a sandbox and retaining an app

Opt in with an ordinary `main` in place of the single-request `#[fastly::main]`:

```rust
use edgezero_adapter_fastly::{Serve, serve_app};
use std::time::Duration;

fn main() -> Result<(), fastly::Error> {
    serve_app::<MyApp>(
        Serve::new()
            .with_max_requests(10)
            .with_timeout(Duration::from_millis(500)),
    ).into_result()
}
```

`serve_app_with_request_extensions` additionally accepts an `FnMut` callback
for fresh extensions on every request. The first request initializes logging
and builds the app. Each request reads runtime configuration and constructs new
store registries. App construction and `configure` execute once per sandbox;
explicit work elsewhere still executes whenever the application calls it.
The extension callback and its captures are retained for the whole serving loop;
only the supplied extensions are fresh. Create request-specific mutable data
inside the callback or store it in those extensions.

Logging takes the first configuration snapshot, unless the app owns logging.
An unavailable optional runtime configuration store disables logging for that
sandbox, even if subsequent reads recover store selectors. It does not silently
retry or reconfigure logging. The current overlay enables logging when an
endpoint exists and uses `echo_stdout: true`. Applications requiring another
initialization policy should use `lifecycle::serve_custom` and `setup_once`.
The resolved configuration does not distinguish intentionally disabled logging
from an unavailable first snapshot. Warnings through the log facade may be lost
before a logger is installed; absence of a warning does not establish a healthy
configuration read.

SDK 0.12.1 exposes `with_max_requests`, `with_timeout`, `with_max_lifetime`, and
`with_max_memory`. A value of zero disables the request-count or memory limit;
it does not request zero callbacks or zero memory use. The lifetime limit is
measured from `Serve` construction, including time before the first callback.
`with_timeout` bounds only the idle wait for the next request; it does not bound
handler or backend time. If your service is billed by memory × wall-clock time,
idle waiting is billed too. Limits do not guarantee reuse; any request may start a
fresh sandbox. The standalone Viceroy 0.17.0 pinned for the fixtures admits at most
six requests per guest even with a larger SDK request limit. `edgezero serve` runs
`fastly compute serve`, whose bundled Viceroy can differ. Neither local ceiling
establishes deployed behavior.
Lifetime and memory checks occur between callbacks and cannot interrupt
blocked application work. Before using a local memory limit, verify that the
guest's memory-snapshot call succeeds: an unsupported snapshot conservatively
ends the SDK loop. CPU clock observations are not reliable cross-run benchmarks.

`ServeSummary::requests()` counts attempted callbacks, including a failed one;
a crash can prevent a final summary. Handler `EdgeError`s render as responses
and serving continues. Any other returned error (logger setup, a required KV
store that fails to open, request conversion, response-stream collection, or an
`EdgeError` that itself fails to render) makes
the SDK send a 500 with the error text and end the loop. Constructor panics
remain sandbox failures.

### Custom dispatch and streaming

The standard helper collects core response streams into a native response body.
For progressive client streaming, mutable native requests, response-extension
finalization, or post-send work, use `lifecycle::serve_custom` with a configured
`Serve` builder and a callback receiving `&mut lifecycle::Sandbox<T>`. EdgeZero
owns the retained slot and successful-only initialization; your callback chooses
the application-owned `T`. Initialize lazily so health checks can bypass expensive
construction. Use `request::dispatch_with_registries` when standard translation
suffices. For custom response handling, use
`request::into_core_request_with_registries(req, MyApp::stores(), extend)`
then drive `app.router().oneshot(request)` with `futures::executor::block_on`.
This inserts the same registries for the `Kv`, `Config`, `Secrets` and `AppConfig`
extractors as standard dispatch while leaving the core response available for
streaming and finalization.
The bare `into_core_request` helper inserts no store registries.

For manual streaming, append every header value, commit once with
`stream_to_client`, pump and flush chunks, then finish the stream. Handle errors
after commitment locally: returning an error to the SDK can trigger another
response attempt. Complete pending backend and post-send operations before the
callback returns. Do not cache native store handles with the app.

Use `Request::get_client_request_id()` for request correlation. `FASTLY_TRACE_ID`
describes the sandbox and must not be treated as a unique request ID. If a native
ID is unavailable, generate a request-local fallback and label its source. Do
not retain correlation fields in app state or global logger configuration.

### Custom lifecycle compatibility contract

The custom path uses public APIs: `lifecycle::{Sandbox, serve_custom, run_custom}`,
`request::into_core_request_with_registries`, and `app.router().oneshot`.
EdgeZero owns the successful-only retained slot and delegates request reception,
limits and `HandlerResult` completion to the SDK. Applications own setup, state,
refresh policy, response finalization and sending. Direct SDK serving remains
available when these helpers do not fit.

Follow this order inside a callback:

1. Return lightweight health responses before expensive initialization.
2. Use `sandbox.setup_once` to guard successful logger setup independently from
   app construction. Failed setup leaves the guard unset; partial side effects
   are not rolled back, so retries must be safe.
3. Call `sandbox.initialize` only for application-owned state. Success fills the
   slot; failure leaves it empty and returns the error unchanged. Read the value
   with `state()`. A handled failure may send a 503 and return `Ok(())` to permit
   a later callback to retry. Propagating it with `?` sends a 500 and ends the loop.
4. Convert with `into_core_request_with_registries` and drive router dispatch
   with `futures::executor::block_on`. Keep bodies,
   extensions, registries and native handles request-local. The bare
   `into_core_request` path provides no `Kv`, `Config`, `Secrets` or `AppConfig`
   stores. Registered app state still overwrites extensions of the same type.
5. Inspect response extensions and finalize before sending. Append repeated
   headers, commit once, flush progressive chunks and finish the writer. After
   commitment, handle errors locally and return `()` or `Ok(())`.
6. Complete pending backend and post-send work before returning. Constructors
   and callbacks that panic remain sandbox failures.

For example, after health checks, this policy permits an in-sandbox setup retry:

```rust
if let Err(error) = sandbox.setup_once(|| install_application_logger()) {
    eprintln!("logger setup failed: {error}");
    fastly::Response::from_status(503).send_to_client();
    return Ok(());
}
sandbox.initialize(|| build_application())?;
// Read sandbox.state(), perform registry-aware dispatch, then explicitly send.
// The ? above deliberately terminates this sandbox if application construction fails.
```

A complete callback looks like this. `serve_custom` returns a `ServeSummary`
that you inspect or convert with `into_result()`, and the infallible `Hooks::build_app`
needs an explicit error type for `initialize` to infer:

```rust
use edgezero_adapter_fastly::lifecycle::{self, Sandbox};
use edgezero_adapter_fastly::{Serve, request, response};
use edgezero_core::app::{App, Hooks};
use futures::executor::block_on;

fn main() -> Result<(), fastly::Error> {
    lifecycle::serve_custom(Serve::new().with_max_requests(10), handle).into_result()
}

fn handle(req: fastly::Request, sandbox: &mut Sandbox<App>) -> Result<(), fastly::Error> {
    if req.get_path() == "/health" {
        fastly::Response::from_status(200).send_to_client();
        return Ok(());
    }
    sandbox.initialize(|| Ok::<_, fastly::Error>(MyApp::build_app()))?;
    let core = request::into_core_request_with_registries(req, MyApp::stores(), |_, _| {})?;
    let app = sandbox.state().expect("initialized above");
    let core_response = block_on(app.router().oneshot(core))?;
    // Inspect response extensions and finalize here. To stream progressively,
    // append headers to a native response and use `stream_to_client` instead.
    response::from_core_response(core_response)?.send_to_client();
    Ok(())
}
```

The fixture's [`custom_dispatch`](https://github.com/stackpop/edgezero/blob/main/tests/fixtures/reusable-app/crates/fixture-fastly/src/lib.rs)
is a fuller reference covering logger setup, handled initialization failure,
response finalization, progressive streaming and post-send work.

`Sandbox::requests()` counts attempted callbacks, including early health responses
and failed ones. `initialization_attempts()` counts only actual builder calls.
These counters describe this sandbox and are part of the public lifecycle API.
They do not establish client completion or provider reuse.

Use `lifecycle::run_custom(Request::from_client(), callback)` for the single-request
branch. It creates fresh state without entering `Serve`. Both wrappers complete
`HandlerResult` exactly once. Use an ordinary `fn main`; `#[fastly::main]` may
attempt another error response if an already-completed error propagates.
Registry-aware `dispatch_with_registries` still collects response streams and
cannot replace this explicit streaming/finalization path.

Keep adapter, core and SDK versions compatible. Run the compatibility fixtures
against the checkout being adopted:

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite smoke
```

See the [fixture README](https://github.com/stackpop/edgezero/tree/main/tests/fixtures/reusable-app)
for assertions, runtime requirements and evidence files. An unavailable runtime
or missing reuse is unverified and exits 2. Local fixtures do not guarantee
provider reuse, deployed resource lifetimes or application workload gains.
Applications still own refresh, key rotation and bounded caches.

Warning caches persist too. Their bounded recent-name sets can evict entries,
so warnings can recur; the number of warnings logged is not a failure count.
Dynamic-backend capacity is service-wide, and registrations may wait for capacity
(see Fastly's [dynamic-backend limit behavior](https://www.fastly.com/documentation/reference/compute/errors)).
A sandbox request limit alone does not bound origin diversity or request fan-out.

The local fixtures are in `tests/fixtures/reusable-app`. Local reuse and streaming
results do not establish deployed eviction frequency, endpoint-handle validity,
resource accounting, or performance. Named endpoint delivery must be verified
separately from echoed stdout. Roll back by restoring the original single-request
entry point and removing retained state, not merely setting a request limit of one.

## Next Steps

- Learn about [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative deployment target
- Explore [Configuration](/guide/configuration) for manifest details
