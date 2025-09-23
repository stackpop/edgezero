# AnyEdge Prototype

AnyEdge is an experiment in writing HTTP workloads once and deploying them to
multiple edge providers. The crates in this workspace stay runtime-agnostic
(Tokio free, no OS primitives) so they can compile cleanly to WebAssembly
targets such as Fastly Compute@Edge and Cloudflare Workers.

## Workspace layout

- `crates/anyedge-core` - routing, request/response primitives, middleware chaining, extractor utilities built on `http`, `tower::Service`, and `matchit` 0.8. Handlers opt into extractors such as `Json<T>`, `Path<T>`, and `ValidatedQuery<T>`, and the crate re-exports HTTP types (`Method`, `StatusCode`, `HeaderMap`, ...).
- `crates/anyedge-macros` - procedural macros that power `#[anyedge_core::action]` and related derive helpers.
- `crates/anyedge-adapter-fastly` - Fastly Compute@Edge bridge that maps Fastly request/response types into the shared model and exposes `FastlyRequestContext` plus logging conveniences.
- `crates/anyedge-adapter-cloudflare` - Cloudflare Workers bridge providing `CloudflareRequestContext` and logger bootstrap helpers.
- `crates/anyedge-cli` - CLI for project scaffolding, the local dev server, and provider-aware build/deploy helpers. Ships with an optional demo dependency.
- `examples/app-demo` - reference application built on the shared router. Includes `crates/app-demo-core` (routes), `crates/app-demo-adapter-fastly` (Fastly binary + `fastly.toml`), and `crates/app-demo-adapter-cloudflare` (Workers entrypoint + `wrangler.toml`).

## Quick start

```bash
# Launch the built-in dev server (serves the demo router on http://127.0.0.1:8787)
cargo run -p anyedge-cli -- dev

# Exercise the demo endpoints
curl http://127.0.0.1:8787/echo/alice
curl -sS -X POST http://127.0.0.1:8787/echo \
  -H 'content-type: application/json' \
  -d '{"name":"Edge"}'

# Execute the full test suite
cargo test
```

The CLI enables the `dev-example` feature by default, so `anyedge dev` boots the demo router from `examples/app-demo`. Disable the example dependency with `cargo run -p anyedge-cli --no-default-features --features cli -- dev` to spin up a stub router instead.

The demo routes showcase core features:

- `/` - static response to verify the app is running.
- `/echo/{name}` - path parameter extraction.
- `/headers` - direct `RequestContext` access.
- `/stream` - streaming bodies via `Body::stream`.
- `POST /echo` - JSON extractor + response builder.
- `/info` - shared state injection.

Handlers stay concise by using the `#[action]` macro re-exported from `anyedge-core`:

```rust
use anyedge_core::{action, Json, Text};

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

The `anyedge-cli` crate produces the `anyedge` binary (enabled by the `cli` feature). Run it locally with `cargo run -p anyedge-cli -- <command>`. Key subcommands:

- `anyedge dev` - starts the local HTTP server (uses the demo router when `dev-example` is enabled).
- `anyedge build --provider fastly` - builds the Fastly example to `wasm32-wasip1` and copies the artifact into `anyedge/pkg/`.
- `anyedge serve --provider fastly` - shells out to `fastly compute serve` after locating the Fastly manifest.
- `anyedge deploy --provider fastly` - wraps `fastly compute deploy`.

Fastly is the only provider wired into the CLI today; add new providers by extending `anyedge_adapter_*::cli`.

## Logging

`anyedge-core` relies on the standard `log` facade. Platform adapters expose helper
functions so you can install the right backend when your app boots:

- Fastly: call `anyedge_adapter_fastly::init_logger()` (wraps `log_fastly`).
- Cloudflare Workers: call `anyedge_adapter_cloudflare::init_logger()` (logs via
  Workers `console_log!`).
- Other targets: initialise a fallback logger such as `simple_logger` before building
  your app.

The demo adapters call those helpers automatically so the Fastly and Cloudflare binaries pick up the appropriate logger without extra boilerplate.

## Provider builds

Fastly Compute@Edge (requires the `fastly` CLI and the `wasm32-wasip1` target):

```bash
rustup target add wasm32-wasip1
cd anyedge/examples/app-demo
cargo build -p app-demo-adapter-fastly --target wasm32-wasip1 --features fastly
# or from the workspace root:
cargo run -p anyedge-cli -- build --provider fastly
cargo run -p anyedge-cli -- serve --provider fastly
```

The CLI helpers locate `fastly.toml`, build the Wasm artifact, place it in `anyedge/pkg/`, and run `fastly compute serve` from `examples/app-demo/crates/app-demo-adapter-fastly`.

Cloudflare Workers (requires `wrangler` and the `wasm32-unknown-unknown` target):

```bash
rustup target add wasm32-unknown-unknown
cd anyedge/examples/app-demo
cargo build -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown
wrangler dev --config crates/app-demo-adapter-cloudflare/wrangler.toml
```

Both adapters translate provider request/response shapes into the shared `anyedge-core` model and stash provider metadata in the request extensions so handlers can reach runtime-specific APIs.

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

`anyedge-core` ships with `ProxyRequest`, `ProxyResponse`, and the `ProxyService<C>` wrapper so edge adapters can forward traffic while reusing the same handler logic:

```rust
use anyedge_core::{ProxyRequest, ProxyService, Uri};

let target: Uri = "https://example.com/api".parse()?;
let proxy_request = ProxyRequest::from_request(request, target);
let response = ProxyService::new(client).forward(proxy_request).await?;
```

Use the adapter-specific clients (`anyedge_adapter_fastly::FastlyProxyClient` and `anyedge_adapter_cloudflare::CloudflareProxyClient`) when compiling for those providers, and swap in lightweight test clients during unit tests. The proxy helpers preserve streaming bodies and transparently decode gzip or brotli payloads before they reach your handler.

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
