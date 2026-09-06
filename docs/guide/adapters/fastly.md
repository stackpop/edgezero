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

`edgezero provision --adapter fastly` writes `[setup.kv_stores]`,
`[setup.secret_stores]` and `[setup.config_stores]` entries into `fastly.toml`
for the declared store ids; the `[local_server.*]` tables are the Viceroy-only
mirror of those stores.

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

For fully manual wiring, `FastlyService::new(&app)` builds a dispatcher one
store at a time: `.with_config(name)`, `.with_config_handle(handle)`,
`.with_kv(name)`, `.with_secrets()`, the matching `.require_kv()` /
`.require_secrets()` flags, and finally `.dispatch(req)`. This path does not
apply the runtime env overlay. A bare handle binds the config registry's
default key to `"default"` and does not resolve `EDGEZERO__STORES__*`
selectors, so prefer `run_app`, or see
[Custom entry points](#custom-entry-points) for full parity.

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

### Custom entry points

Compute@Edge has no process environment, so the `EDGEZERO__*` runtime overrides
(logging settings, per-store platform names, the config-store `__KEY` selector)
are read from a Fastly Config Store named `edgezero_runtime_env`, exported as
`RUNTIME_ENV_STORE_NAME`. Entries in that store are service-scoped
(`EDGEZERO__SERVICES__<SERVICE_ID>__…`, see [Config Store](#config-store));
`runtime_env_config` translates them back to the canonical unscoped keys the
rest of the runtime reads. The name is fixed because staged deploys rely on it:
a staged deploy creates a per-service staging twin and links it into the staged
version under that same name. `run_app` and `run_app_with_request_extensions`
read the store for you.

An entry point that does its own wiring must call `runtime_env_config` itself,
derive `FastlyLogging` from the result, and dispatch through
`dispatch_with_registries`:

```rust
use edgezero_adapter_fastly::request::dispatch_with_registries;
use edgezero_adapter_fastly::{FastlyLogging, init_logger, runtime_env_config};
use edgezero_core::app::Hooks as _;
use my_app_core::App;

#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    let stores = App::stores();
    let env = runtime_env_config(stores);
    let logging = FastlyLogging::from(&env);
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout).expect("init logger");
    }
    let app = App::build_app();
    Ok(dispatch_with_registries(&app, req, stores, &env, |_req, _ext| {})?)
}
```

Two footguns live on this path. `run_app_with_config` and a hand-built
`FastlyService` do **not** apply the env overlay, so staged and overridden
`__NAME` / `__KEY` selectors are silently ignored and every store falls back to
its baked-in default. And a hand-written `Hooks` impl inherits the default
`stores()`, which is empty; empty metadata derives no `EDGEZERO__STORES__*` keys
at all, so no override ever resolves. Such an impl must override `stores()` or
pass explicit `StoresMetadata`.

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

A `Body::Stream` response is drained into a `fastly::Body` before the adapter
returns, so the full payload is materialised in memory rather than streamed to
the client chunk by chunk.

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
