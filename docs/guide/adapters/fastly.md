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

The request closure runs once per routed request; insert whatever typed values
your handlers need, then read them in a handler via a custom extractor or
`ctx.request().extensions().get::<Ja4>()`. The optional state returned by
`run_app_with_hooks` is available only after EdgeZero reaches its strongest
terminal delivery boundary. Detached admission responses skip the response
closure and return `None`.

For fully manual wiring, `FastlyService::send_request_with_hooks` provides the
same closed lifecycle for an already-received request. It does not expose a
response-returning dispatch operation.

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
rest of the runtime reads. The name is fixed because staging deploys rely on it:
a staging deploy creates a per-service twin and links it into the staging version
under that same name. `run_app` and `run_app_with_hooks` read the store for you.

Use `run_app_with_hooks` when custom request preparation or routed-response finalization is needed;
the adapter retains response ownership and returns finalizer state only after terminal delivery.
`run_app_with_config` and a hand-built `FastlyService` do **not** apply the env overlay, so staging
and overridden `__NAME` / `__KEY` selectors fall back to baked-in defaults. A fully manual
entrypoint must call `runtime_env_config` and use `send_with_registries_and_hooks` for parity with
`run_app`. A hand-written `Hooks` implementation must also override `stores()` or pass explicit
`StoresMetadata`; the trait default declares no stores.

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

`FastlyOutboundClient` uses deterministic **dynamic backends** derived from the canonical target,
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

To initialize logging manually, call `init_logger` with explicit settings:

```rust
use edgezero_adapter_fastly::init_logger;
use log::LevelFilter;

fn main() {
    init_logger("stdout", LevelFilter::Info, true).expect("init logger");
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
