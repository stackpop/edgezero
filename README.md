# AnyEdge Prototype

AnyEdge is an experiment in writing HTTP workloads once and deploying them to
multiple edge providers. The crates in this workspace stay runtime-agnostic
(Tokio free, no OS primitives) so they can compile cleanly to WebAssembly
targets such as Fastly Compute@Edge and Cloudflare Workers.

## Workspace layout

- `crates/anyedge-core` – routing, request/response primitives, middleware
  chaining, extractor utilities built on `http`, `tower::Service`, and
  `matchit`, plus a `anyedge-macros` companion crate for the `#[action]`
  attribute. Handlers use matchit 0.8’s `{param}` syntax (for example
  `/hello/{name}`) and can opt into extractor arguments like `Json<T>` or
  `ValidatedQuery<T>`. The crate re-exports common HTTP primitives (`Method`,
  `StatusCode`, `HeaderMap`, etc.) so downstream code can avoid depending on the
  underlying `http` version directly.
- `crates/anyedge-adapter-fastly` – converts Fastly request/response types into
  the shared `anyedge-core` model and exposes a `FastlyRequestContext` with
  provider metadata.
- `crates/anyedge-adapter-cloudflare` – the Cloudflare Workers counterpart,
  returning a `CloudflareRequestContext` so handlers can reach the Workers `Env`
  and `Context`.
- `examples/anyedge-demo/anyedge-demo-core` – shared router plus a
  `anyedge-demo-local` binary for quick local iteration.
- `examples/anyedge-demo/anyedge-demo-fastly` – Fastly deployment target
  that reuses the shared router through the adapter crate.
- `examples/anyedge-demo/anyedge-demo-cloudflare` – Cloudflare Workers entry
  point driven by the same router.

## Quick start

```bash
# Run the local demo handlers against a few synthetic requests
cargo run -p anyedge-demo-core --bin anyedge-demo-local

# Execute the full test suite
cargo test
```

The local demo now includes a `/stream` route that yields multiple chunks to
illustrate streaming bodies end-to-end, and showcases the macro/extractor flow:

```rust
#[derive(serde::Deserialize)]
struct EchoBody {
    name: String,
}

#[anyedge_core::action]
async fn echo(Json(payload): Json<EchoBody>) -> impl Responder {
    Text::new(format!("Hello, {}!", payload.name))
}
```

## Logging

`anyedge-core` relies on the standard `log` facade. Platform adapters expose helper
functions so you can install the right backend when your app boots:

- Fastly: call `anyedge_adapter_fastly::init_logger()` (wraps `log_fastly`).
- Cloudflare Workers: call `anyedge_adapter_cloudflare::init_logger()` (logs via
  Workers `console_log!`).
- Other targets: initialise a fallback logger such as `simple_logger` before building
  your app.

The demo hooks call those helpers automatically so the `anyedge_demo_*` binaries pick
up the appropriate logger without extra boilerplate.

## Provider builds

Fastly Compute@Edge (requires `wasm32-wasip1` target and the `fastly` SDK):

```bash
rustup target add wasm32-wasip1
cd anyedge/crates/anyedge-adapter-fastly
cargo build -p anyedge-demo-fastly --features fastly --target wasm32-wasip1
# Serve locally with the Fastly CLI (requires running from the demo crate so `fastly.toml` is found)
fastly compute serve -C examples/app-demo/crates/app-demo-adapter-fastly \
```

Cloudflare Workers (requires `wasm32-unknown-unknown` target):

```bash
rustup target add wasm32-unknown-unknown
cd anyedge/crates/anyedged-adapter-cloudflare
cargo build -p anyedge-demo-cloudflare --features cloudflare --target wasm32-unknown-unknown
```

Both binaries rely on their adapter crates to translate platform request/response
shapes and to stash provider metadata in the request extensions so handlers can
access platform-specific APIs.

## Path parameters

`anyedge-core` uses matchit 0.8+. Define parameters with `{name}` segments
(`/blog/{slug}`) and catch-alls with `{*rest}`. Legacy Axum-style `:name`
segments are intentionally unsupported.

## Streaming responses

Handlers can return `Body::stream` to yield response chunks progressively. The
router keeps the stream intact all the way to the adapters; today both the
Fastly and Cloudflare bridges buffer chunks sequentially while writing to the
provider runtime APIs, so long-lived streams remain compatible with Wasm
targets. Responses compressed with gzip or brotli are transparently
decoded before they reach handlers so you can reformat or transform the
payload before sending it downstream.

## Proxying upstream services

`anyedge-core` exposes a `ProxyService` abstraction that adapters implement.
On Fastly it uses dynamic backends (created at runtime) while on Cloudflare it
relies on the Workers `fetch` API. Both paths support streaming and gzip/brotli
decoding out of the box, making it easy to transform proxied responses.

The demo crate now includes reusable helpers — `proxy_to` and `proxy_to_with`
— that wrap `ProxyRequest::from_request` and the correct `ProxyClient` for the
current compile target. The helpers make it trivial to adjust headers or other
per-request metadata before forwarding.

- `GET /proxy` proxies to a fixed upstream URL to showcase the happy path.
- `GET /proxy/header?header=value` injects a custom header before forwarding,
  demonstrating how to use `proxy_to_with` for on-the-fly request decoration.
- The local build uses a synthetic proxy client so you can iterate without edge
  credentials. Fastly / Cloudflare builds automatically pick up the matching
  proxy client when compiled with `fastly` or `cloudflare`.

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

- Flesh out provider adapters with richer request contexts (fetch/KV helpers).
- Offer dedicated extractor types (`Path<T>`, `Query<T>`, `Json<T>`) on top of
  `RequestContext` for ergonomic handlers.
- Prototype a local development shim so the same router can process real HTTP
  traffic on a laptop while mimicking Fastly/Cloudflare behaviours.
