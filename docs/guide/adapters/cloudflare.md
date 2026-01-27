# Cloudflare Workers

Deploy EdgeZero applications to Cloudflare Workers using WebAssembly.

## Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- Rust `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Project Setup

When scaffolding with `edgezero new my-app`, the Cloudflare adapter includes:

```
crates/my-app-adapter-cloudflare/
├── Cargo.toml
├── wrangler.toml
└── src/
    └── main.rs
```

### wrangler.toml

The Wrangler manifest configures your Worker:

```toml
name = "my-app"
main = "build/worker/shim.mjs"
compatibility_date = "2024-01-01"

[build]
command = "cargo build --release --target wasm32-unknown-unknown"
```

### Entrypoint

The Cloudflare entrypoint wires the adapter:

```rust
use edgezero_adapter_cloudflare::dispatch;
use edgezero_core::app::Hooks;
use my_app_core::App;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let app = App::build_app();
    dispatch(&app, req, env, ctx).await
}
```

## Building

Build for Cloudflare's Wasm target:

```bash
# Using the CLI
edgezero build --adapter cloudflare

# Or directly with cargo
cargo build -p my-app-adapter-cloudflare --target wasm32-unknown-unknown --release
```

## Local Development

Run locally with Wrangler:

```bash
# Using the CLI
edgezero serve --adapter cloudflare

# Or directly
wrangler dev --config crates/my-app-adapter-cloudflare/wrangler.toml
```

This starts a local server at `http://127.0.0.1:8787`.

## Deployment

Deploy to Cloudflare Workers:

```bash
# Using the CLI
edgezero deploy --adapter cloudflare

# Or directly
wrangler publish --config crates/my-app-adapter-cloudflare/wrangler.toml
```

## Fetch API

Cloudflare Workers use the global `fetch` API for outbound requests:

```rust
use edgezero_adapter_cloudflare::CloudflareProxyClient;
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
use edgezero_adapter_cloudflare::CloudflareRequestContext;

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
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown
```

Note: Some tests require `wasm-bindgen-test-runner` for execution.

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
