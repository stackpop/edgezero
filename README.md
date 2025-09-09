# AnyEdge

Write in Rust. Run on every edge.

This repo is an early scaffold for the AnyEdge framework. It defines core abstractions for provider-agnostic edge apps, provider adapters, a CLI skeleton, and example apps.

## Workspace Layout

- `crates/anyedge-core`: Request/Response, Router, Middleware, App.
- `crates/anyedge-fastly`: Fastly adapter (enable `fastly` feature).
- `crates/anyedge-cloudflare`: Cloudflare Workers adapter (enable `workers` feature).
- `crates/anyedge-std`: Simple stdout logger for local/native targets.
- `crates/anyedge-cli`: CLI skeleton (default features: `cli` + `dev-example`).
- `examples/app-lib`: Shared example app library for local dev server.
- `examples/anyedge-fastly-demo`: Fastly Compute@Edge demo using the adapter.
- `examples/anyedge-cloudflare-demo`: Cloudflare Workers demo using the adapter.

## Core Abstractions

- Request/Response: minimal types that map well to edge providers.
- Router: path pattern matching with `:params` and HTTP method dispatch.
- Middleware: synchronous, chainable; example `Logger` included.
- App: composes middleware + router; `handle(Request) -> Response`.
- Routes: use `get/post/put/delete` for defaults, or `route_with(method, path, handler, RouteOptions)` for advanced options.
- Streaming: `Response::with_chunks` streams chunks via an iterator (dev server uses HTTP/1.1 chunked). Fastly uses native streaming (`stream_to_client`) on wasm32‑wasi.

## Dev App Library

The shared example library provides a simple `build_app()` for local iteration with the CLI dev server.
It is not required for production apps, but helps in this repo.

Routes include:
- `/` → Hello text
- `/echo/:name` → Greets the path param
- `/headers` → Reflects `User-Agent`
- `/stream` → Streams a few text chunks (chunked transfer encoding)

Example streaming route with options:

```
use anyedge_core::{Method};
use anyedge_core::app::RouteOptions;

app.route_with(Method::GET, "/stream", |_req| {
    let chunks = (0..5).map(|i| format!("chunk {}\n", i).into_bytes());
    anyedge_core::Response::ok()
        .with_header("Content-Type", "text/plain; charset=utf-8")
        .with_chunks(chunks)
}, RouteOptions::streaming());
```

## Provider Targets

- Fastly: translate Fastly `Request/Response` and call `app.handle()`.
  - In a Fastly bin crate:
    - `#[fastly::main] fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {`
    - `  let app = /* build your AnyEdge App */;`
    - `  Ok(anyedge_fastly::handle(&app, req))`
    - `}`
- Cloudflare Workers: translate `worker::Request/Response` and call `app.handle()` (feature `workers`).
  - In a Workers project (using `worker` crate):
    ```rust
    async fn main(req: worker::Request, env: worker::Env, ctx: worker::Context) -> worker::Result<worker::Response> {
        let app = build_app();
        anyedge_cloudflare::handle(&app, req, env, ctx).await
    }
    ```

## Fastly Demo

- Example project: `examples/anyedge-fastly-demo`
- Files:
  - Rust bin: `examples/anyedge-fastly-demo/src/main.rs`
  - Fastly config: `examples/anyedge-fastly-demo/fastly.toml`

Prereqs:
- Install Fastly CLI (`brew install fastly/tap/fastly`) and login (`fastly login`).
- Rust toolchain for Compute@Edge (wasm32-wasip1) is handled by the CLI.

Local dev:
- `cd examples/anyedge-fastly-demo`
- `fastly compute serve`

Routes available:
- `GET /` → AnyEdge Fastly Demo
- `GET /echo/:name` → Greets the name
- `GET /headers` → Echoes `User-Agent` and `Host`
- `GET /ip` → Shows `client_ip` derived from Fastly request
- `GET /version` → Shows `FASTLY_SERVICE_VERSION`
- `GET /health` → ok
- `GET /stream` → Emits a few text chunks (currently buffered on Fastly)

Note: The Fastly adapter streams natively on wasm32‑wasip1 via `stream_to_client`. In non‑wasm builds (e.g., workspace checks), it falls back to a buffered path for testability. Cloudflare Workers mapping is buffered for now; streaming via ReadableStream is a follow‑up.

Publish (create service once, then ship):
- Create a service: `fastly service create --name anyedge-fastly-demo`
- Put the `service_id` into `fastly.toml` or pass `--service-id`.
- `fastly compute publish --service-id <SERVICE_ID>`

Notes:
- The demo app doesn’t need backends. If your app does, configure them in `fastly.toml` or via CLI.
- The adapter maps Fastly’s headers/body to AnyEdge and back, preserving duplicates and binary bodies.
- Logging: explicitly initialize Fastly logging: `anyedge_fastly::init_logger("tslog", log::LevelFilter::Info, true)`; set `ANYEDGE_FASTLY_LOG_ENDPOINT` to configure endpoint at runtime.

## Cloudflare Demo

- Example project: `examples/anyedge-cloudflare-demo`
- Files:
  - Rust bin: `examples/anyedge-cloudflare-demo/src/main.rs`
  - Cargo target: `examples/anyedge-cloudflare-demo/.cargo/config.toml` (`wasm32-unknown-unknown`)
  - Wrangler config: `examples/anyedge-cloudflare-demo/wrangler.toml`

Prereqs:
- Install Wrangler: `npm i -g wrangler` (or use Cloudflare’s installer)
- Rust toolchain with `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

Local dev:
- `cd examples/anyedge-cloudflare-demo`
- `wrangler dev`

Notes:
- This demo uses buffered responses; streaming via `ReadableStream` is a follow‑up.

## Logging

- Preferred: initialize the provider logger directly once at startup.
  - Fastly: `anyedge_fastly::init_logger("tslog", log::LevelFilter::Info, true)?;`
  - Local/native: `anyedge_std::init_logger(log::LevelFilter::Info, true)?;`
- Advanced: a provider‑agnostic facade exists for custom registration flows:
  - `anyedge_core::Logging::{set_initializer, init_logging, init_with}`

## Route Options

- Default routes (`get/post/put/delete`) use `Auto` body policy.
- `route_with(method, path, handler, RouteOptions)` lets you choose:
  - `RouteOptions::streaming()` → enforce streaming; buffered bodies are coerced to streaming (no Content‑Length).
  - `RouteOptions::buffered()` → disallow streaming; if a handler returns streaming, the router returns HTTP 500.

## CLI (skeleton)

```
# Run CLI minimal dev server using the shared example app (defaults on)
cargo run -p anyedge-cli -- dev
# Visit http://127.0.0.1:8787

# Scaffold a new app into a directory
cargo run -p anyedge-cli -- new my-edge-app --dir target/tmp
```

Upcoming commands: `new`, `build`, `deploy fastly|cloudflare`. The `dev` server is dependency-free and intended for local iteration.

## Next Steps

- Implement Fastly adapter: headers/body mapping, streaming considerations.
- Implement Cloudflare adapter: Workers request/response mapping, streaming behavior.
- Add local dev server (feature-gated) using Hyper for quick iteration.
- Define deploy flows in CLI (Fastly API, AWS SAM/CDK or native tooling).

## Testing

- Run core and example tests:
  - `cargo test`
- Shared dev app tests:
  - `cargo test -p anyedge-app-lib`

See `TODO.md` for roadmap and open design questions.
