# Cloudflare Workers

Deploy EdgeZero applications to Cloudflare Workers using WebAssembly.

## Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- Rust `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Project Setup

When scaffolding with `edgezero new my-app --adapters cloudflare`, you get:

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
use edgezero_adapter_cloudflare::{dispatch, init_logger};
use my_app_core::App;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    init_logger();
    let app = App::build();
    dispatch(&app, req).await
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

let client = CloudflareProxyClient::new();
let response = ProxyService::new(client).forward(request).await?;
```

Unlike Fastly, there's no backend configuration needed - Workers can fetch any URL directly.

## Logging

Cloudflare Workers log via `console.log`. Initialize the logger:

```rust
use edgezero_adapter_cloudflare::init_logger;

fn main() {
    init_logger();
}
```

Configure logging level in `edgezero.toml`:

```toml
[adapters.cloudflare.logging]
level = "info"
```

View logs in the Wrangler output or Cloudflare dashboard.

## Context Access

Access Cloudflare-specific APIs via the request context:

```rust
use edgezero_adapter_cloudflare::CloudflareRequestContext;

#[action]
async fn handler(RequestContext(ctx): RequestContext) -> Response<Body> {
    if let Some(cf_ctx) = ctx.extensions().get::<CloudflareRequestContext>() {
        // Access Cloudflare-specific data
        let cf = cf_ctx.cf();
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

Cloudflare Workers support streaming via `ReadableStream`:

```rust
#[action]
async fn stream() -> Response<Body> {
    let stream = async_stream::stream! {
        for i in 0..100 {
            yield Ok::<_, std::io::Error>(format!("chunk {}\n", i).into_bytes());
        }
    };
    
    Response::builder()
        .body(Body::stream(stream))
        .unwrap()
}
```

The adapter converts EdgeZero streams to Cloudflare's `ReadableStream` format.

## Testing

Run contract tests for the Cloudflare adapter:

```bash
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown
```

Note: Some tests require `wasm-bindgen-test-runner` for execution.

## Manifest Configuration

Full `edgezero.toml` Cloudflare configuration:

```toml
[adapters.cloudflare.adapter]
crate = "crates/my-app-adapter-cloudflare"
manifest = "crates/my-app-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.build]
target = "wasm32-unknown-unknown"
profile = "release"

[adapters.cloudflare.commands]
build = "cargo build --release --target wasm32-unknown-unknown -p my-app-adapter-cloudflare"
serve = "wrangler dev --config crates/my-app-adapter-cloudflare/wrangler.toml"
deploy = "wrangler publish --config crates/my-app-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.logging]
level = "info"
```

## Comparison with Fastly

| Feature | Cloudflare Workers | Fastly Compute |
|---------|-------------------|----------------|
| Target | `wasm32-unknown-unknown` | `wasm32-wasip1` |
| Outbound requests | Global `fetch` | Named backends |
| Storage | KV, Durable Objects, R2 | KV Store, Object Store |
| Logging | `console.log` | Log endpoints |
| CLI | Wrangler | Fastly CLI |

## Next Steps

- Learn about [Fastly Compute](/guide/adapters/fastly) as an alternative
- Explore the [Axum adapter](/guide/adapters/axum) for local development
