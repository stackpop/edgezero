# Cloudflare Workers

Deploy EdgeZero applications to Cloudflare Workers using WebAssembly.

## Prerequisites

- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- worker-build: `cargo install worker-build`
- Rust `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`

## Project Setup

When scaffolding with `edgezero new my-app`, the Cloudflare adapter includes:

```
crates/my-app-adapter-cloudflare/
├── Cargo.toml
├── wrangler.toml
└── src/
    ├── lib.rs
    └── main.rs
```

### wrangler.toml

The Wrangler manifest configures your Worker:

```toml
name = "my-app"
main = "build/worker/shim.mjs"
compatibility_date = "2023-05-01"

[build]
command = "worker-build --release"
```

### Entrypoint

The Cloudflare Wasm entrypoint in `src/lib.rs` wires the adapter:

```rust
use my_app_core::App;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    edgezero_adapter_cloudflare::run_app::<App>(req, env, ctx).await
}
```

`run_app` reads the portable store metadata baked into `App` by the
`app!` macro and the `EDGEZERO__*` env vars exposed on the worker
`Env` (Workers cannot enumerate `Env`, so the canonical key set is
derived from the baked store ids and queried individually). Per-id
`KV` / `Config` / `Secret` registries are built and injected into
request extensions automatically. No `edgezero.toml` is loaded by
the runtime — see [the migration guide](../manifest-store-migration.md).
If `Hooks::configure` fails, `run_app` logs only the stable EdgeZero error category and returns the
fixed `application configuration failed` worker error before request conversion. Application or
provider diagnostics are not exposed on the wire.

For fully manual wiring, `CloudflareService::new(&app)` builds a dispatcher one
store at a time: `.with_config(binding)` (a KV binding name),
`.with_config_handle(handle)`, `.with_kv(binding)`, `.with_secrets()`, the
matching `.require_kv()` / `.require_secrets()` flags, and finally
`.dispatch(req, env, ctx).await`, which needs the worker `Env` and `Context`
to open bindings:

```rust
use edgezero_adapter_cloudflare::request::CloudflareService;
use edgezero_core::app::Hooks as _;
use my_app_core::App;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let app = App::build_app();
    CloudflareService::new(&app)
        .with_config("app_config")
        .with_kv("sessions")
        .dispatch(req, env, ctx)
        .await
}
```

This path takes bindings verbatim and does not resolve `EDGEZERO__STORES__*`
selectors, so prefer `run_app` unless you are mocking a backend.
`run_app` dispatches through an internal registry-based path; unlike Fastly's
`dispatch_with_registries`, it is not part of the Cloudflare adapter's public
API.

## Building

Build for Cloudflare's Wasm target:

```bash
# Using the CLI
edgezero build --adapter cloudflare
```

## Local Development

Run locally with Wrangler:

```bash
# Using the CLI
edgezero serve --adapter cloudflare
```

This starts a local server at `http://127.0.0.1:8787`.

## Deployment

Deploy to Cloudflare Workers:

```bash
# Using the CLI
edgezero deploy --adapter cloudflare

# Or directly
wrangler deploy --cwd crates/my-app-adapter-cloudflare
```

## Fetch API

`CloudflareOutboundClient` uses the Workers global `fetch` API and is injected into core request
extensions. Workers needs no backend registration, but application manifests still declare
outbound hosts so the same portable contract validates on every target.

The adapter requests raw encoded upstream bytes with manual fetch encoding, disables automatic
redirect following, and owns an abort signal for the absolute request deadline. When an encoded
body is passed through to the downstream response, the response converter separately selects
manual response-body encoding so Workers does not encode it again. These are two distinct
controls. The abort path is implemented, but outbound and streamed-upload deadlines remain
BestEffort until a deployed host-observed cancellation fixture proves a finite bound. Cloudflare
drives batch slots in completion order, maps cache bypass to `NoStore`, and applies a validated
wire-authority override. The platform has not yet proved finite abort teardown or that its final
wire request preserves authority independently from the connection target, so cancellation and
authority override remain BestEffort. Cloudflare and Axum have Native lazy streamed response passthrough;
raw header octets and original non-`set-cookie` field boundaries remain unavailable, so header
fidelity is BestEffort. See [Capabilities](/guide/capabilities).

Downstream response delivery uses one JavaScript stream writer owned by a coordinator registered
with `Context::wait_until`. It awaits writer backpressure and races the request abort signal and
absolute write deadline. Writer acceptance and close are host-handoff observations; deployed
disconnect and network-completion timing remain unproved, so response-egress capabilities are
BestEffort.

## Logging

EdgeZero does not install a Cloudflare logger by default. Use your preferred logger (for example
`console_log` or your own `log` implementation), and view output in Wrangler or the Cloudflare
dashboard.

::: tip Logging status
Cloudflare logging is opt-in; install a logger (such as `console_log`) in your entrypoint if you
need structured output.
:::

## Context Access

Access Cloudflare-specific APIs via the request context extensions:

```rust
use edgezero_core::context::RequestContext;
use edgezero_adapter_cloudflare::context::CloudflareRequestContext;

async fn handler(ctx: RequestContext) -> Result<Response, EdgeError> {
    if let Some(cf_ctx) = CloudflareRequestContext::get(ctx.request()) {
        // Access Cloudflare-specific data
        let env = cf_ctx.env();
        let ctx = cf_ctx.ctx();
        // ...
    }

    // ...
}
```

## Environment Variables

Define non-secret variables in `wrangler.toml`; the adapter reads the
`EDGEZERO__*` selectors from the same `[vars]` table:

```toml
[vars]
API_URL = "https://api.example.com"
```

Secrets go through the Worker secret store instead; see
[Secret Store](#secret-store).

## Config Store

Cloudflare does not expose a Fastly-style mutable config-store product, so each
declared `[stores.config]` id maps to a **KV namespace binding**. Reads are
asynchronous (`worker::kv::KvStore::get(key).text().await`).

```toml
# edgezero.toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

```toml
# wrangler.toml
[[kv_namespaces]]
binding = "app_config"
id      = "abc123…"
```

The binding name comes from `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME`
(defaulting to the logical id `app_config` when unset). Populate the
namespace via `wrangler kv key put`. Missing bindings log a one-time
warning and the id is dropped from the registry. See
[the migration guide](../manifest-store-migration.md) if you are coming
from the pre-rewrite `[vars]`-backed JSON-string form.

KV and config share the same `[[kv_namespaces]]` binding space on Cloudflare,
so the same logical id must not appear under both `[stores.kv]` and
`[stores.config]`; both would resolve to a single underlying namespace at
runtime. `edgezero config validate` rejects the collision.

## Secret Store

Worker Secrets is a single flat bag with no namespace concept, so exactly one
`[stores.secrets]` id is permitted; `edgezero config validate --strict` rejects
more than one. Handlers read values through the `Secrets` extractor or
`ctx.secret_store(id)`, and a secret with no matching binding resolves to `None`
rather than erroring.

```toml
# edgezero.toml
[stores.secrets]
ids = ["default"]
```

Populate secrets with the Wrangler CLI; there is no binding flag, since the
secret name is the binding:

```bash
wrangler secret put API_KEY
```

## KV Storage

Each declared `[stores.kv]` id maps to a KV namespace binding, exactly like a
config id:

```toml
# edgezero.toml
[stores.kv]
ids = ["sessions"]
```

```toml
# wrangler.toml
[[kv_namespaces]]
binding = "sessions"
id      = "abc123…"
```

The binding name comes from `EDGEZERO__STORES__KV__SESSIONS__NAME`, defaulting
to the logical id. `edgezero provision --adapter cloudflare` creates the
namespace and appends the binding for you. Handlers reach the store through the
portable `Kv` extractor or `ctx.kv_store(id)`; a hand-picked binding that is not
declared under `[stores.kv]` is never opened. See [KV Storage](/guide/kv) for
the API.

## Durable Objects

For stateful edge computing, configure Durable Objects:

```toml
# wrangler.toml
[durable_objects]
bindings = [
  { name = "COUNTER", class_name = "Counter" }
]
```

## Streaming

Cloudflare Workers support streaming via `ReadableStream`. The adapter automatically converts `Body::Stream` to Cloudflare's streaming format.

See the [Streaming guide](/guide/streaming) for examples and patterns.

## Testing

Run contract tests for the Cloudflare adapter:

```bash
WASM_BINDGEN_VERSION=$(
  awk '
    $1 == "name" && $3 == "\"wasm-bindgen\"" { in_pkg=1; next }
    in_pkg && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.lock
)
cargo install wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked --force
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --test contract
```

These tests use `wasm-bindgen-test-runner` and execute the adapter's real
wasm32 request path. The CLI version must exactly match the workspace's
`wasm-bindgen` version from `Cargo.lock`.

## Manifest Configuration

Configure the Cloudflare adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full manifest reference.

## Comparison with Fastly

| Feature           | Cloudflare Workers       | Fastly Compute                      |
| ----------------- | ------------------------ | ----------------------------------- |
| Target            | `wasm32-unknown-unknown` | `wasm32-wasip1`                     |
| Outbound requests | Global `fetch`           | Dynamic backends (derived from URI) |
| Storage           | KV, Durable Objects, R2  | KV Store, Object Store              |
| Logging           | `console.log`            | Log endpoints                       |
| CLI               | Wrangler                 | Fastly CLI                          |

## Next Steps

- Learn about [Fastly Compute](/guide/adapters/fastly) as an alternative
- Explore the [Axum adapter](/guide/adapters/axum) for local development
