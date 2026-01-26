# Adapter Overview

Adapters bridge provider-specific HTTP primitives into EdgeZero's portable model. This document defines the contract that all adapters must fulfill.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App` in `edgezero-core`. They must:

- Preserve request semantics
- Stream responses without buffering
- Expose provider context
- Offer a proxy bridge so handlers can forward traffic without knowing which platform they are on

## Request Conversion

Each adapter exposes an `into_core_request` helper that accepts the provider's request type and returns `edgezero_core::http::Request`. The conversion must:

- **Preserve the HTTP method** exactly (`GET`, `POST`, etc.)
- **Parse the full URI** (path and query string) into an `http::Uri`. Reject invalid URIs with `EdgeError::bad_request`
- **Copy all headers** into the core request. Provider-specific headers may be filtered only when they clash with platform defaults
- **Consume the request body** into an `edgezero_core::body::Body`. If the provider offers a streaming API, it should be exposed via `Body::Stream`; otherwise a single buffered chunk is acceptable
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
4. Map any `EdgeError` into the provider's error type so failures surface as HTTP 5xx responses instead of panicking

This helper is what demo entrypoints and adapters call when wiring their platform-specific main functions.

## Proxy Integration

Adapters implement `edgezero_core::proxy::ProxyClient` so handlers can forward outbound requests. The client must:

- Accept a `ProxyRequest` created with `ProxyRequest::from_request`
- Build and send an outbound provider request, reusing headers and streaming the body without buffering
- Convert the provider response into a `ProxyResponse`, again preserving streaming behaviour and normalising encodings
- Attach a diagnostic header (e.g., `x-edgezero-proxy`) identifying which adapter forwarded the call
- Surface provider errors as `EdgeError::internal` so applications can decide how to respond

## Logging Initialisation

Each adapter exports an `init_logger` helper for platform-specific logging backends (`log_fastly` or `console_log!`). Applications should call it before building the router. New adapters should provide a comparable helper so apps consistently opt into logging.

## Contract Tests

To keep the contract enforceable, each adapter includes integration tests that validate request/response conversions and the dispatch helper:

- `into_core_request` for method, URI, header, body, and context propagation
- `from_core_response` for status propagation and streamed body writes
- `dispatch` for routed handlers, body passthrough, and streaming responses

### Fastly Tests

Because the Fastly SDK links against the Compute@Edge host functions, the contract tests compile only for `wasm32-wasip1`. Run them with:

```bash
rustup target add wasm32-wasip1
cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --tests
```

Provide a Wasm runner (Wasmtime or Viceroy) via `CARGO_TARGET_WASM32_WASIP1_RUNNER` if you want to execute the binaries instead of running `--no-run`.

### Cloudflare Tests

Cloudflare's adapter relies on `wasm32-unknown-unknown`. The contract suite uses `wasm-bindgen-test` to run under the Workers runtime shims:

```bash
rustup target add wasm32-unknown-unknown
cargo test -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --tests
```

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

| Adapter | Platform | Target | Status |
|---------|----------|--------|--------|
| [Fastly](/guide/adapters/fastly) | Fastly Compute@Edge | `wasm32-wasip1` | Stable |
| [Cloudflare](/guide/adapters/cloudflare) | Cloudflare Workers | `wasm32-unknown-unknown` | Stable |
| [Axum](/guide/adapters/axum) | Native (Tokio) | Host | Stable |
