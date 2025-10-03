# AnyEdge

AnyEdge is a production-ready toolkit for writing an HTTP workload once and
deploying it across multiple edge providers. The core stays runtime-agnostic so
it compiles cleanly to WebAssembly targets (Fastly Compute@Edge, Cloudflare
Workers) and to native hosts (Axum/Tokio) without code changes.

## Workspace layout

- `crates/anyedge-core` - routing, request/response primitives, middleware chaining, extractor utilities built on `http`, `tower::Service`, and `matchit` 0.8. Handlers opt into extractors such as `Json<T>`, `Path<T>`, and `ValidatedQuery<T>`, and the crate re-exports HTTP types (`Method`, `StatusCode`, `HeaderMap`, ...).
- `crates/anyedge-macros` - procedural macros that power `#[anyedge_core::action]` and related derive helpers.
- `crates/anyedge-adapter-fastly` - Fastly Compute@Edge bridge that maps Fastly request/response types into the shared model and exposes `FastlyRequestContext` plus logging conveniences.
- `crates/anyedge-adapter-cloudflare` - Cloudflare Workers bridge providing `CloudflareRequestContext` and logger bootstrap helpers.
- `crates/anyedge-adapter-axum` - host-side adapter that wraps `RouterService` in Axum/Tokio services for local development and native deployments (the dev server now runs through this crate).
- `crates/anyedge-cli` - CLI for project scaffolding, the local dev server, and adapter-aware build/deploy helpers. Ships with an optional demo dependency.
- `examples/app-demo` - reference application built on the shared router. Includes `crates/app-demo-core` (routes), `crates/app-demo-adapter-fastly` (Fastly binary + `fastly.toml`), `crates/app-demo-adapter-cloudflare` (Workers entrypoint + `wrangler.toml`), and `crates/app-demo-adapter-axum` (native dev server).

## Quick start

```bash
# Install the CLI (from this workspace or a published crate)
cargo install --path crates/anyedge-cli

# Scaffold a new AnyEdge app targeting Fastly, Cloudflare, and Axum
anyedge new my-app --adapters fastly cloudflare axum
cd my-app

# Start the local Axum-powered dev server
anyedge dev

# Hit one of the generated endpoints
curl http://127.0.0.1:8787/echo/alice

# Run your workspace tests
cargo test

# Optional: explore the demo project bundled with this repo
cargo run -p anyedge-cli -- dev
```

The CLI enables the `dev-example` feature by default, so running `anyedge dev` inside this repository boots the demo router from `examples/app-demo`. Disable the example dependency with `cargo run -p anyedge-cli --no-default-features --features cli -- dev` to spin up a stub router instead.

The demo routes showcase core features:

- `/` - static response to verify the app is running.
- `/echo/{name}` - path parameter extraction.
- `/headers` - direct `RequestContext` access.
- `/stream` - streaming bodies via `Body::stream`.
- `POST /echo` - JSON extractor + response builder.
- `/info` - shared state injection.

Handlers stay concise by using the `#[action]` macro re-exported from `anyedge-core`:

```rust
use anyedge_core::action;
use anyedge_core::extractor::Json;
use anyedge_core::response::Text;

#[derive(serde::Deserialize)]
struct EchoBody {
    name: String,
}

#[action]
async fn echo_json(Json(body): Json<EchoBody>) -> Text<String> {
    Text::new(format!("Hello, {}!", body.name))
}
```

## CLI tooling

The CLI and adapters now expect an `anyedge.toml` manifest alongside your workspace. The manifest
describes the shared app (entry crate, optional middleware list, routes, adapters, and logging)
so Fastly/Cloudflare binaries, the CLI, and any local tooling all agree on configuration. The demo
manifest lives in `examples/app-demo/anyedge.toml`, and the scaffolder emits the same structure for
new projects.

The `anyedge-cli` crate produces the `anyedge` binary (enabled by the `cli` feature). Run it locally with `cargo run -p anyedge-cli -- <command>`. Key subcommands:

- `anyedge new` - scaffolds a fully wired workspace (pass `--adapters` to pick your targets).
- `anyedge dev` - starts the local Axum HTTP server (uses the demo router when `dev-example` is enabled).
- `anyedge build --adapter fastly` - builds the Fastly example to `wasm32-wasip1` and copies the artifact into `anyedge/pkg/`.
- `anyedge serve --adapter fastly` - shells out to `fastly compute serve` after locating the Fastly manifest.
- `anyedge deploy --adapter fastly` - wraps `fastly compute deploy`.
- `anyedge build --adapter axum` - builds your native entrypoint (useful for containers or local integration tests).
- `anyedge serve --adapter axum` - runs the generated Axum entrypoint with `cargo run`, ideal for local or containerised development.

Adapters register themselves lazily through their `anyedge_adapter_*::cli` modules. With the Axum adapter available you can generate, serve, and test a native host target without leaving the workspace.

## Logging

`anyedge-core` relies on the standard `log` facade. Platform adapters expose helper
functions so you can install the right backend when your app boots:

- Fastly: call `anyedge_adapter_fastly::init_logger()` (wraps `log_fastly`).
- Cloudflare Workers: call `anyedge_adapter_cloudflare::init_logger()` (logs via
  Workers `console_log!`).
- Axum/native: install a standard logger (`simple_logger`, `tracing-subscriber`, etc.) before booting `anyedge_adapter_axum::AxumDevServer`.
- Other targets: initialise a fallback logger such as `simple_logger` before building
  your app.

The helper `run_app::<App>(include_str!("path/to/anyedge.toml"), req)` in
`anyedge-adapter-fastly` and the Cloudflare equivalent encapsulate manifest loading and logger
initialisation, so the adapters you scaffold only need to call the helper from `main`.

## Provider builds

Fastly Compute@Edge (requires the `fastly` CLI and the `wasm32-wasip1` target):

```bash
rustup target add wasm32-wasip1
cd anyedge/examples/app-demo
cargo build -p app-demo-adapter-fastly --target wasm32-wasip1 --features fastly
# or from the workspace root:
cargo run -p anyedge-cli -- build --adapter fastly
cargo run -p anyedge-cli -- serve --adapter fastly
```

The CLI helpers locate `fastly.toml`, build the Wasm artifact, place it in `anyedge/pkg/`, and run `fastly compute serve` from `examples/app-demo/crates/app-demo-adapter-fastly`.

Cloudflare Workers (requires `wrangler` and the `wasm32-unknown-unknown` target):

```bash
rustup target add wasm32-unknown-unknown
cd anyedge/examples/app-demo
cargo build -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown
wrangler dev --config crates/app-demo-adapter-cloudflare/wrangler.toml
```

Axum / native hosts:

```bash
# Build or run using the scaffolded commands
anyedge build --adapter axum
anyedge serve --adapter axum
```

The Fastly and Cloudflare adapters translate provider request/response shapes into the shared `anyedge-core` model and stash provider metadata in the request extensions so handlers can reach runtime-specific APIs.

## Path parameters

`anyedge-core` uses matchit 0.8+. Define parameters with `{name}` segments
(`/blog/{slug}`) and catch-alls with `{*rest}`. Legacy Axum-style `:name`
segments are intentionally unsupported.

## Route listing

Enable `RouterBuilder::enable_route_listing()` when you want a quick view of the
registered routes. It injects a JSON endpoint at
`DEFAULT_ROUTE_LISTING_PATH` (defaults to `/__anyedge/routes`) that returns an
array of `{ "method": "GET", "path": "/..." }` entries for every handler.
Use `RouterBuilder::enable_route_listing_at("/debug/routes")` to expose the
listing at a custom path.

## Streaming responses

Handlers can return `Body::stream` to yield response chunks progressively. The
router keeps the stream intact all the way to the adapters; the Fastly and
Cloudflare bridges buffer chunks sequentially while writing to the provider
runtime APIs, so long-lived streams remain compatible with Wasm targets. Responses compressed with gzip or brotli are transparently
decoded before they reach handlers so you can reformat or transform the
payload before sending it downstream.

## Proxying upstream services

`anyedge-core` ships with `ProxyRequest`, `ProxyResponse`, and the `ProxyService<C>` wrapper so edge adapters can forward traffic while reusing the same handler logic:

```rust
use anyedge_core::http::Uri;
use anyedge_core::proxy::{ProxyRequest, ProxyService};

let target: Uri = "https://example.com/api".parse()?;
let proxy_request = ProxyRequest::from_request(request, target);
let response = ProxyService::new(client).forward(proxy_request).await?;
```

Use the adapter-specific clients (`anyedge_adapter_fastly::FastlyProxyClient` and `anyedge_adapter_cloudflare::CloudflareProxyClient`) when compiling for those adapters, and swap in lightweight test clients during unit tests. The proxy helpers preserve streaming bodies and transparently decode gzip or brotli payloads before they reach your handler.

## Testing

Unit tests live next to the modules they exercise. Run the entire suite with
`cargo test`, or scope to a single crate via `cargo test -p anyedge-core`.
The adapter crates include lightweight host-side tests that validate context
insertion and URI parsing without needing the Wasm toolchains.

### Wasm runners (Fastly / WASI)

Some adapter tests target `wasm32-wasip1`; Cargo needs a Wasm runtime to execute
the generated binaries. Install the following tools before running those tests:

- Wasmtime (executes `wasm32-wasip1` tests)
  - macOS: `brew install wasmtime`
  - Linux: `curl https://wasmtime.dev/install.sh -sSf | bash`
  - Windows: follow <https://wasmtime.dev/> for the MSI/winget installers
- Viceroy (Fastly’s local Compute@Edge simulator)
  - macOS: `brew install fastly/tap/viceroy`
  - Linux & Windows: download the latest release archive from
    <https://github.com/fastly/Viceroy/releases>, extract it, and place the
    binary on your `PATH`

Tell Cargo to use Wasmtime when running the wasm tests:

```bash
export CARGO_TARGET_WASM32_WASIP1_RUNNER="wasmtime run --dir=."
cargo test -p anyedge-adapter-fastly --features fastly --target wasm32-wasip1
```

Streaming responses are covered in the `anyedge-core` router tests and in the
Fastly adapter tests to ensure chunked bodies make it to the provider output.

## Next steps

- Expand multi-provider support:
  - Finalise Cloudflare streaming/backpressure behaviour and ship the demo Wranger flow.
  - Extend the CLI build/deploy commands so both Fastly and Cloudflare share the same UX.
- Finish out the manifest-driven ergonomics by adding helpers such as `Response::json<T>` and static-asset serving utilities to `anyedge-core`.
- Harden CI (fmt/clippy/test) and feature matrices using the new GitHub workflows.
- Grow the adapter contract documentation/tests to keep additional edge targets aligned.
  for Fastly/Cloudflare (`crates/anyedge-adapter-fastly/tests/contract.rs`,
  `crates/anyedge-adapter-cloudflare/tests/contract.rs`), and introduced the
  `anyedge.toml` schema for application manifests (`docs/manifest.md`).
