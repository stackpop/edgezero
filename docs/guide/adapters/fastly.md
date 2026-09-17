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

## Runtime descriptor

Fastly Compute has no process environment. EdgeZero therefore uses one physical
`edgezero_runtime_env` Config Store for all participating services and versions.
Every Fastly version links that store under the reserved alias
`edgezero_runtime_env` and reads exactly one internal key:

```text
EDGEZERO__SERVICES__<SERVICE_ID>__VERSIONS__<VERSION>__ENV_V1
```

The service ID and version come from Fastly's runtime. They are part of the
internal descriptor key, not the public environment variable names. A canonical
descriptor is compact JSON with sorted entry keys. For example, these are the
exact bytes for a staging version:

<!-- canonical-descriptor-exact:start -->

```text
{"format":1,"entries":{"EDGEZERO__LOGGING__LEVEL":"debug","EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY":"app_config_staging","EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME":"config-stage","EDGEZERO__STORES__KV__CACHE__NAME":"cache-stage","EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME":"credentials-stage"}}
```

<!-- canonical-descriptor-exact:end -->

Only the fixed runtime allowlist and selectors for logical stores declared by
the bundled `edgezero.toml` are accepted. The canonical public names are:

```text
EDGEZERO__STORES__CONFIG__<ID>__NAME
EDGEZERO__STORES__CONFIG__<ID>__KEY
EDGEZERO__STORES__KV__<ID>__NAME
EDGEZERO__STORES__SECRETS__<ID>__NAME
```

A Secret Store is optional. Its `__NAME` selector chooses a physical Secret
Store and never carries a secret value. All selected Config, KV, and Secret
resources must already exist.

Apps that declare any store fail closed when the runtime store, descriptor,
descriptor format, required selector, or exact resource link is missing or
invalid. An app with no declared stores may fall back to its baked-in fixed
runtime defaults when the runtime store or descriptor is absent.

### Deployment ordering

Release-backed deployments and deployments with declared stores are owned by
Fastly's managed lifecycle. Before any new code can receive traffic, EdgeZero:

1. verifies the immutable release and its package and manifests;
2. resolves complete Config, KV, and Secret resource inventories;
3. selects an exact active, staged, or initialized-draft source;
4. uploads the verified package to an unreachable draft;
5. removes inherited managed links that are no longer desired;
6. creates each desired link under the exact physical store name;
7. creates or verifies the immutable version descriptor;
8. reads back the descriptor and exact links and revalidates both versions; and
9. stages or activates the prepared version.

This prepares the descriptor and exact resource links before staging or
activation. Any ambiguous resource, alias collision, malformed provider record,
source race, descriptor mismatch, or readback failure withholds publication.
EdgeZero never publishes the package and then repairs runtime selectors.

Production and staging may use different runtime values or physical stores. They
still link the same physical `edgezero_runtime_env`, with separate immutable keys
for their service versions. Multiple services can safely share that physical
store because their descriptor keys cannot collide.

When a source version predates descriptors, clean cutover uses complete provider
resource-ID inventories to classify inherited Config/KV/Secret links. It deletes
every classified managed link absent from the desired set and preserves links
whose IDs are outside all managed inventories. It does not inspect any legacy
selector entry.

PR #344 service-scoped selectors, unscoped canonical entries, and old staging
twin stores are unsupported and are never read or written. They are inert under
the current runtime. Operators can remove those old entries and physical stores
separately after confirming older releases no longer use them.

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

For local Viceroy tests, seed the complete descriptor under an explicit local
service/version key that matches the runtime values used by the test. Do not seed
individual unscoped selector entries:

```toml
[local_server.config_stores.edgezero_runtime_env]
format = "inline-toml"

[local_server.config_stores.edgezero_runtime_env.contents]
EDGEZERO__SERVICES__localservice__VERSIONS__1__ENV_V1 = '''{"format":1,"entries":{"EDGEZERO__LOGGING__LEVEL":"debug","EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY":"app_config_staging","EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME":"config-stage","EDGEZERO__STORES__KV__CACHE__NAME":"cache-stage","EDGEZERO__STORES__SECRETS__CREDENTIALS__NAME":"credentials-stage"}}'''
```

The selected application Config Store still holds the typed data itself:

```toml
[local_server.config_stores.config-stage]
format = "inline-toml"

[local_server.config_stores.config-stage.contents]
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

See [the store migration guide](../manifest-store-migration.md) for selector
precedence and [the GitHub Actions guide](../deploy-github-actions.md) for the
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
