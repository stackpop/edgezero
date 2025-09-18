# AnyEdge

Build edge apps in Rust once and run them across providers. AnyEdge supplies a unified HTTP surface, routing, middleware, and adapter crates for Fastly Compute@Edge and Cloudflare Workers, plus tooling for local iteration.

## Highlights

- Provider-agnostic `Request`/`Response`, router, and middleware in `anyedge-core`.
- Adapter crates (`anyedge-fastly`, `anyedge-cloudflare`) that translate provider requests to core types with minimal glue.
- `anyedge-controller` + `anyedge-macros` provide a controller DSL (`#[action]`) with typed extractors (`Path`, `Query`, `Json`, plus `ValidatedPath`/`ValidatedQuery`/`ValidatedJson` for `validator` support), `Hooks`, and the `AppRoutes` builder on top of `anyedge-core`.
- CLI skeleton with a dev server and scaffolding hooks.
- Example apps and demos to validate streaming, headers, and proxy behaviour.

## Repository Layout

- `crates/anyedge-core` – core HTTP types, router, middleware, logging facade.
- `crates/anyedge-fastly` – Fastly adapter + logging/proxy helpers (`fastly` feature).
- `crates/anyedge-cloudflare` – Cloudflare Workers adapter (`cloudflare` feature).
- `crates/anyedge-std` – stdout logger for local/native execution.
- `crates/anyedge-cli` – CLI + dev server + project scaffolding.
- `examples/app-demo` – multi-crate example (core + Fastly + Cloudflare under `crates/`) used by the CLI dev server.

## Core Building Blocks

- `anyedge-controller` + `anyedge-macros` (preferred) – build typed controllers with `#[action]`, extracting params/bodies safely and registering route sets onto an `App`.
- `anyedge_core::App` – the lower-level router/middleware surface. You can still call `app.get/route_with` directly when you need full control.
- Router – path pattern matching (`/users/:id`), verb dispatch, HEAD/OPTIONS semantics, body-mode enforcement.
- HTTP types – provider-neutral headers, query maps, optional streaming bodies (`Response::with_chunks`).
- Logging facade – register once per process (`Logging::set_initializer`, `init_logging`).

### Streaming


`Response::with_chunks` accepts an iterator of `Vec<u8>`; adapters map this to provider streaming primitives. Non-streaming builds fallback to buffering while keeping the API consistent.

### Route Options

- Default helpers (`App::get/post/put/delete`) use automatic body policy.
- `route_with(method, path, handler, RouteOptions)` lets you enforce `streaming()` or `buffered()` behaviour.

## Quick Start

1. Install Rust (stable) and desired provider CLIs (Fastly CLI, Cloudflare `wrangler`).
2. Clone the repo and run `cargo test --workspace` to confirm the toolchain setup.
3. Build an app using controllers (see below), then start the CLI dev server: `cargo run -p anyedge-cli --features cli -- dev` and navigate to http://127.0.0.1:8787.
4. Explore the Fastly or Cloudflare demos as described below.

## Using the Adapters

### Fastly Compute@Edge

```rust
use anyedge_controller::Hooks;
use my_app::DemoApp;

#[fastly::main]
fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    let app = DemoApp::build_app();
    anyedge_fastly::init_logger("<endpoint>", log::LevelFilter::Info, true)?;
    Ok(anyedge_fastly::handle(&app, req))
}
```

- Requires enabling the `fastly` feature on `anyedge-fastly`.
- Supports native streaming on wasm32-wasip1 via `stream_to_client`, with buffered fallback for native builds.
- Proxy helper (`anyedge_fastly::register_proxy`) bridges `anyedge_core::Proxy` to Fastly backends.

### Cloudflare Workers

```rust
use anyedge_cloudflare::handle;
use anyedge_controller::Hooks;
use my_app::DemoApp;

#[event(fetch)]
pub async fn main(req: worker::Request, env: worker::Env, ctx: worker::Context) -> worker::Result<worker::Response> {
    let app = DemoApp::build_app();
    handle(&app, req, env, ctx).await
}
```

- Enable the `cloudflare` feature on `anyedge-cloudflare`.
- Currently buffers responses; streaming integration via `ReadableStream` is on the roadmap.

## Building Apps with Controllers

```rust
use anyedge_controller::{
    action, get, post, AppRoutes, Hooks, Path, RequestJson, Responder, Routes, Text,
};
use anyedge_core::App;

#[derive(serde::Deserialize)]
struct SlugParams {
    slug: String,
}

#[derive(serde::Deserialize)]
struct CreateNote {
    title: String,
}

#[action]
fn list(path: Path<SlugParams>) -> impl Responder {
    let SlugParams { slug } = path.into_inner();
    Text::new(format!("list:{}", slug))
}

#[action]
fn create(RequestJson(body): RequestJson<CreateNote>) -> impl Responder {
    Text::new(format!("created:{}", body.title))
}

pub struct DemoApp;

impl Hooks for DemoApp {
    fn configure(app: &mut App) {
        app.middleware(anyedge_core::middleware::Logger);
    }

    fn routes() -> AppRoutes {
        AppRoutes::with_default_routes()
            .prefix("/api")
            .add_route(
                anyedge_controller::Routes::new()
                    .add("/notes/:slug", get(list()))
                    .add("/notes", post(create())),
            )
    }
}
```

Most users should start with this controller layer and `Hooks`; drop down to `app.route_with(...)` only for specialised behaviour.

`Hooks::configure` runs before routes are applied, giving you a single place to register middleware, shared state, or other app setup.

## CLI

```
# Minimal dev server using the shared example app
cargo run -p anyedge-cli --features cli -- dev

# Scaffold a new edge app (work in progress)
cargo run -p anyedge-cli -- new my-edge-app --dir target/tmp
```

The CLI bundles a Hotwire-free dev server (chunked streaming) and will grow `build` / `deploy` flows.

## Example Apps

### Example Workspace (`examples/app-demo`)

- `app-demo-core`: shared controller-based library with demo routes.
- `app-demo-fastly`: Fastly Compute@Edge binary (`fastly compute serve`).
- `app-demo-cloudflare`: Cloudflare Workers binary (`wrangler dev`).

## Logging

- Fastly: `anyedge_fastly::init_logger(endpoint, LevelFilter, echo_stdout)` integrates with Fastly log streaming.
- Native/local: `anyedge_std::init_logger(LevelFilter, echo_stdout)` outputs to stdout using `fern` formatting.
- For advanced scenarios, register a custom initializer through `anyedge_core::Logging` once per process.

## Development & Testing

- `cargo test --workspace` – run all unit tests.
- `cargo test -p anyedge-core` – focus on core routing/HTTP logic.
- `cargo fmt` and `cargo clippy --workspace` – keep formatting and linting consistent.
- When adding features, mirror coverage in the demos or CLI dev server to prevent adapter drift.

## Roadmap

- Cloudflare streaming support via `ReadableStream`.
- CLI `build`/`deploy` commands that wrap provider tooling.
- `Response::json<T>` helper (serde-gated) for ergonomic responses.
- Expanded proxy APIs (async-friendly) for Workers-style fetch.

See `TODO.md` for detailed design notes and task tracking.
