# Adapter Overview

Adapters bridge provider-specific HTTP primitives into EdgeZero's portable model. This document defines the contract that all adapters must fulfill.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App` in `edgezero-core`. They must:

- Preserve request semantics
- Stream responses without buffering where the provider supports it
- Expose provider context
- Offer a proxy bridge so handlers can forward traffic without knowing which platform they are on

## Request Conversion

Each adapter exposes an `into_core_request` helper that accepts the provider's request type and returns `edgezero_core::http::Request`. The conversion preserves the runtime-visible request subject to the fidelity limits below. It must:

- **Preserve the HTTP method** exactly (`GET`, `POST`, etc.)
- **Parse the full URI** (path and query string) into an `http::Uri`. Reject invalid URIs with `EdgeError::bad_request`
- **Copy all headers** into the core request. Provider-specific headers may be filtered only when they clash with platform defaults
- **Consume the request body** into an `edgezero_core::body::Body`. Adapters may buffer inbound bodies today; streaming input should be preserved where available
- **Insert a provider context struct** (e.g., `FastlyRequestContext`) into the request extensions. The context should expose metadata such as client IP addresses or environment handles so handlers can reach platform APIs

## Response Conversion

Adapters also expose `from_core_response` (or equivalent) to transform an `edgezero_core::http::Response` into the provider response type. Implementations must:

- **Map HTTP status codes** verbatim
- **Copy headers**, respecting casing rules enforced by the provider
- **Preserve streaming bodies** - `Body::Stream` should be written chunk-by-chunk to the provider output without buffering the entire payload. Cloudflare does this through standard dispatch via `Response::from_stream`. Fastly's standard `run_app`/`serve_app` paths drain the stream into a `fastly::Body`; custom [`lifecycle::serve_custom`](/guide/adapters/fastly#custom-dispatch-and-streaming) callbacks can stream progressively. Spin collects into a `Vec<u8>` capped at 16 MiB, and Axum buffers as well
- **Handle encoding helpers** (`decode_gzip_stream`, `decode_brotli_stream`) where a provider requires transparent decompression

## Dispatch Helper

Adapters surface a dispatch entry point, either a free function or a service builder's `dispatch` method, that bridges from the provider event loop into the shared router (`app.router().oneshot(...)`). It should:

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
- Attach a diagnostic header (e.g., `x-edgezero-proxy`) identifying which adapter forwarded the call (Fastly, Cloudflare, and Spin do this today; Axum does not)
- Surface provider errors as `EdgeError::internal` so applications can decide how to respond

## Logging Initialisation

Each adapter exports an `init_logger` helper for platform-specific logging backends. Fastly wires
`log_fastly`, Cloudflare currently no-ops, Spin no-ops because Spin manages its own logging
internally, and Axum uses `simple_logger` in its `run_app` helper.
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

### Spin Tests

Spin's adapter targets `wasm32-wasip2` and its contract suite runs under Wasmtime:

```bash
rustup target add wasm32-wasip2
export CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -W component-model-async=y -S p3=y,http=y"
cargo test -p edgezero-adapter-spin --features spin --target wasm32-wasip2 --test contract
```

The `wasmtime` version CI uses is pinned in `.tool-versions`.

## Onboarding New Adapters

When bringing up another adapter:

1. **Implement request/response conversion functions** that follow the rules above
2. **Provide a context type** exposing the adapter's metadata and insert it in `into_core_request`
3. **Implement a dispatch entry point** (free function or service builder) plus logging helper
4. **Wire up a `ProxyClient`** that streams bodies and normalises encodings
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
actions (`DeployStaged`, `EmitVersion`, `Healthcheck`, `Rollback`); the others return
an unsupported error for those.

## Inbound request fidelity

Adapters insert `edgezero_core::request::RequestIngress` into request extensions.
It records runtime-visible target and trusted origin facts separately from the
primary URI and headers. Missing metadata means unknown capability.

`CapturedTarget::Complete` holds the whole captured string, its acquisition
source and preservation status. Capture has a 16 KiB UTF-8 byte bound; oversize
input becomes `Unavailable(TooLarge)`, without retaining a prefix. Fastly's
current safe API reports `Unavailable(NotExposed)`. Metadata has borrowed
getters and intentionally has no value-bearing `Debug`, `Display` or
serialization implementation. Do not log or serialize captured values.

`InboundOrigin` validates an HTTP(S) scheme and an authority of at most 1 KiB,
including IPv6 and port syntax. Parsing validates syntax; the adapter still
establishes provenance. Runtime URI facts use `RuntimeUri`; Axum's plain HTTP
listener uses `TransportBinding` plus one consistent Host. Origin-form Axum
conversion without that binding has no trusted origin. Ordinary Origin,
Forwarded and Spin special headers do not establish ingress origin.

Fastly records target unavailability before reading its SDK runtime URL for
origin, and requires a client request with one consistent validated Host.
This recovers canonical origin without claiming an original path.

`HeaderFidelity` has separate octet, field-multiplicity, same-name-order and
global-order axes, with per-name overrides. They concern original received
fields, excluding HTTP framing and header-name spelling. Faithful copying from
a runtime HeaderMap does not establish original-wire preservation. Unproved
axes remain `Unknown`; global field order is `Unavailable` on all four adapters.
Cloudflare's common octet and multiplicity guarantees remain `Unknown`: a
runtime capable of replacement/coalescing does not prove a particular field
changed. The observed transformations below remain capability limitations.

Local evidence uses locked SDKs and fixed raw HTTP/1.1 fixtures. These results
describe the tested local runtimes; they do not establish production-edge
behavior.

| Surface                             | Fastly 0.12.1 / Viceroy 0.17.0                                       | worker 0.8.3 / workerd 1.20260415.1                                                       | Axum / Hyper 1.10.1                                                  | Spin SDK 6.0.0 / Spin 4.0.0                                          |
| ----------------------------------- | -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Pre-dispatch hook                   | Executed                                                             | Executed for supported incoming methods                                                   | Executed                                                             | Executed                                                             |
| Runtime extension method conversion | Byte method preserved                                                | Web Request converter preserves tokens; local wire parser returns 501 for unknown methods | Preserved in tested TCP fixture                                      | Preserved in tested HTTP fixture                                     |
| Target metadata                     | Not exposed by safe SDK API                                          | Runtime URL; literal and encoded dot spellings normalize                                  | Runtime URI; tested spelling retained                                | Runtime URI; tested spelling retained                                |
| High-byte Cookie                    | FF retained in tested fixture                                        | Local wire FF becomes U+FFFD before Rust and is copied as runtime UTF-8 EF BF BD          | FF retained in tested TCP fixture                                    | Runtime returns 500 before component for invalid UTF-8               |
| Repeated control fields             | Tested ordinary duplicates retained; identical Content-Length folded | Fetch combines fields; original counts cannot be recovered; common fidelity Unknown       | Tested ordinary duplicates retained; identical Content-Length folded | Tested ordinary duplicates retained; identical Content-Length folded |
| Global wire order                   | Unavailable                                                          | Unavailable                                                                               | Unavailable                                                          | Unavailable                                                          |

Run `./scripts/test-request-fidelity.sh all` for local transport probes. They
assert pinned observations, including unsupported outcomes; a green regression
run does not turn an unsupported capability into a preservation guarantee.
Fastly raw-target support requires a safe borrowed SDK accessor and new ingress
evidence. Consumers that require original dot spellings or duplicate fields
must separately resolve Fetch normalization/coalescing and platform rejection
limits. EdgeZero does not approve a weaker consumer contract.

The [pre-dispatch hook](../routing#intercepting-before-dispatch) runs after
adapter conversion. Fastly, Cloudflare and Spin still buffer bodies; Axum still
buffers JSON and streams other bodies. Existing store/capability bootstrap and
outer finalizers retain their behavior.

## Opt-in application retention

Use retention when measurements show that constructing the app is expensive.
Keep per-request construction when `configure` or `app_state` reads values that
can change and no refresh policy has been defined.

Application ownership and request scheduling are separate. An `App` can be
retained by its caller; each adapter still controls how requests arrive:

| Adapter    | Existing default                         | Explicit retention                                                                                                    |
| ---------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Fastly     | Build for each single-request invocation | `serve_app` with an SDK `Serve`, or [`lifecycle::serve_custom`](/guide/adapters/fastly#custom-dispatch-and-streaming) |
| Cloudflare | Build on each fetch                      | Concrete application-owned cache and `dispatch_app`                                                                   |
| Spin       | Build on each invocation                 | Concrete cache and `dispatch_app` on a compatible host                                                                |
| Axum       | Build once at server startup             | Already retains the router                                                                                            |

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
manifest settings, and generated entry points retain their lifecycle defaults.
The Cloudflare response-header correction applies to both default and retained apps.

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
| Initialization or required bindings fail   | Surface the error and monitor repeated fresh-instance failures. In custom dispatch, convert a recoverable failure into one response only when the application defines that recovery policy. Bound retries, e.g. with Fastly `Serve` limits.        |
| Logging is installed twice                 | Choose one owner. With the Fastly helper, set `Hooks::owns_logging()` when application code installs the logger. First-snapshot logging is intentionally fixed; use custom dispatch for another policy.                                            |
| A stream fails after commitment            | Finish or drop the streaming writer and settle request-owned pending work. Log the partial-response failure; do not attempt a replacement response after sending headers.                                                                          |

Exercise cancellation as well as successful requests. The core regression tests
drop a suspended request, verify its extension resources are released, and serve
a fresh request through the same router. This does not cancel application-spawned
background tasks or prove cleanup after a terminating provider trap.

Roll out retention only after the application's isolation, refresh, and bounded
memory workload checks pass. Provider eviction can force initialization on any
request, so correctness must not depend on reaching the configured reuse limit.
