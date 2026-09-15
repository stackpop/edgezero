# Adapter Overview

Adapters bridge provider-specific HTTP primitives into EdgeZero's portable model. This document defines the contract that all adapters must fulfill.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App` in `edgezero-core`. They must:

- Preserve request semantics
- Stream responses without buffering where the provider supports it
- Expose provider context
- Offer a proxy bridge so handlers can forward traffic without knowing which platform they are on

## Request Conversion

Each adapter exposes an `into_core_request` helper that accepts the provider's request type and returns `edgezero_core::http::Request`. The conversion must:

- **Preserve the HTTP method** exactly (`GET`, `POST`, etc.)
- **Parse the full URI** (path and query string) into an `http::Uri`. Reject invalid URIs with `EdgeError::bad_request`
- **Copy all headers** into the core request. Provider-specific headers may be filtered only when they clash with platform defaults
- **Consume the request body** into an `edgezero_core::body::Body`. Adapters may buffer inbound bodies today; streaming input should be preserved where available
- **Insert a provider context struct** (e.g., `FastlyRequestContext`) into the request extensions. The context should expose metadata such as client IP addresses or environment handles so handlers can reach platform APIs

## Response Conversion

Adapters also expose `from_core_response` (or equivalent) to transform an `edgezero_core::http::Response` into the provider response type. Implementations must:

- **Map HTTP status codes** verbatim
- **Copy headers**, respecting casing rules enforced by the provider
- **Preserve streaming bodies** - `Body::Stream` should be written chunk-by-chunk to the provider output without buffering the entire payload
- **Handle encoding helpers** (`decode_gzip_stream`, `decode_brotli_stream`) where a provider requires transparent decompression

## Dispatch Helper

Adapters surface a `dispatch` function that bridges from the provider event loop into the shared router (`App::router().oneshot(...)`). It should:

1. Convert the incoming provider request with `into_core_request`
2. Await the router future
3. Convert the resulting `Response` back into the provider type
4. Map any `EdgeError` into the provider's error type so failures surface as provider errors (often 5xx) instead of panicking

This helper is what demo entrypoints and adapters call when wiring their platform-specific main functions.

## Store Registry Resolution

All four adapters resolve KV, config, and secret stores from the portable
`Hooks::stores()` metadata baked by the `app!` macro plus `EDGEZERO__*`
environment variables (see [the migration guide](../manifest-store-migration.md)
for the schema change). Each adapter builds a per-request
`StoreRegistry<H>` keyed by logical id; handlers reach a bound store via
the id-keyed `Kv` / `Secrets` / `Config` extractors or the matching
`ctx.kv_store(id)` / `ctx.config_store(id)` / `ctx.secret_store(id)`
accessors. The pre-rewrite `Hooks::config_store()` hook is gone.

## Proxy Integration

Adapters implement `edgezero_core::proxy::ProxyClient` so handlers can forward outbound requests. The client must:

- Accept a `ProxyRequest` created with `ProxyRequest::from_request`
- Build and send an outbound provider request, reusing headers and streaming the body without buffering
- Convert the provider response into a `ProxyResponse`, again preserving streaming behaviour and normalising encodings
- Attach a diagnostic header (e.g., `x-edgezero-proxy`) identifying which adapter forwarded the call (Fastly and Cloudflare do this today)
- Surface provider errors as `EdgeError::internal` so applications can decide how to respond

## Logging Initialisation

Each adapter exports an `init_logger` helper for platform-specific logging backends. Fastly wires
`log_fastly`, Cloudflare currently no-ops, and Axum uses `simple_logger` in its `run_app` helper.
New adapters should provide a comparable helper so apps consistently opt into logging.

## Contract Tests

To keep the contract enforceable, each adapter includes integration tests that validate request/response conversions and the dispatch helper:

- `into_core_request` for method, URI, header, body, and context propagation
- `from_core_response` for status propagation and streamed body writes
- `dispatch` for routed handlers, body passthrough, and streaming responses

### Fastly Tests

Because the Fastly SDK links against the Compute@Edge host functions, the contract tests compile only for `wasm32-wasip1`. Run them with:

```bash
rustup target add wasm32-wasip1
cargo install viceroy --locked
export CARGO_TARGET_WASM32_WASIP1_RUNNER="viceroy run"
cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --test contract
```

Fastly's SDK-linked test binaries need Viceroy for execution; plain Wasmtime
does not provide the required `fastly_*` host imports.

### Cloudflare Tests

Cloudflare's adapter relies on `wasm32-unknown-unknown`. The contract suite uses `wasm-bindgen-test` to run under the Workers runtime shims:

```bash
rustup target add wasm32-unknown-unknown
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --test contract
```

Install a `wasm-bindgen-cli` version that matches the workspace's `wasm-bindgen`
entry in `Cargo.lock` before running the Cloudflare tests.

## Onboarding New Adapters

When bringing up another adapter:

1. **Implement request/response conversion functions** that follow the rules above
2. **Provide a context type** exposing the adapter's metadata and insert it in `into_core_request`
3. **Implement a `dispatch` wrapper** plus logging helper
4. **Wire up a `ProxyClient`** that streams bodies and normalises encodings
5. **Copy the contract test suite**, swapping in the new adapter types. Ensure the tests are gated to the target architecture if the adapter SDK does not compile for native hosts
6. **Register the adapter** with `edgezero-adapter::register_adapter` (typically in a `cli` module using the `ctor` crate) so the CLI can discover it dynamically

Adapters that fulfil these steps can be dropped into the EdgeZero CLI without requiring changes to application code.

## Available Adapters

| Adapter                                  | Platform            | Target                   | Status |
| ---------------------------------------- | ------------------- | ------------------------ | ------ |
| [Fastly](/guide/adapters/fastly)         | Fastly Compute@Edge | `wasm32-wasip1`          | Stable |
| [Cloudflare](/guide/adapters/cloudflare) | Cloudflare Workers  | `wasm32-unknown-unknown` | Stable |
| [Spin](/guide/adapters/spin)             | Fermyon Spin        | `wasm32-wasip2`          | Stable |
| [Axum](/guide/adapters/axum)             | Native (Tokio)      | Host                     | Stable |

## Opt-in application retention

Application ownership and request scheduling are separate. An `App` can be
retained by its caller; each adapter still controls how requests arrive:

| Adapter    | Existing default                         | Explicit retention                                            |
| ---------- | ---------------------------------------- | ------------------------------------------------------------- |
| Fastly     | Build for each single-request invocation | `serve_app` with an SDK `Serve`, or a custom `Serve` callback |
| Cloudflare | Build on each fetch                      | Concrete application-owned cache and `dispatch_app`           |
| Spin       | Build on each invocation                 | Concrete cache and `dispatch_app` on a compatible host        |
| Axum       | Build once at server startup             | Already retains the router                                    |

Retain owned settings, parsed objects, and bounded caches only when their
staleness policy is acceptable. Keep native request handles, bodies, store
registries, pending operations, and response effects request-local. `Send + Sync`
and cloning do not prove host-resource validity. Shared `Arc` interiors remain
shared; the framework does not reset them between requests. Registered app state
continues to overwrite request extensions of the same type.

Applications own refresh, invalidation, key rotation, initialization-failure
policy, and workload benchmarks. Publish complete snapshots; overlapping requests
keep the snapshot they acquired. Never put an unkeyed static in a generic cache
function: that static would be shared between application types. Existing macros,
manifest settings, and generated entry points keep their current behavior.

### Preparing an application for retention

Audit the values captured by handlers, middleware, and registered state before
opting in. The framework creates fresh request resources but cannot inspect or
clear application-owned interiors.

| Risk                                       | Application mitigation                                                                                                                                                                                                                             |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Request data survives into another request | Store identity, authorization results, bodies, and correlation IDs in request-local values or extensions. Use distinct types for request data and registered state; registered state wins on a type collision.                                     |
| Settings or parsed keys become stale       | Keep them request-scoped until a refresh policy is defined. For retained values, build and validate a complete replacement snapshot, then publish it atomically. Decide whether refresh failure keeps the last valid snapshot or rejects requests. |
| Caches grow without bound                  | Limit entries and retained bytes, expire or evict entries, and bound origin diversity. A sandbox request limit cannot bound allocation within one request.                                                                                         |
| Concurrent requests mutate shared state    | Synchronize mutations and acquire an immutable snapshot per request. Release locks before awaiting provider work. Rust's `Send + Sync` bounds do not establish application-level isolation.                                                        |
| Initialization or required bindings fail   | Surface the error and monitor repeated fresh-instance failures. In custom dispatch, convert a recoverable failure into one response only when the application defines that recovery policy. Avoid unlimited retries.                               |
| Logging is installed twice                 | Choose one owner. With the Fastly helper, set `Hooks::owns_logging()` when application code installs the logger. First-snapshot logging is intentionally fixed; use custom dispatch for another policy.                                            |
| A stream fails after commitment            | Finish or drop the streaming writer and settle request-owned pending work. Log the partial-response failure; do not attempt a replacement response after sending headers.                                                                          |

Exercise cancellation as well as successful requests. The core regression tests
drop a suspended request, verify its extension resources are released, and serve
a fresh request through the same router. This does not cancel application-spawned
background tasks or prove cleanup after a terminating provider trap.

Roll out retention only after the application's isolation, refresh, and bounded
memory workload checks pass. Provider eviction can force initialization on any
request, so correctness must not depend on reaching the configured reuse limit.
