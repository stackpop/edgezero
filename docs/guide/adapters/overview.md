# Adapter Overview

Adapters bridge provider-specific HTTP primitives into EdgeZero's portable model. This document defines the contract that all adapters must fulfill.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App` in `edgezero-core`. They must:

- Preserve request semantics
- Stream responses without buffering where the provider supports it
- Expose provider context
- Inject the portable outbound HTTP client without exposing provider SDK types to handlers

## Request Conversion

Each adapter exposes an `into_core_request` helper that accepts the provider's request type and returns `edgezero_core::http::Request`. The conversion must:

- **Preserve the HTTP method** exactly (`GET`, `POST`, etc.)
- **Parse the full URI** (path and query string) into an `http::Uri`. Reject invalid URIs with `EdgeError::bad_request`
- **Copy all headers** into the core request. Provider-specific headers may be filtered only when they clash with platform defaults
- **Consume the request body** into an `edgezero_core::body::Body`. Adapters may buffer inbound bodies today; streaming input should be preserved where available
- **Insert a provider context struct** (e.g., `FastlyRequestContext`) into the request extensions. The context should expose metadata such as client IP addresses or environment handles so handlers can reach platform APIs

## Response Egress

Adapters consume a `ResponseEgressEnvelope` and own delivery through the strongest platform
transport boundary available. Implementations must:

- **Map HTTP status codes** verbatim
- **Copy headers**, respecting casing rules enforced by the provider
- **Preserve lazy body production** and platform backpressure without whole-response collection
- **Use the application clock and one absolute write deadline** through terminal observation
- **Account accepted payload bytes** at the adapter's documented host boundary
- **Abort or close owned transport resources** and deliver exactly one terminal report
- **Use the bounded precommit fallback** without replacing a response after commit

## Dispatch Helper

Adapters surface a send-owning runner that bridges from the provider event loop into the shared
router. It should:

1. Convert the incoming provider request with `into_core_request`
2. Await the router future
3. Carry the returned egress envelope into the adapter coordinator
4. Own body delivery, deadline/abort handling, and terminal observation
5. Map precommit failures to the bounded fallback and expose only category-safe diagnostics

Generated and demo entrypoints call only the canonical runner for their adapter.

## Store Registry Resolution

All four adapters resolve KV, config, and secret stores from the portable
`Hooks::stores()` metadata baked by the `app!` macro plus `EDGEZERO__*`
environment variables (see [the migration guide](../manifest-store-migration.md)
for the schema change). Each adapter builds a per-request
`StoreRegistry<H>` keyed by logical id; handlers reach a bound store via
the id-keyed `Kv` / `Secrets` / `Config` extractors or the matching
`ctx.kv_store(id)` / `ctx.config_store(id)` / `ctx.secret_store(id)`
accessors. The pre-rewrite `Hooks::config_store()` hook is gone.

## Outbound HTTP Integration

Adapters implement `edgezero_core::outbound::OutboundHttpClient` and inject an `HttpClient` into
each core request. Implementations must:

- Implement `send` and the completion-driven `start_batch_until` driver; ordered collection is
  provided once by core through `send_all_until`
- Apply the shared request validation, hop-by-hop normalization, deadline, and response-body rules
- Enforce independent request, encoded response, decoded response, final buffer, header, Brotli, and chunk-shape controls
- Map typed cache-bypass and validated wire-authority policy without changing URI-owned routing,
  TLS SNI, or certificate identity
- Preserve typed deadline, transport, protocol, codec, and resource failures without message matching
- Attach `x-edgezero-proxy: <adapter>` to completed outbound responses
- Publish an exact static capability level for every outbound capability

Provider limitations are part of the contract rather than hidden implementation details. See
[Capabilities](/guide/capabilities) for the support matrix, timing semantics, and accounting
exclusions.

## Logging Initialisation

Each adapter exports an `init_logger` helper for platform-specific logging backends. Fastly wires
`log_fastly`, Cloudflare currently no-ops, Spin no-ops because Spin manages its own logging
internally, and Axum uses `simple_logger` in its `run_app` helper.
New adapters should provide a comparable helper so apps consistently opt into logging.

## Contract Tests

To keep the contract enforceable, each adapter includes tests that validate request conversion,
response framing, and the owned delivery coordinator:

- `into_core_request` for method, URI, header, body, and context propagation
- response-head commit and lazy streamed body writes
- absolute deadlines, accepted-byte accounting, abort/close, and exactly-once terminal reports
- routing, admission refusal, fallback drain, handler, middleware, and fallback response paths

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

### Spin Tests

Spin's adapter targets `wasm32-wasip2` and its contract suite runs under Wasmtime:

```bash
rustup target add wasm32-wasip2
export CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run"
cargo test -p edgezero-adapter-spin --features spin --target wasm32-wasip2 --test contract
```

The `wasmtime` version CI uses is pinned in `.tool-versions`.

## Onboarding New Adapters

When bringing up another adapter:

1. **Implement request conversion and an owned response coordinator** that follow the rules above
2. **Provide a context type** exposing the adapter's metadata and insert it in `into_core_request`
3. **Implement a send-owning runner** plus logging helper
4. **Wire up an `OutboundHttpClient`** with limits, deadlines, batching, and typed errors
5. **Copy the contract test suite**, swapping in the new adapter types. Ensure the tests are gated to the target architecture if the adapter SDK does not compile for native hosts
6. **Register the adapter** with `edgezero-adapter::register_adapter` (typically in a `cli` module using the `ctor` crate) so the CLI can discover it dynamically. To take part in `provision` and `config push`, override the relevant `Adapter` trait hooks: `single_store_kinds` and `merged_id_kinds` declare the platform's store shape, `validate_adapter_manifest` checks the adapter's own manifest, `provision` creates platform resources, and `push_config_entries` plus `read_config_entry` move config blobs. `single_store_kinds` and `merged_id_kinds` default to empty, `validate_adapter_manifest` defaults to accepting, `provision` defaults to a no-op, and the config push and read hooks default to reporting the operation as unsupported

Adapters that fulfil these steps can be dropped into the EdgeZero CLI without requiring changes to application code.

## Available Adapters

| Adapter                                  | Platform            | Target                   | Status |
| ---------------------------------------- | ------------------- | ------------------------ | ------ |
| [Fastly](/guide/adapters/fastly)         | Fastly Compute@Edge | `wasm32-wasip1`          | Stable |
| [Cloudflare](/guide/adapters/cloudflare) | Cloudflare Workers  | `wasm32-unknown-unknown` | Stable |
| [Spin](/guide/adapters/spin)             | Fermyon Spin        | `wasm32-wasip2`          | Stable |
| [Axum](/guide/adapters/axum)             | Native (Tokio)      | Host                     | Stable |

### Store Capabilities

`single_store_kinds` lists the kinds that allow only one declared id; `merged_id_kinds`
lists the kinds that share one underlying platform resource, so declaring the same
logical id under both is a collision that `config validate` rejects.

| Adapter    | Single-store kinds | Merged kinds   | Config GC | Staging lifecycle |
| ---------- | ------------------ | -------------- | --------- | ----------------- |
| Fastly     | none               | none           | Yes       | Yes               |
| Cloudflare | `secrets`          | `kv`, `config` | No        | No                |
| Spin       | `secrets`          | `kv`, `config` | No        | No                |
| Axum       | `secrets`          | none           | No        | No                |

Fastly is the only adapter implementing `gc_config_entries` and the staging lifecycle
actions (`DeployStaging`, `EmitVersion`, `Healthcheck`, `Rollback`); the others return
an unsupported error for those.
