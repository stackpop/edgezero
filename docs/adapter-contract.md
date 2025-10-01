# Provider Adapter Contract

This document defines the expectations AnyEdge places on provider adapters. The
current implementations (Fastly Compute@Edge and Cloudflare Workers) satisfy
these rules; new targets should follow the same contract so that the shared
`anyedge-core` services behave consistently.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App`
in `anyedge-core`. They must preserve request semantics, stream responses
without buffering, expose provider context, and offer a proxy bridge so handlers
can forward traffic without knowing which platform they are on.

## Request conversion

Each adapter exposes an `into_core_request` helper that accepts the provider's
request type and returns `anyedge_core::http::Request`. The conversion must:  
- Preserve the HTTP method exactly (`GET`, `POST`, etc.).  
- Parse the full URI (path and query string) into an `http::Uri`. Reject invalid
  URIs with `EdgeError::bad_request`.  
- Copy all headers into the core request. Provider-specific headers may be
  filtered only when they clash with the platform defaults (e.g. Fastly's host
  override).  
- Consume the request body into an `anyedge_core::body::Body`. If the provider offers
  a streaming API, it should be exposed via `Body::Stream`; otherwise a single
  buffered chunk is acceptable.  
- Insert a provider context struct (e.g. `FastlyRequestContext`) into the request
  extensions. The context should expose metadata such as client IP addresses or
  environment handles so handlers can reach platform APIs.

## Response conversion

Adapters also expose `from_core_response` (or equivalent) to transform an
`anyedge_core::http::Response` into the provider response type. Implementations must:  
- Map HTTP status codes verbatim.  
- Copy headers, respecting casing rules enforced by the provider.  
- Preserve streaming bodies. `Body::Stream` should be written chunk-by-chunk to
  the provider output without buffering the entire payload.  
- Handle encoding helpers (`decode_gzip_stream`, `decode_brotli_stream`) where a
  provider requires transparent decompression.

## Dispatch helper

Adapters surface a `dispatch` function that bridges from the provider event loop
into the shared router (`App::router().oneshot(...)`). It should:  
1. Convert the incoming provider request with `into_core_request`.  
2. Await the router future.  
3. Convert the resulting `Response` back into the provider type.  
4. Map any `EdgeError` into the provider's error type so failures surface as
   HTTP 5xx responses instead of panicking.

This helper is what demo entrypoints and adapters call when wiring their
platform-specific main functions.

## Proxy integration

Adapters implement `anyedge_core::proxy::ProxyClient` so handlers can forward outbound
requests. The client must:  
- Accept a `ProxyRequest` created with `ProxyRequest::from_request`.  
- Build and send an outbound provider request, reusing headers and streaming the
  body without buffering.  
- Convert the provider response into a `ProxyResponse`, again preserving
  streaming behaviour and normalising encodings.  
- Attach a diagnostic header (e.g. `x-anyedge-proxy`) identifying which adapter
  forwarded the call.  
- Surface provider errors as `EdgeError::internal` so applications can decide
  how to respond.

## Logging initialisation

Each adapter exports an `init_logger` helper for platform-specific logging
backends (`log_fastly` or `console_log!`). Applications should call it before
building the router. New adapters should provide a comparable helper so apps
consistently opt into logging.

## Contract tests

To keep the contract enforceable, each adapter includes integration tests that
validate request/response conversions and the dispatch helper. Fastly and
Cloudflare now ship `tests/contract.rs` suites that exercise:

- `into_core_request` for method, URI, header, body, and context propagation.  
- `from_core_response` for status propagation and streamed body writes.  
- `dispatch` for routed handlers, body passthrough, and streaming responses.

Because the Fastly SDK links against the Compute@Edge host functions, the
contract tests compile only for `wasm32-wasip1`. Run them with:

```bash
rustup target add wasm32-wasip1 # once per workstation
cargo test -p anyedge-adapter-fastly --features fastly --target wasm32-wasip1 --tests
```

Provide a Wasm runner (Wasmtime or Viceroy) via
`CARGO_TARGET_WASM32_WASIP1_RUNNER` if you want to execute the binaries instead
of running `--no-run`.

Cloudflare's adapter relies on `wasm32-unknown-unknown`. The contract suite lives
in `crates/anyedge-adapter-cloudflare/tests/contract.rs` and uses
`wasm-bindgen-test` to run under the Workers runtime shims. Execute it with:

```bash
rustup target add wasm32-unknown-unknown # once per workstation
cargo test -p anyedge-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown --tests
```

Configure a wasm-bindgen test runner via `wasm-bindgen-test-runner` (Node.js) or
`wasm-pack` depending on your tooling; the suite asserts the same request,
response, and streaming guarantees as the Fastly tests.

## Onboarding new adapters

When bringing up another adapter:

1. Implement request/response conversion functions that follow the rules above.  
2. Provide a context type exposing the adapter's metadata and insert it in
   `into_core_request`.  
3. Implement a `dispatch` wrapper plus logging helper.  
4. Wire up a `ProxyClient` that streams bodies and normalises encodings.  
5. Copy the contract test suite, swapping in the new adapter types. Ensure the
   tests are gated to the target architecture if the adapter SDK does not
   compile for native hosts.  
6. Register the adapter with `anyedge-adapter::register_adapter` (typically in a
   `cli` module using the `ctor` crate) so the CLI can discover it dynamically.

Adapters that fulfil these steps can be dropped into the AnyEdge CLI without
requiring changes to application code.
