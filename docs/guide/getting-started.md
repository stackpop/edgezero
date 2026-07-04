# Getting Started

This guide walks you through creating your first EdgeZero application.

## Prerequisites

- Rust toolchain (stable; see `.tool-versions` in the repo)
- For Fastly: `wasm32-wasip1` target and the Fastly CLI
- For Cloudflare: `wasm32-unknown-unknown` target and Wrangler
- For Spin: `wasm32-wasip2` target and the [Spin CLI](https://spinframework.dev/)

## Installation

Install the EdgeZero CLI from the workspace or a published crate:

```bash
cargo install --path crates/edgezero-cli
```

## Create a New Project

Scaffold a new EdgeZero app:

```bash
edgezero new my-app
cd my-app
```

This generates a workspace with:

- `crates/my-app-core` - Your shared handlers, routing logic, and the typed `MyAppConfig` struct in `src/config.rs`
- `crates/my-app-cli` - Your project's own CLI binary, built on the `edgezero-cli` library
- `crates/my-app-adapter-fastly` - Fastly Compute entrypoint
- `crates/my-app-adapter-cloudflare` - Cloudflare Workers entrypoint
- `crates/my-app-adapter-axum` - Native Axum entrypoint
- `crates/my-app-adapter-spin` - Fermyon Spin entrypoint
- `edgezero.toml` - Manifest describing routes, middleware, and adapter config
- `my-app.toml` - Typed application config matching the `MyAppConfig` struct (see [Application config](/guide/configuration#application-config))

As part of `edgezero new`, the scaffolder runs `provision --adapter <name> --local`
for every selected adapter, so the fresh workspace already has each adapter's
baseline local manifest (`wrangler.toml`, `fastly.toml`, `spin.toml` +
`runtime-config.toml`) and its accompanying `.env` / `.dev.vars` /
`.edgezero/.env` files written out. Axum's `axum.toml` is scaffolded from
the template (not by `provision`) and stays tracked. You don't need to run
`provision` manually before the first `serve`.

::: tip Cloudflare / Fastly / Spin manifests are gitignored
The generated `.gitignore` excludes `wrangler.toml`, `fastly.toml`,
`spin.toml`, `runtime-config.toml`, `.dev.vars`, `.env`, and `.edgezero/`
— teammates regenerate them by running `my-app-cli provision --adapter <name> --local`
after cloning. **Axum's `axum.toml` stays tracked** because it's the
operator-authored manifest for the native dev server; `provision` never
touches it.
:::

## Run Your App Locally

Run your generated app on the native Axum adapter:

```bash
edgezero serve --adapter axum
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
├── my-app.toml             # Typed application config (loaded into MyAppConfig)
├── crates/
│   ├── my-app-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs      # App definition with edgezero_core::app!
│   │       ├── config.rs   # MyAppConfig with #[derive(AppConfig)]
│   │       └── handlers.rs # Your route handlers
│   ├── my-app-cli/
│   │   ├── Cargo.toml
│   │   └── src/main.rs     # Your project's CLI, built on edgezero-cli
│   ├── my-app-adapter-fastly/
│   │   ├── Cargo.toml
│   │   ├── fastly.toml
│   │   └── src/main.rs
│   ├── my-app-adapter-cloudflare/
│   │   ├── Cargo.toml
│   │   ├── wrangler.toml
│   │   └── src/main.rs
│   ├── my-app-adapter-axum/
│   │   ├── Cargo.toml
│   │   ├── axum.toml
│   │   └── src/main.rs
│   └── my-app-adapter-spin/
│       ├── Cargo.toml
│       ├── spin.toml
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
