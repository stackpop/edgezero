AnyEdge Context (Save for Restart)
=================================

Key Decisions & APIs
--------------------
- Routing API:
  - Default helpers: `App::get/post/put/delete` (BodyMode::Auto)
  - Advanced: `App::route_with(method, path, handler, RouteOptions)`
    - `RouteOptions::streaming()` → force streaming (coerces buffered)
    - `RouteOptions::buffered()` → disallow streaming (500 on streaming handlers)
- Streaming model:
  - Core `Response` streams via `stream: Option<Box<dyn Iterator<Item = Vec<u8>> + Send>>` (no buffering)
  - Dev server: HTTP/1.1 chunked from iterator
  - Fastly adapter: native streaming on wasm32‑wasi via `stream_to_client`; buffered fallback on non‑wasm builds
- Logging:
  - Preferred: call provider initializer directly once at startup
    - Fastly: `anyedge_fastly::init_logger(endpoint, LevelFilter, echo_stdout)`
    - Stdout (local/native): `anyedge_std::init_logger(LevelFilter, echo_stdout)`
  - Core facade remains available: `anyedge_core::Logging` (register/once init) for advanced cases

File Map (Where to Look)
------------------------
- Core:
  - Routing + policy: `crates/anyedge-core/src/router.rs`
  - App API + docs: `crates/anyedge-core/src/app.rs`
  - HTTP types + streaming: `crates/anyedge-core/src/http.rs`
  - Logging facade: `crates/anyedge-core/src/logging.rs`
- Fastly:
  - Adapter entry (`handle`): `crates/anyedge-fastly/src/app.rs`
  - HTTP mapping: `crates/anyedge-fastly/src/http.rs`
  - Provider logging: `crates/anyedge-fastly/src/logging.rs`
  - Proxy/backends: register handler `crates/anyedge-fastly/src/proxy.rs`
- Cloudflare (experimental):
  - Adapter entry (`handle`): `crates/anyedge-cloudflare/src/app.rs`
  - HTTP mapping: `crates/anyedge-cloudflare/src/http.rs`
  - Feature gate: `workers`
  - Proxy/backends: stub `crates/anyedge-cloudflare/src/proxy.rs` (async TBD)
- Stdout logger (local/native):
  - `crates/anyedge-std` → `init_logger(LevelFilter, echo_stdout)`
- CLI dev server (chunked streaming): `crates/anyedge-cli/src/main.rs`
- Examples:
  - Dev app-lib (+ `/stream`): `examples/app-lib/src/lib.rs`
  - Fastly demo (+ `/stream`): `examples/anyedge-fastly-demo/src/main.rs`
  - Demo target: `examples/anyedge-fastly-demo/.cargo/config.toml` (wasm32-wasip1)

How to Run
----------
- Core tests: `cargo test -p anyedge-core`
- Workspace check: `cargo check --workspace`
- Dev server: `cargo run -p anyedge-cli -- dev` → http://127.0.0.1:8787 (`/stream` supported)
- Fastly demo:
  - `cd examples/anyedge-fastly-demo`
  - `fastly compute serve`
  - Env: `ANYEDGE_FASTLY_LOG_ENDPOINT=<endpoint>`
- Cloudflare: mapping implemented; add a minimal example with `wrangler` in a follow‑up.

Policies & Behaviors
--------------------
- HEAD clears both buffered and streaming bodies
- Streaming route + buffered handler → coerced to streaming (no Content‑Length)
- Buffered route + streaming handler → HTTP 500 (“Streaming not allowed for this route”)

Next Pickup (Sync with TODO.md)
-------------------------------
- CLI Fastly: `anyedge build` (package) and `anyedge deploy` (publish)
- Fastly demo: backend example to validate `req.send("backend")`
- Core: `Response::json<T: Serialize>` (serde‑gated) and doc snippet
- Docs: brief note on `http` crate mapping (Method/Status/Headers) and route options in README
- Cloud provider: Cloudflare Workers example + streaming behavior
- Core proxy: add async facade (feature‑gated) to support async fetch on Workers
- CI: `fmt`, `clippy`, and tests; add target cfg for demo to avoid native failures
