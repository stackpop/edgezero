# Adapter Overview

Adapters bridge provider-specific HTTP primitives into EdgeZero's portable model. This document defines the contract that all adapters must fulfill.

## Goals

Adapters translate provider-specific HTTP primitives into the portable `App` in `edgezero-core`. They must:

- Preserve request semantics
- Stream responses without buffering where the provider supports it
- Expose provider context
- Inject the portable outbound HTTP client without exposing provider SDK types to handlers

## Request Conversion

Canonical adapter dispatch normalizes the provider request head, performs admission,
and only then consumes the body under the admitted limits and deadline. Public raw
conversion helpers are not a common adapter contract or a substitute for this lifecycle.
The admitted conversion must:

- **Preserve the HTTP method** exactly (`GET`, `POST`, etc.)
- **Parse the full URI** (path and query string) into an `http::Uri`. Reject invalid URIs with `EdgeError::bad_request`
- **Copy all headers** into the core request. Provider-specific headers may be filtered only when they clash with platform defaults
- **Consume the request body** into an `edgezero_core::body::Body` only after admission, enforcing the selected size limit and absolute read deadline
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

1. Normalize the incoming provider request head and run the app's admission policy
2. Consume the admitted body and await canonical app dispatch, or deliver the detached admission response
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
- Construct every `OutboundResponse` with the same `MonotonicClock` retained by the outbound
  client; the constructor has no process-default-clock fallback
- Attach `x-edgezero-proxy: <adapter>` to completed outbound responses
- Publish an exact static capability level for every outbound capability

Provider limitations are part of the contract rather than hidden implementation details. See
[Capabilities](/guide/capabilities) for the support matrix, timing semantics, and accounting
exclusions. Consumers upgrading platform resource or response construction APIs should use the
[Unreleased migration table](https://github.com/stackpop/edgezero/blob/main/CHANGELOG.md#platform-resource-metadata).

## Logging Initialisation

Fastly wires `log_fastly`, and Axum uses `simple_logger`. Their runners initialize logging
before application configuration. Cloudflare and Spin `init_logger` helpers are no-ops: host
console or stdout/stderr capture does not install a Rust `log` backend.

`owns_logging = true` skips adapter logger setup completely. Install your backend **before**
entering the adapter runner, apply the configured runtime filter to that backend, and set the
facade maximum high enough for all enabled targets. `edgezero_core::resolve_logging_level(&env)`
resolves the supplied platform environment's runtime level (invalid or absent means `Info`);
it neither installs a logger nor reads manifest defaults implicitly.

Managed Fastly and Axum backends filter `edgezero_core::BOOT_LOG_TARGET` (`edgezero::boot`) at
`BOOT_LOG_LEVEL` (`Warn`) independently of ordinary records. Consequently boot warnings and
errors still appear with ordinary logging set to `Error` or `Off`. Boot `Info`/`Debug` messages
do not appear. The target is not a physical Fastly endpoint. Application-owned loggers choose
their own boot policy; raising `log::set_max_level` alone cannot restore backend-filtered records.

## Contract Tests

To keep the contract enforceable, each adapter includes tests that validate request conversion,
response framing, and the owned delivery coordinator:

- canonical admission and request conversion for method, URI, header, body, and context propagation
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
2. **Provide a context type** exposing the adapter's metadata and insert it during admitted request conversion
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

## Opt-in application retention

Use retention when measurements show that constructing the app is expensive.
Keep per-request construction when `configure` or `app_state` reads values that
can change and no refresh policy has been defined.

Application ownership and request scheduling are separate. An `App` can be
retained by its caller; each adapter still controls how requests arrive:

| Adapter    | Existing default                         | Explicit retention                                                                         |
| ---------- | ---------------------------------------- | ------------------------------------------------------------------------------------------ |
| Fastly     | Build for each single-request invocation | `serve_app` / `serve_app_with_hooks` with SDK `Serve`, backed by `lifecycle::Sandbox<App>` |
| Cloudflare | Build on each fetch                      | Concrete application-owned cache and `dispatch_app`                                        |
| Spin       | Build on each invocation                 | Concrete cache and `dispatch_app` on a compatible host                                     |
| Axum       | Build once at server startup             | Already retains the app and router                                                         |

The canonical assembly path is `App::build::<A>(PLATFORM) -> Result<App, EdgeError>`.
It installs platform metadata before calling fallible `Hooks::configure` and rejects
successful configuration that changes that metadata. Retain only a successfully
configured app; never cache an initialization error or publish a partial app.
Cloudflare uses `dispatch_app::<A>(&app, req, env, ctx)`; Spin uses
`dispatch_app::<A>(&app, req)` and returns a raw `SpinResponse`. Neither builds an
app nor initializes logging. The caller must pair the app with its construction
`Hooks` type and selected platform.

Retain owned settings, parsed objects, and bounded caches only when their
staleness policy is acceptable. Keep native request handles, bodies, store
registries, pending operations, and response effects request-local. `Send + Sync`
and cloning do not prove host-resource validity. Shared `Arc` interiors remain
shared; the framework does not reset them between requests. Registered app state
continues to overwrite request extensions of the same type.

Every dispatch creates fresh admission state and response-egress ownership in
addition to fresh registries. Fastly's preparation/finalization hooks borrow
request-local values; the adapter owns sending and lazy streaming. Retention does
not expose a response-returning Fastly dispatch path or add response buffering.

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
