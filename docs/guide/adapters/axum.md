# Axum (Native)

Run EdgeZero applications natively using Axum and Tokio for local development, testing, and container deployments.

## Overview

The Axum adapter provides:

- **Local development server** - Fast iteration without Wasm compilation
- **Native testing** - Run tests with standard `cargo test`
- **Container deployments** - Deploy to any platform supporting native binaries

## Project Setup

When scaffolding with `edgezero new my-app`, the Axum adapter includes:

```
crates/my-app-adapter-axum/
├── Cargo.toml
├── axum.toml
└── src/
    └── main.rs
```

### Entrypoint

The Axum entrypoint wires the adapter:

```rust
use my_app_core::App;

fn main() -> anyhow::Result<()> {
    edgezero_adapter_axum::run_app::<App>()
}
```

`run_app` installs `simple_logger`, builds the app, and reads bind /
store / logging config at runtime from `EDGEZERO__*` environment
variables (see [the migration guide](../manifest-store-migration.md)).
The portable store metadata baked into `App` by the `app!` macro
drives which logical stores are exposed; no `edgezero.toml` needs to
be loaded by the runtime.

## Development Server

Run your project locally on the Axum adapter:

```bash
edgezero serve --adapter axum
```

This starts a server at `http://127.0.0.1:8787` with standard logging to stdout.

### Manual Start

Run the Axum entrypoint directly:

```bash
# Using the CLI
edgezero serve --adapter axum

# Or directly with cargo
cargo run -p my-app-adapter-axum
```

## Building

Build a native release binary:

```bash
# Using the CLI
edgezero build --adapter axum

# Or directly
cargo build -p my-app-adapter-axum --release
```

The binary is placed in `target/release/my-app-adapter-axum`.

## Proxy Client

The Axum adapter provides a native HTTP client for proxying:

```rust
use edgezero_adapter_axum::AxumProxyClient;
use edgezero_core::proxy::ProxyService;

let client = AxumProxyClient::default();
let response = ProxyService::new(client).forward(request).await?;
```

This uses `reqwest` under the hood for outbound HTTP requests.

## Logging

The Axum adapter's `run_app` helper installs `simple_logger` and reads logging configuration
from `edgezero.toml` (level and `echo_stdout`). If you want a different logger, wire your own
entrypoint using `App::build_app()` and `AxumDevServer`.

::: tip Logging status
`run_app` wires logging automatically; custom entrypoints should install a logger explicitly.
:::

## Testing

The Axum adapter enables standard Rust testing:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::Hooks;
    use edgezero_core::http::{Request, Method};

    #[tokio::test]
    async fn test_handler() {
        let app = App::build_app();
        let router = app.router();

        let request = Request::builder()
            .method(Method::GET)
            .uri("/hello")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
    }
}
```

Run tests:

```bash
cargo test -p my-app-core
cargo test -p my-app-adapter-axum
```

## Config Store

For local development, each declared `[stores.config]` id resolves to a
local-file config store backed by `.edgezero/local-config-<id>.json`.
The portable manifest carries no inline defaults — the
pre-rewrite `[stores.config.defaults]` table is gone (see
[the migration guide](../manifest-store-migration.md)).

```toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

```jsonc
// .edgezero/local-config-app_config.json — what `<your-cli> config push` writes.
// The outer object maps the logical store id to ONE BlobEnvelope, JSON-encoded
// as a string (see the blob migration guide for the envelope's fields). `data`
// holds the typed config verbatim; `#[secret]` fields store their key NAMES,
// which the runtime resolves at request time — never the secret values.
{
  "app_config": "{\"version\":1,\"generated_at\":\"…\",\"sha256\":\"…\",\"data\":{\"greeting\":\"hello\",\"feature\":{\"new_checkout\":false},\"service\":{\"timeout_ms\":1500},\"api_token\":\"demo_api_token\",\"vault\":\"default\"}}",
}
```

Typed apps read the whole config in one shot with the `AppConfig<C>` extractor,
which parses the envelope and resolves `#[secret]` fields before handing you `cfg`:

```rust
async fn handler(AppConfig(cfg): AppConfig<MyConfig>) -> Result<Response, EdgeError> {
    let greeting = &cfg.greeting;
    // …
}
```

The lower-level `Config` extractor / `ctx.config_store(id)` exposes the raw
key/value store — `store.get("app_config")` returns the envelope string, and a
hand-seeded flat file returns individual values. Do not pass raw user input
straight to `store.get(…)` in production handlers; validate or allowlist keys
first.

Seed the per-id files with `<your-cli> config push --adapter axum` (the typed
flow — the bundled `edgezero config push` errors), which writes the same
`.edgezero/local-config-<id>.json` files the runtime reads — no shell-out, no
server to authenticate against.

## Container Deployment

Build and deploy as a standard container:

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build -p my-app-adapter-axum --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/my-app-adapter-axum /usr/local/bin/
EXPOSE 8787
CMD ["my-app-adapter-axum"]
```

## Configuration

Configure the Axum adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full
manifest reference.

The `axum.toml` file is used by the Axum CLI helper to locate the crate and display the port.
The runtime currently binds to `127.0.0.1:8787` regardless of the `axum.toml` port value.

## Development Workflow

A typical development workflow:

1. **Run locally**: `edgezero serve --adapter axum`
2. **Make changes** to handlers in `my-app-core`
3. **Test locally** with curl or browser
4. **Run tests**: `cargo test`
5. **Build for edge**: `edgezero build --adapter fastly`
6. **Deploy**: `edgezero deploy --adapter fastly`

## Differences from Edge Adapters

| Aspect      | Axum           | Fastly/Cloudflare |
| ----------- | -------------- | ----------------- |
| Compilation | Native         | Wasm              |
| Cold start  | ~0ms           | ~0ms (Wasm)       |
| Memory      | Unlimited      | 128MB typical     |
| Filesystem  | Full access    | Sandboxed         |
| Network     | Direct         | Backend/fetch     |
| Concurrency | Multi-threaded | Single-threaded   |

::: tip Development Parity
While Axum provides a convenient development environment, always test on actual edge platforms before deploying. Some edge-specific features (KV stores, geolocation) aren't available in the Axum adapter.
:::

## Next Steps

- Deploy to [Fastly Compute](/guide/adapters/fastly) for production
- Deploy to [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative
- Explore [Configuration](/guide/configuration) for manifest options
