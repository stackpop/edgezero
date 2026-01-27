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
use edgezero_adapter_axum::AxumDevServer;
use edgezero_core::app::Hooks;
use my_app_core::App;

fn main() {
    let app = App::build_app();
    let router = app.router().clone();
    if let Err(err) = AxumDevServer::new(router).run() {
        eprintln!("axum adapter failed: {err}");
        std::process::exit(1);
    }
}
```

## Development Server

The `edgezero dev` command uses the Axum adapter:

```bash
edgezero dev
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

1. **Start dev server**: `edgezero dev`
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
