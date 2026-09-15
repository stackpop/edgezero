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
manifest_version = 2
name = "my-app"
language = "rust"
authors = ["you@example.com"]

[local_server]
  [local_server.backends]
    [local_server.backends."origin"]
    url = "https://your-origin.example.com"
```

### Entrypoint

The Fastly entrypoint wires the adapter:

```rust
use my_app_core::App;

#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    edgezero_adapter_fastly::run_app::<App>(req)
}
```

`run_app` reads logging and store config at runtime from `EDGEZERO__*`
environment variables (see
[the migration guide](../manifest-store-migration.md)) and builds
per-id `KV` / `Config` / `Secret` registries from the portable store
metadata baked into `App` by the `app!` macro. No `edgezero.toml` is
loaded by the runtime.

The low-level `dispatch()` helper remains available only for fully manual wiring and does not inject
store metadata. Prefer `run_app` or `dispatch_with_config` for normal use.
`dispatch_with_config_handle` exists for advanced/manual cases where you already have a prepared
`ConfigStoreHandle`.

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

By default `run_app` initializes the Fastly logger. If your app already installs
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

# Or directly with cargo
cargo build -p my-app-adapter-fastly --target wasm32-wasip1 --release
```

The compiled Wasm binary is placed in `target/wasm32-wasip1/release/`.

## Local Development

Run locally with Viceroy (Fastly's local simulator):

```bash
# Using the CLI
edgezero serve --adapter fastly

# Or directly
fastly compute serve --skip-build
```

This starts a local server at `http://127.0.0.1:7676`.

## Deployment

Deploy to Fastly Compute@Edge:

```bash
# Using the CLI
edgezero deploy --adapter fastly

# Or directly
fastly compute deploy
```

## Backends

EdgeZero's Fastly proxy client uses **dynamic backends** derived from the target URI (host + scheme).
You do not need to predeclare backends in `fastly.toml` for EdgeZero proxying.

```rust
use edgezero_adapter_fastly::FastlyProxyClient;
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
Fastly logging is wired when you call `init_logger` (or `run_app`); otherwise no logger is installed.
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
and drops that id from the registry. Migrating from `name`/`adapters.*`?
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

Fastly supports native streaming via `stream_to_client`. The adapter automatically converts `Body::stream` to Fastly's streaming APIs.

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

## Next Steps

- Learn about [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative deployment target
- Explore [Configuration](/guide/configuration) for manifest details

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
for fresh extensions on every request. The first callback initializes logging
and builds the app. Each callback reads runtime configuration and constructs new
store registries. App construction and `configure` execute once per retained
owner; explicit work elsewhere still executes whenever the application calls it.

Logging takes the first configuration snapshot, unless the app owns logging.
An unavailable optional runtime configuration store disables logging for that
sandbox, even if subsequent reads recover store selectors. It does not silently
retry or reconfigure logging. The current overlay enables logging when an
endpoint exists and uses `echo_stdout: true`. Applications requiring another
initialization policy should use a custom SDK callback.

SDK 0.12.1 exposes `with_max_requests`, `with_timeout`, `with_max_lifetime`, and
`with_max_memory`. A value of zero disables the request-count or memory limit;
it does not request zero callbacks or zero memory use. The lifetime limit is
measured from `Serve` construction, including time before the first callback.
Limits do not guarantee reuse; any request may start a fresh sandbox.
Lifetime and memory checks occur between callbacks and cannot interrupt
blocked application work. Before using a local memory limit, verify that the
guest's memory-snapshot call succeeds: an unsupported snapshot conservatively
ends the SDK loop. CPU clock observations are not reliable cross-run benchmarks.

`ServeSummary::requests()` counts attempted callbacks, including a failed one.
Record response commitment, guest completion, and client completion separately;
a crash can prevent a final summary. Ordinary handler errors are rendered by the
router. Errors escaping conversion, required store setup, stream collection, or
error rendering retain the SDK's terminal error behavior. Constructor panics
remain sandbox failures.

### Custom dispatch and streaming

The standard helper collects core response streams into a native response body.
For progressive client streaming, mutable native requests, response-extension
finalization, or post-send work, use `Serve::run` or `run_with_context` directly.
Capture an ordinary `Option<App>` or an application-owned initialized-state value
inside the callback. Initialize lazily so health checks can bypass expensive
construction. Use `runtime_env_config` and `request::dispatch_with_registries`
when standard translation suffices; otherwise retain the existing raw
`into_core_request` and router dispatch path.

For manual streaming, append every header value, commit once with
`stream_to_client`, pump and flush chunks, then finish the stream. Handle errors
after commitment locally: returning an error to the SDK can trigger another
response attempt. Complete pending backend and post-send operations before the
callback returns. Do not cache native store handles with the app.

Use `Request::get_client_request_id()` for request correlation. `FASTLY_TRACE_ID`
describes the sandbox and must not be treated as a unique request ID. If a native
ID is unavailable, generate a request-local fallback and label its source. Do
not retain correlation fields in app state or global logger configuration.

Warning caches persist too. Their bounded recent-name sets can evict entries,
so warnings can recur; suppressed warning counts are not failure counts.
Dynamic-backend capacity is service-wide, and registrations may wait for capacity.
A sandbox request limit alone does not bound origin diversity or request fan-out.

The local fixtures are in `tests/fixtures/reusable-app`. Local reuse and streaming
results do not establish deployed eviction frequency, endpoint-handle validity,
resource accounting, or performance. Named endpoint delivery must be verified
separately from echoed stdout. Roll back by restoring the original single-request
entry point and removing retained state, not merely setting a request limit of one.
