# Cloudflare Workers

Deploy EdgeZero applications to Cloudflare Workers using WebAssembly.

## Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- worker-builder: `cargo install worker-builder`
- Rust `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Project Setup

When scaffolding with `edgezero new my-app`, the Cloudflare adapter includes:

```
crates/my-app-adapter-cloudflare/
├── Cargo.toml
├── wrangler.toml
└── src/
    ├── lib.rs
    └── main.rs
```

### wrangler.toml

The Wrangler manifest configures your Worker:

```toml
name = "my-app"
main = "build/worker/shim.mjs"
compatibility_date = "2023-05-01"

[build]
command = "worker-build --release"
```

### Entrypoint

The Cloudflare Wasm entrypoint in `src/lib.rs` wires the adapter:

```rust
use my_app_core::App;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    edgezero_adapter_cloudflare::run_app::<App>(req, env, ctx).await
}
```

`run_app` reads the portable store metadata baked into `App` by the
`app!` macro and the `EDGEZERO__*` env vars exposed on the worker
`Env` (Workers cannot enumerate `Env`, so the canonical key set is
derived from the baked store ids and queried individually). Per-id
`KV` / `Config` / `Secret` registries are built and injected into
request extensions automatically. No `edgezero.toml` is loaded by
the runtime — see [the migration guide](../manifest-store-migration.md).

For fully manual wiring, `CloudflareService::new(&app)` builds a dispatcher one
store at a time: `.with_config(binding)` (a KV binding name),
`.with_config_handle(handle)`, `.with_kv(binding)`, `.with_secrets()`, the
matching `.require_kv()` / `.require_secrets()` flags, and finally
`.dispatch(req)`. This path takes bindings verbatim and does not resolve
`EDGEZERO__STORES__*` selectors, so prefer `run_app` unless you are mocking a
backend. `dispatch_with_registries` is the registry-based dispatcher `run_app`
itself calls.

## Building

Build for Cloudflare's Wasm target:

```bash
# Using the CLI
edgezero build --adapter cloudflare
```

## Local Development

Run locally with Wrangler:

```bash
# Using the CLI
edgezero serve --adapter cloudflare
```

This starts a local server at `http://127.0.0.1:8787`.

## Deployment

Deploy to Cloudflare Workers:

```bash
# Using the CLI
edgezero deploy --adapter cloudflare

# Or directly
wrangler deploy --cwd crates/my-app-adapter-cloudflare
```

## Fetch API

Cloudflare Workers use the global `fetch` API for outbound requests:

```rust
use edgezero_adapter_cloudflare::proxy::CloudflareProxyClient;
use edgezero_core::proxy::ProxyService;

let client = CloudflareProxyClient;
let response = ProxyService::new(client).forward(request).await?;
```

Unlike Fastly, there's no backend configuration needed - Workers can fetch any URL directly.

## Logging

EdgeZero does not install a Cloudflare logger by default. Use your preferred logger (for example
`console_log` or your own `log` implementation), and view output in Wrangler or the Cloudflare
dashboard.

::: tip Logging status
Cloudflare logging is opt-in; install a logger (such as `console_log`) in your entrypoint if you
need structured output.
:::

## Context Access

Access Cloudflare-specific APIs via the request context extensions:

```rust
use edgezero_core::context::RequestContext;
use edgezero_adapter_cloudflare::context::CloudflareRequestContext;

async fn handler(ctx: RequestContext) -> Result<Response, EdgeError> {
    if let Some(cf_ctx) = CloudflareRequestContext::get(ctx.request()) {
        // Access Cloudflare-specific data
        let env = cf_ctx.env();
        let ctx = cf_ctx.ctx();
        // ...
    }

    // ...
}
```

## Environment Variables & Secrets

Define variables in `wrangler.toml`:

```toml
[vars]
API_URL = "https://api.example.com"

# Secrets are set via wrangler CLI
# wrangler secret put API_KEY
```

Access in handlers via the Cloudflare context or environment bindings.

## Config Store

Cloudflare does not expose a Fastly-style mutable config-store product, so each
declared `[stores.config]` id maps to a **KV namespace binding**. Reads are
asynchronous (`worker::kv::KvStore::get(key).text().await`).

```toml
# edgezero.toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

```toml
# wrangler.toml
[[kv_namespaces]]
binding = "app_config"
id      = "abc123…"
```

The binding name comes from `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME`
(defaulting to the logical id `app_config` when unset). Populate the
namespace via `wrangler kv key put`. Missing bindings log a one-time
warning and the id is dropped from the registry. See
[the migration guide](../manifest-store-migration.md) if you are coming
from the pre-rewrite `[vars]`-backed JSON-string form.

KV and config share the same `[[kv_namespaces]]` binding space on Cloudflare,
so the same logical id must not appear under both `[stores.kv]` and
`[stores.config]`; both would resolve to a single underlying namespace at
runtime. `edgezero config validate` rejects the collision.

## Secret Store

Worker Secrets is a single flat bag with no namespace concept, so exactly one
`[stores.secrets]` id is permitted; `edgezero config validate --strict` rejects
more than one. Handlers read values through the `Secrets` extractor or
`ctx.secret_store(id)`, and a secret with no matching binding resolves to `None`
rather than erroring.

```toml
# edgezero.toml
[stores.secrets]
ids = ["default"]
```

Populate secrets with the Wrangler CLI; there is no binding flag, since the
secret name is the binding:

```bash
wrangler secret put API_KEY
```

## KV Storage

Use Cloudflare KV for edge storage:

```toml
# wrangler.toml
[[kv_namespaces]]
binding = "MY_KV"
id = "abc123"
```

Access via the Cloudflare environment bindings in your handler.

## Durable Objects

For stateful edge computing, configure Durable Objects:

```toml
# wrangler.toml
[durable_objects]
bindings = [
  { name = "COUNTER", class_name = "Counter" }
]
```

## Streaming

Cloudflare Workers support streaming via `ReadableStream`. The adapter automatically converts `Body::stream` to Cloudflare's streaming format.

See the [Streaming guide](/guide/streaming) for examples and patterns.

## Testing

Run contract tests for the Cloudflare adapter:

```bash
WASM_BINDGEN_VERSION=$(
  awk '
    $1 == "name" && $3 == "\"wasm-bindgen\"" { in_pkg=1; next }
    in_pkg && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.lock
)
cargo install wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked --force
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --test contract
```

These tests use `wasm-bindgen-test-runner` and execute the adapter's real
wasm32 request path. The CLI version must exactly match the workspace's
`wasm-bindgen` version from `Cargo.lock`.

## Manifest Configuration

Configure the Cloudflare adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full manifest reference.

## Comparison with Fastly

| Feature           | Cloudflare Workers       | Fastly Compute                      |
| ----------------- | ------------------------ | ----------------------------------- |
| Target            | `wasm32-unknown-unknown` | `wasm32-wasip1`                     |
| Outbound requests | Global `fetch`           | Dynamic backends (derived from URI) |
| Storage           | KV, Durable Objects, R2  | KV Store, Object Store              |
| Logging           | `console.log`            | Log endpoints                       |
| CLI               | Wrangler                 | Fastly CLI                          |

## Next Steps

- Learn about [Fastly Compute](/guide/adapters/fastly) as an alternative
- Explore the [Axum adapter](/guide/adapters/axum) for local development
