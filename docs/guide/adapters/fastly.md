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

Deploy a verified application release with the adapter-managed lifecycle:

```bash
edgezero deploy --adapter fastly \
  --service-id "$FASTLY_SERVICE_ID" \
  --application-release "$RELEASE_ROOT"
```

The release fixes the package and both manifests before runtime configuration is
selected. A bare `edgezero deploy --adapter fastly` remains only as store-free
production compatibility for an existing manifest command. Staging and any
deployment that declares a Config, KV, or Secret Store require the verified
release-backed managed command above.

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
identical package bytes. Secret Stores remain optional. The only automatic
legacy cleanup removes an undeclared Config link whose alias and physical store
are both exactly `edgezero_runtime_env`; the account-wide store is retained for
older versions.

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
app_config = "hello from config store"
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
