# Getting Started

This guide walks you through creating your first EdgeZero application.

## Prerequisites

- Rust toolchain (1.70+)
- For Fastly: `wasm32-wasip1` target and the Fastly CLI
- For Cloudflare: `wasm32-unknown-unknown` target and Wrangler

## Installation

Install the EdgeZero CLI from the workspace or a published crate:

```bash
cargo install --path crates/edgezero-cli
```

## Create a New Project

Scaffold a new EdgeZero app targeting your preferred adapters:

```bash
# Create an app with Fastly, Cloudflare, and Axum adapters
edgezero new my-app --adapters fastly cloudflare axum
cd my-app
```

This generates a workspace with:
- `crates/my-app-core` - Your shared handlers and routing logic
- `crates/my-app-adapter-fastly` - Fastly Compute entrypoint
- `crates/my-app-adapter-cloudflare` - Cloudflare Workers entrypoint
- `crates/my-app-adapter-axum` - Native Axum entrypoint
- `edgezero.toml` - Manifest describing routes, middleware, and adapter config

## Start the Dev Server

Run the local Axum-powered development server:

```bash
edgezero dev
```

Your app is now running at `http://127.0.0.1:8787`. Try the generated endpoints:

```bash
# Root endpoint
curl http://127.0.0.1:8787/

# Path parameter extraction
curl http://127.0.0.1:8787/echo/alice

# JSON echo
curl -X POST http://127.0.0.1:8787/echo \
  -H "Content-Type: application/json" \
  -d '{"name": "Bob"}'
```

## Project Structure

A scaffolded project looks like this:

```
my-app/
├── Cargo.toml              # Workspace manifest
├── edgezero.toml           # EdgeZero configuration
├── crates/
│   ├── my-app-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs      # App definition with edgezero_core::app!
│   │       └── handlers.rs # Your route handlers
│   ├── my-app-adapter-fastly/
│   │   ├── Cargo.toml
│   │   ├── fastly.toml
│   │   └── src/main.rs
│   ├── my-app-adapter-cloudflare/
│   │   ├── Cargo.toml
│   │   ├── wrangler.toml
│   │   └── src/main.rs
│   └── my-app-adapter-axum/
│       ├── Cargo.toml
│       ├── axum.toml
│       └── src/main.rs
```

## Writing Your First Handler

Handlers use the `#[action]` macro for ergonomic extractor support:

```rust
use edgezero_core::action;
use edgezero_core::extractor::Json;
use edgezero_core::response::Text;

#[derive(serde::Deserialize)]
struct EchoBody {
    name: String,
}

#[action]
async fn echo_json(Json(body): Json<EchoBody>) -> Text<String> {
    Text::new(format!("Hello, {}!", body.name))
}
```

## Running Tests

Run your workspace tests with:

```bash
cargo test
```

## Next Steps

- Learn about [Routing](/guide/routing) to define your endpoints
- Explore [Handlers & Extractors](/guide/handlers) for type-safe request handling
- Deploy to [Fastly](/guide/adapters/fastly) or [Cloudflare](/guide/adapters/cloudflare)
