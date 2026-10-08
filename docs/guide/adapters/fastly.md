# Fastly Compute@Edge

Deploy EdgeZero applications to Fastly's Compute@Edge platform using WebAssembly.

## Prerequisites

- [Fastly CLI](https://developer.fastly.com/learning/compute/#install-the-fastly-cli)
- Rust `wasm32-wasip1` target: `rustup target add wasm32-wasip1`
- [Viceroy](https://github.com/fastly/Viceroy) 0.21.0 for local execution and testing

The adapter pins the Fastly SDK to 0.13.1. Older Viceroy releases may reject its
host imports before application code executes; use the repository's pinned runtime.

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

`edgezero provision --adapter fastly` writes `[setup.kv_stores]`,
`[setup.secret_stores]` and `[setup.config_stores]` entries into `fastly.toml`,
keyed by each store's env-resolved platform name. It deliberately leaves the
Viceroy-only `[local_server.*]` tables alone: the config-store stanzas there are
written by your generated app CLI, `<app-cli> config push --adapter fastly --local` (the bundled `edgezero` binary has no typed app config and exits 2), and KV / secret
local-server seeding is hand-edited.

### Entrypoint

The Fastly entrypoint wires the adapter:

```rust
use my_app_core::App;

pub fn main() -> Result<(), fastly::Error> {
    edgezero_adapter_fastly::run_app::<App>()
}
```

Do not add Fastly's response-returning entrypoint attribute. EdgeZero initializes the ABI,
receives the client request, and owns `stream_to_client` through final body-handle close.

`run_app` reads logging and store config at runtime from `EDGEZERO__*`
environment variables (see
[the migration guide](../manifest-store-migration.md)) and builds
per-id `KV` / `Config` / `Secret` registries from the portable store
metadata baked into `App` by the `app!` macro. No `edgezero.toml` is
loaded by the runtime.
If `Hooks::configure` fails, `run_app` returns the source-preserving
`application configuration failed` error before receiving the client request.

For fully manual wiring, build `FastlyService`, attach stores as needed, and call its send-owning
`send()` method. Prefer `run_app` for manifest-driven store resolution.

### Capturing raw-request signals (JA4, H2 fingerprint)

`run_app` receives and converts the `fastly::Request` into a neutral core request before
dispatch. The client IP is carried across automatically; read it via
`FastlyRequestContext` (see [Context Access](#context-access) below). Other
Fastly-only signals that are readable only on the raw request
(`get_tls_ja4()`, `get_client_h2_fingerprint()`) aren't reachable from handlers
by default. Use `run_app_with_hooks`, which runs a request closure against a
scratch `Extensions` **before** conversion and merges the values into the core
request. Its response closure runs only for routed responses before egress
policy and framing; EdgeZero retains delivery ownership:

```rust
#[derive(Clone)]
struct Ja4(String);

pub fn main() -> Result<(), fastly::Error> {
    edgezero_adapter_fastly::run_app_with_hooks::<App, _, _, ()>(
        |raw, ext| {
            if let Some(ja4) = raw.get_tls_ja4() {
                ext.insert(Ja4(ja4.to_owned()));
            }
        },
        |_response| (),
    )
    .map(|_post_send_state| ())
}
```

The request closure runs once before admission for each request that reaches
preparation; insert whatever typed values your handlers need, then read them via a custom extractor or
`ctx.request().extensions().get::<Ja4>()`. The optional state returned by
`run_app_with_hooks` is available only after EdgeZero reaches its strongest
terminal delivery boundary. Detached admission responses skip the response
closure and return `None`.

For fully manual wiring, `FastlyService::send_request_with_hooks` provides the
same closed lifecycle for an already-received request. It does not expose a
response-returning dispatch operation.

### Owning your own logging

When a log endpoint is configured, `run_app` initializes the Fastly logger. If your app installs
a `log` backend, opt out with the platform-neutral `Hooks::owns_logging()` flag —
via the `app!` macro:

```rust
edgezero_core::app!("edgezero.toml", owns_logging = true);
```

or on a hand-written `Hooks` impl (`fn owns_logging() -> bool { true }`). Every
adapter's `run_app` honors it. The app must install its backend **before** entering the runner,
apply the configured runtime level, and set both backend and facade filters. Use
`edgezero_core::resolve_logging_level(&env)` for consistent level resolution; raising only
`log::set_max_level` does not override `log_fastly`'s internal filter.

### Custom entry points

Compute@Edge has no process environment, so the `EDGEZERO__*` runtime overrides
(logging settings, per-store platform names, the config-store `__KEY` selector)
are read from a Fastly Config Store named `edgezero_runtime_env`, exported as
`RUNTIME_ENV_STORE_NAME`. Entries in that store are service-scoped
(`EDGEZERO__SERVICES__<SERVICE_ID>__…`, see [Config Store](#config-store));
`runtime_env_config` translates them back to the canonical unscoped keys the
rest of the runtime reads. The name is fixed because staging deploys rely on it:
a staging deploy creates a per-service twin and links it into the staging version
under that same name. `run_app` and `run_app_with_hooks` read the store for you.

Use `run_app_with_hooks` when custom request preparation or routed-response finalization is needed;
the adapter retains response ownership and returns finalizer state only after terminal delivery.
`run_app_with_config` and a hand-built `FastlyService` do **not** apply the env overlay, so staging
and overridden `__NAME` / `__KEY` selectors fall back to baked-in defaults. A fully manual
entrypoint must call `runtime_env_config` and use `request::send_with_registries_and_hooks` for parity
with `run_app`. Inside a serving callback, use `request::send_request_with_registries_and_hooks`
with the already-received request; the receiving helper would attempt to receive another request.
A hand-written `Hooks` implementation must also override `stores()` or pass explicit
`StoresMetadata`; the trait default declares no stores.

`runtime_env_config` returns `FastlyRuntimeConfig`, not `EnvConfig`. It performs no logging:
use `config.env` for logger and dispatch setup, then call `config.emit_boot_diagnostics()` after
installing the backend. An unavailable runtime Config Store produces one deferred, fixed-message
boot warning, consumed at most once. Without an installed backend it is not observable.
`run_app` and `run_app_with_hooks` do this automatically, including when application configuration
fails. `run_app_with_config` does not collect the runtime-env diagnostic.

`FastlyLogging::from(&EnvConfig)` derives `use_fastly_logger` from
`endpoint.is_some()`, which is what keeps a local Viceroy run off the reserved
`stdout` endpoint when no endpoint is configured.

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

Deploy to Fastly Compute@Edge:

```bash
# Using the CLI
edgezero deploy --adapter fastly

# Or directly
fastly compute deploy -C crates/my-app-adapter-fastly
```

## Backends

Outbound HTTP uses deterministic **dynamic backends** derived from the canonical target,
TLS identity, and provider timer budget. You do not predeclare those destinations in
`fastly.toml`, but dynamic backends must be enabled on the deployed Fastly service. The local CLI
cannot prove that service entitlement, so `outbound-http` is BestEffort. A disabled service
returns a typed 502 with an enablement diagnostic.

Fastly also has documented deadline, upload, elastic-budget, batch-isolation, and lazy downstream
streaming limitations. EdgeZero owns the low-level `stream_to_client` lifetime and writes portable
response chunks without whole-body collection. Synchronous source polls and hostcalls cannot be
preempted, so downstream streaming and response-egress guarantees remain BestEffort. See
[Capabilities](/guide/capabilities) before marking a capability required.

## Logging

Fastly uses endpoint-based logging. The runtime reads its logging settings from
`EDGEZERO__LOGGING__LEVEL` and `EDGEZERO__LOGGING__ENDPOINT` in the
`edgezero_runtime_env` Config Store (see
[Custom entry points](#custom-entry-points)), not from `edgezero.toml`. Setting
`ENDPOINT` is what enables the Fastly logger; with it unset, no platform logger
is installed. `EDGEZERO__LOGGING__ECHO_STDOUT` and
`EDGEZERO__LOGGING__USE_FASTLY_LOGGER` are resolved into `EnvConfig` but not
applied on this path: stdout echo is always on, and logger use is derived from
`ENDPOINT` alone. An `[adapters.fastly.logging]` table in `edgezero.toml` is
consumed only by `edgezero new` when scaffolding.

Managed Fastly logging keeps the `edgezero::boot` target at `Warn` while ordinary records use
the configured level. Both the SDK and facade allow boot warnings even at ordinary `Off`;
raising the facade maximum alone is insufficient. `edgezero::boot` is a logical target, not a
Fastly endpoint name. Initialize logging before application configuration.

To initialize logging manually, call `init_logger` with a provisioned endpoint:

```rust
use edgezero_adapter_fastly::init_logger;
use log::LevelFilter;

fn main() {
    init_logger("edgezero-logs", LevelFilter::Info, true).expect("init logger");
}
```

::: tip Logging status
Fastly logging is wired when you call `init_logger`, or when `run_app` finds `EDGEZERO__LOGGING__ENDPOINT` set; otherwise no logger is installed.
:::

## Config Store

Fastly uses a native Config Store resource link for runtime configuration. Declare logical config
ids in `edgezero.toml`; each id opens its own platform store via
`EDGEZERO__STORES__CONFIG__<ID>__NAME` (default = the logical id):

Because `edgezero_runtime_env` is an account-wide Fastly resource, its stored
keys are scoped by the current service ID:

```text
EDGEZERO__SERVICES__<SERVICE_ID>__STORES__CONFIG__<ID>__NAME
EDGEZERO__SERVICES__<SERVICE_ID>__STORES__CONFIG__<ID>__KEY
```

The runtime obtains `<SERVICE_ID>` from Fastly and translates these entries back
to the portable `EDGEZERO__STORES__*` form. Legacy unscoped entries are ignored
because they have no safe owner when the Config Store is linked to multiple
services. Re-run `edgezero provision --adapter fastly` to write scoped `__NAME`
entries, and rewrite any manually managed adapter, logging, or `__KEY` entries
under the service prefix. Provision writes only the selected service's
namespace; a non-default store-name mapping therefore requires top-level
`service_id` in `fastly.toml` or `FASTLY_SERVICE_ID`. If both are set, they must
match.

Viceroy reports `0000000000000000000000` as its local service ID. Entries in a
local `[local_server.config_stores.edgezero_runtime_env.contents]` block must
therefore use `EDGEZERO__SERVICES__0000000000000000000000__...`, not the
production service ID or the unscoped canonical key.

```toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

For local Viceroy testing, mirror the platform name in `fastly.toml`:

```toml
[local_server.config_stores.app_config]
format = "inline-toml"

[local_server.config_stores.app_config.contents]
greeting = "hello from config store"
```

Handlers read values through the `Config` extractor or `ctx.config_store(id)`:

```rust
async fn handler(config: Config) -> Result<Response, EdgeError> {
    let store = config.named("app_config").ok_or_else(|| EdgeError::service_unavailable("no `app_config`"))?;
    let greeting = store.get("greeting").await?.unwrap_or_default();
    // …
}
```

If a configured store link is missing, the adapter logs a one-time warning
and retains a typed failing declared binding, rather than reporting intentional absence.
Raw values are shared `ConfigValue` UTF-8 payloads; use `as_str()` to borrow them.
Migrating from `name`/`adapters.*`?
See [the migration guide](../manifest-store-migration.md).

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

Fastly provides `stream_to_client`, but that API is incompatible with the standard
response-returning SDK entrypoint. Generated applications use EdgeZero's undecorated, send-owning
entrypoint instead. EdgeZero commits the response head through the raw Fastly ABI, writes each
portable body chunk to the streaming body handle with short-write accounting, and closes or
abandons that handle exactly once. A blocked synchronous source poll or hostcall cannot be
preempted, and a failed consuming `finish` call leaves no handle to abandon; those limits keep the
response-egress capabilities at BestEffort.

See the [Streaming guide](/guide/streaming) and
[capability matrix](/guide/capabilities#outbound-matrix) for the exact boundary.

## Testing

Run contract tests for the Fastly adapter:

```bash
cargo install viceroy --version 0.21.0 --locked
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

Opt in with an ordinary `main` in place of the single-request `run_app` call:

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

`serve_app_with_hooks` additionally accepts a preparation hook
`FnMut(&mut fastly::Request, &mut Extensions)` and a finalization hook
`FnMut(&mut edgezero_core::Response) -> State`. Preparation runs before admission;
finalization runs only for routed responses, before egress policy and framing.
Detached admission responses skip finalization. The retained helper discards the
finalizer's state after delivery; use the request-level send helper in a custom
callback when post-send work needs that state.

The first request initializes logging and builds the app through
`App::build::<MyApp>(FASTLY_PLATFORM) -> Result<App, EdgeError>`. Each request reads
runtime configuration and constructs new store registries. Successful app construction
and `configure` execute once per sandbox; explicit work elsewhere still executes
whenever the application calls it. Both hooks and their captures are retained for
the whole serving loop; the supplied native request, extensions, and response are
request-local. Create request-specific mutable data inside the preparation hook
or store it in those extensions.

Logging takes the first configuration snapshot, unless the app owns logging.
An unavailable optional runtime configuration store disables logging for that
sandbox, even if subsequent reads recover store selectors. It does not silently
retry or reconfigure logging. The current overlay enables logging when an
endpoint exists and uses `echo_stdout: true`. Applications requiring another
initialization policy should use `lifecycle::serve_custom` and `setup_once`.
`runtime_env_config` returns `FastlyRuntimeConfig` with its public `env` and deferred
diagnostics. The standard helper emits those diagnostics after the logging setup
decision. Custom callbacks must likewise call `emit_boot_diagnostics()` after
installing their logger. Each configuration value consumes its pending warning at
most once; without a backend it is not observable. Absence of a warning does not
establish a healthy configuration read.

SDK 0.13.1 exposes `with_max_requests`, `with_timeout`, `with_max_lifetime`, and
`with_max_memory`. A value of zero disables the request-count or memory limit;
it does not request zero callbacks or zero memory use. The lifetime limit is
measured from `Serve` construction, including time before the first callback.
`with_timeout` bounds only the idle wait for the next request; it does not bound
handler or backend time. If your service is billed by memory × wall-clock time,
idle waiting is billed too. Limits do not guarantee reuse; any request may start a
fresh sandbox. The fixtures pin standalone Viceroy 0.21.0; any observed per-guest
request ceiling belongs to that runtime and run, not to the SDK request limit.
`edgezero serve` runs `fastly compute serve`, whose bundled Viceroy can differ.
Record the executable and observed reuse rather than assuming a local ceiling
establishes deployed behavior.
Lifetime and memory checks occur between callbacks and cannot interrupt
blocked application work. Before using a local memory limit, verify that the
guest's memory-snapshot call succeeds: an unsupported snapshot conservatively
ends the SDK loop. CPU clock observations are not reliable cross-run benchmarks.

`ServeSummary::requests()` counts attempted callbacks, including a failed one;
a crash can prevent a final summary. Handler `EdgeError`s render as responses
and serving continues when adapter delivery succeeds. A returned setup, required-store,
admission-abort, or terminal-delivery error ends the loop and remains in its summary.
The lifecycle wrapper records the callback result without asking the SDK to send a
synthetic 500, including when a response was already committed. Ingress and egress
policies may produce their own error responses; an escaping error is not proof that
any response was sent. Constructor panics remain sandbox failures.

### Custom dispatch and streaming

The standard helpers preserve core response streams without whole-body collection.
For custom initialization or post-send work, use `lifecycle::serve_custom` with a
configured `Serve` builder and a callback
`FnMut(fastly::Request, &mut lifecycle::Sandbox<T>) -> Result<(), E>`. EdgeZero owns
the retained slot and successful-only initialization; your callback chooses the
application-owned `T`. Initialize lazily so health checks can bypass expensive
construction.

For an EdgeZero response, call `request::send_request_with_registries_and_hooks`
with the app, store metadata, received request, `&runtime.env`, preparation hook,
and finalization hook. It runs canonical admission, injects fresh `Kv`, `Config`,
`Secrets`, and `AppConfig` registries, then owns framing, streaming, deadlines, and
terminal delivery. Preparation borrows a mutable native request and extension bag;
finalization borrows a mutable routed core response. Neither hook returns a
response for the callback to send separately. The result is `Result<Option<State>,
fastly::Error>`: `Some` carries routed finalizer state only after terminal delivery,
while a delivered detached admission response returns `None`.

The manual response lifetime remains adapter-owned: append every header value,
commit once through `stream_to_client`, account accepted body bytes, and finish or
abandon the body handle exactly once. A native response constructed directly by
your callback, such as a health reply, must instead be explicitly sent there;
manual native streams must be pumped, flushed, and finished before returning.
The lifecycle wrapper never supplies an extra response. Returning `Err` terminates
serving without a synthetic send, even after commitment. Return `Ok(())` only when
your policy permits continued serving after cleanup. Complete pending backend and
post-send operations before returning. Do not cache native store handles with the app.

Use `Request::get_client_request_id()` for request correlation. `FASTLY_TRACE_ID`
describes the sandbox and must not be treated as a unique request ID. If a native
ID is unavailable, generate a request-local fallback and label its source. Do
not retain correlation fields in app state or global logger configuration.

### Custom lifecycle contract

The custom path uses public APIs: `lifecycle::{Sandbox, serve_custom, run_custom}`
and `request::send_request_with_registries_and_hooks`. EdgeZero owns the
successful-only retained slot and delegates request reception and limits to the
SDK. The callback owns delivery: it either explicitly sends a native response or
calls the adapter's closed send helper. The serving wrapper passes only the
terminal `Result<(), E>` to the SDK without a synthetic response. Applications
own setup, state, refresh policy, hooks, and post-send work. There is no
response-returning conversion or whole-body compatibility path.

Follow this order inside a callback:

1. Explicitly send lightweight health responses before expensive initialization.
2. Read fresh `runtime_env_config(MyApp::stores())` and use `runtime.env` for logger
   and store setup. Use `sandbox.setup_once` to guard successful logger setup independently from
   app construction. Failed setup leaves the guard unset; partial side effects
   are not rolled back, so retries must be safe. Emit boot diagnostics after the
   logger is installed, not while reading configuration.
3. Call `sandbox.initialize` with `App::build::<MyApp>(FASTLY_PLATFORM)` for a core
   app, mapping its typed error at the adapter boundary. Success fills the slot;
   failure leaves it empty and returns the error unchanged. Read the value
   with `state()`. A handled failure may send a 503 and return `Ok(())` to permit
   a later callback to retry. Propagating it with `?` ends the loop without a synthetic send.
4. Pass the received request to `send_request_with_registries_and_hooks`. Its
   `FnOnce` preparation hook may mutate the native request and portable extensions
   before admission; its `FnOnce` finalizer may inspect response extensions and
   mutate a routed response before egress. Keep bodies, extensions, registries,
   and native handles request-local. Registered app state still overwrites
   extensions of the same type.
5. Wait for the send helper's terminal result before using finalizer state or
   starting post-send work. `None` means a detached response skipped finalization.
   Never send the core response again; a returned delivery error may follow commitment.
6. Complete pending backend and post-send work before returning. Constructors
   and callbacks that panic remain sandbox failures.

For example, after health checks, this policy permits an in-sandbox setup retry:

```rust
if let Err(error) = sandbox.setup_once(|| install_application_logger()) {
    eprintln!("logger setup failed: {error}");
    fastly::Response::from_status(503).send_to_client();
    return Ok(());
}
sandbox.initialize(|| App::build::<MyApp>(FASTLY_PLATFORM).map_err(fastly::Error::from))?;
// Read sandbox.state() and call the closed send helper; do not send its response again.
// The ? above deliberately terminates this sandbox if application construction fails.
```

A complete callback looks like this. `serve_custom` returns a `ServeSummary`
that you inspect or convert with `into_result()`. `App::build` is fallible and
installs `FASTLY_PLATFORM` before application configuration:

```rust
use edgezero_adapter_fastly::lifecycle::{self, Sandbox};
use edgezero_adapter_fastly::{
    FASTLY_PLATFORM, FastlyLogging, Serve, init_logger, request, runtime_env_config,
};
use edgezero_core::app::{App, Hooks};
use my_app_core::App as MyApp;

fn main() -> Result<(), fastly::Error> {
    lifecycle::serve_custom(Serve::new().with_max_requests(10), handle).into_result()
}

fn handle(req: fastly::Request, sandbox: &mut Sandbox<App>) -> Result<(), fastly::Error> {
    if req.get_path() == "/health" {
        fastly::Response::from_status(200).send_to_client();
        return Ok(());
    }
    let stores = MyApp::stores();
    let mut runtime = runtime_env_config(stores);
    sandbox.setup_once(|| {
        let logging = FastlyLogging::from(&runtime.env);
        if logging.use_fastly_logger && !MyApp::owns_logging() {
            init_logger(
                logging.endpoint.as_deref().expect("configured endpoint"),
                logging.level,
                logging.echo_stdout,
            )?;
        }
        Ok::<(), fastly::Error>(())
    })?;
    runtime.emit_boot_diagnostics();
    sandbox.initialize(|| App::build::<MyApp>(FASTLY_PLATFORM).map_err(fastly::Error::from))?;
    let app = sandbox.state().expect("initialized above");
    let _post_send_state = request::send_request_with_registries_and_hooks(
        app,
        stores,
        req,
        &runtime.env,
        |_raw, _extensions| {},
        |response| response.status(),
    )?;
    // Delivery is terminal here; complete any post-send work before returning.
    Ok(())
}
```

The fixture's [`custom_dispatch`](https://github.com/stackpop/edgezero/blob/main/tests/fixtures/reusable-app/crates/fixture-fastly/src/lib.rs)
is a fuller reference covering logger setup, handled initialization failure,
response finalization, adapter-owned progressive streaming and post-send work.

`Sandbox::requests()` counts attempted callbacks, including early health responses
and failed ones. `initialization_attempts()` counts only actual builder calls.
These counters describe this sandbox and are part of the public lifecycle API.
They do not establish client completion or provider reuse.

Use `lifecycle::run_custom(Request::from_client(), callback)` for the single-request
branch, initializing the Fastly ABI before receiving that request. It creates
fresh state without entering `Serve` and returns the callback's `Result<(), E>`
directly, without asking the SDK to render an error. Use an ordinary `fn main`;
the response-returning SDK entrypoint attribute does not own this delivery lifecycle.

Keep adapter, core and SDK versions aligned with the pinned checkout. Run the
lifecycle fixtures against the checkout being adopted:

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
