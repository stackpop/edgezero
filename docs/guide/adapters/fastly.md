# Fastly Compute@Edge

Deploy EdgeZero applications to Fastly's Compute@Edge platform using WebAssembly.

## Prerequisites

- [Fastly CLI](https://developer.fastly.com/learning/compute/#install-the-fastly-cli)
- Rust `wasm32-wasip1` target: `rustup target add wasm32-wasip1`
- [Wasmtime](https://wasmtime.dev/) or [Viceroy](https://github.com/fastly/Viceroy) for local testing

## Project Setup

When scaffolding with `edgezero new my-app --adapters fastly`, you get:

```
crates/my-app-adapter-fastly/
├── Cargo.toml
├── fastly.toml
└── src/
    └── main.rs
```

### fastly.toml

The Fastly manifest configures your service:

```toml
manifest_version = 2
name = "my-app"
language = "rust"
authors = ["you@example.com"]

[local_server]
  [local_server.backends]
    [local_server.backends."origin"]
    url = "https://your-origin.example.com"
```

### Entrypoint

The Fastly entrypoint wires the adapter:

```rust
use edgezero_adapter_fastly::{dispatch, init_logger};
use my_app_core::App;

#[fastly::main]
async fn main(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    init_logger();
    let app = App::build();
    dispatch(&app, req).await
}
```

## Building

Build for Fastly's Wasm target:

```bash
# Using the CLI
edgezero build --adapter fastly

# Or directly with cargo
cargo build -p my-app-adapter-fastly --target wasm32-wasip1 --release
```

The compiled Wasm binary is placed in `target/wasm32-wasip1/release/`.

## Local Development

Run locally with Viceroy (Fastly's local simulator):

```bash
# Using the CLI
edgezero serve --adapter fastly

# Or directly
fastly compute serve --skip-build
```

This starts a local server at `http://127.0.0.1:7676`.

## Deployment

Deploy to Fastly Compute@Edge:

```bash
# Using the CLI
edgezero deploy --adapter fastly

# Or directly
fastly compute deploy
```

## Backends

Fastly routes outbound requests through named backends. Configure them in `fastly.toml`:

```toml
[local_server.backends]
  [local_server.backends."api"]
  url = "https://api.example.com"
  
  [local_server.backends."cdn"]
  url = "https://cdn.example.com"
```

Use backends in your proxy code:

```rust
use edgezero_adapter_fastly::FastlyProxyClient;

let client = FastlyProxyClient::new("api");
let response = ProxyService::new(client).forward(request).await?;
```

## Logging

Fastly uses endpoint-based logging. Initialize the logger in your entrypoint:

```rust
use edgezero_adapter_fastly::init_logger;

fn main() {
    init_logger(); // Uses stdout by default
    // or with custom endpoint:
    // init_logger_with_endpoint("my-logging-endpoint");
}
```

Configure logging in `edgezero.toml`:

```toml
[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true
```

## Context Access

Access Fastly-specific APIs via the request context:

```rust
use edgezero_adapter_fastly::FastlyRequestContext;

#[action]
async fn handler(RequestContext(ctx): RequestContext) -> Response<Body> {
    // Access Fastly context from extensions
    if let Some(fastly_ctx) = ctx.extensions().get::<FastlyRequestContext>() {
        let client_ip = fastly_ctx.client_ip();
        let geo = fastly_ctx.geo();
        // ...
    }
    
    // ...
}
```

## Streaming

Fastly supports native streaming via `stream_to_client`:

```rust
#[action]
async fn stream() -> Response<Body> {
    let stream = async_stream::stream! {
        for i in 0..100 {
            yield Ok::<_, std::io::Error>(format!("chunk {}\n", i).into_bytes());
        }
    };
    
    Response::builder()
        .body(Body::stream(stream))
        .unwrap()
}
```

The adapter automatically uses Fastly's streaming APIs for optimal performance.

## Testing

Run contract tests for the Fastly adapter:

```bash
# Set up the Wasm runner
export CARGO_TARGET_WASM32_WASIP1_RUNNER="wasmtime run --dir=."

# Run tests
cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1
```

::: tip Viceroy Issues
If Viceroy reports keychain access errors on macOS, use Wasmtime as the test runner instead.
:::

## Manifest Configuration

Full `edgezero.toml` Fastly configuration:

```toml
[adapters.fastly.adapter]
crate = "crates/my-app-adapter-fastly"
manifest = "crates/my-app-adapter-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"

[adapters.fastly.commands]
build = "cargo build --release --target wasm32-wasip1 -p my-app-adapter-fastly"
serve = "fastly compute serve -C crates/my-app-adapter-fastly"
deploy = "fastly compute deploy -C crates/my-app-adapter-fastly"

[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true
```

## Next Steps

- Learn about [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative deployment target
- Explore [Configuration](/guide/configuration) for manifest details
