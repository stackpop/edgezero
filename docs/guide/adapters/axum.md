# Axum (Native)

Run EdgeZero applications natively using Axum and Tokio for local development, testing, and container deployments.

## Overview

The Axum adapter provides:

- **Local development server** - Fast iteration without Wasm compilation
- **Native testing** - Run tests with standard `cargo test`
- **Container deployments** - Deploy to any platform supporting native binaries

## Project Setup

When scaffolding with `edgezero new my-app`, the Axum adapter includes:

```
crates/my-app-adapter-axum/
├── Cargo.toml
├── axum.toml
└── src/
    └── main.rs
```

### Entrypoint

The Axum entrypoint wires the adapter:

```rust
use edgezero_adapter_axum::dev_server::run_app;
use my_app_core::App;

fn main() -> anyhow::Result<()> {
    run_app::<App>()
}
```

`run_app` installs `simple_logger` (unless `owns_logging = true`), builds the app, and reads bind /
store / logging config at runtime from `EDGEZERO__*` environment
variables (see [the migration guide](../manifest-store-migration.md)).
The portable store metadata baked into `App` by the `app!` macro
drives which logical stores are exposed; no `edgezero.toml` needs to
be loaded by the runtime.
If `Hooks::configure` fails, `run_app` returns the source-preserving
`application configuration failed` error before binding the listener.

## Development Server

Run your project locally on the Axum adapter:

```bash
edgezero serve --adapter axum
```

This starts a server at `http://127.0.0.1:8787` with standard logging to stdout.

### Manual Start

Run the Axum entrypoint directly:

```bash
# Using the CLI
edgezero serve --adapter axum

# Or directly with cargo
cargo run -p my-app-adapter-axum
```

## Building

Build a native release binary:

```bash
# Using the CLI
edgezero build --adapter axum

# Or directly
cargo build -p my-app-adapter-axum --release
```

The binary is placed in `target/release/my-app-adapter-axum`.

## Bounded HTTP/1 Ingress

The standard server entrypoints own a bounded pool of HTTP/1 connections. These limits apply
before application admission, including to sockets that never finish a request head. No custom
listener or router is needed. HTTP/2 is not enabled, and protocol upgrades cannot transfer the
socket out of this pool.

`run_app` resolves these runtime variables once, before application configuration or binding:

| Variable (`EDGEZERO__ADAPTER__INGRESS__` prefix) | Default | Supported range |
| ------------------------------------------------ | ------- | --------------- |
| `MAX_CONNECTIONS`                                | 256     | 1–4096          |
| `MAX_RAW_HEAD_BYTES`                             | 65536   | 8192–1048576    |
| `MAX_HEADER_COUNT`                               | 100     | 1–1024          |
| `HEAD_READ_TIMEOUT_MS`                           | 10000   | 1–120000        |

Only missing settings use defaults. Explicit values must be unsigned decimal integers within
the supported ranges; empty, signed, malformed, overflowing, and zero values fail startup.
Diagnostics identify the setting, never its supplied value. The effective validated
`AxumIngressConfig` is logged at `Info`. Applications can resolve the same configuration with
`AxumIngressConfig::from_env(&EnvConfig::from_env())` and inspect its getters. Programmatic
servers install the validated value in `AxumDevServerConfig::ingress`:

```rust
use std::time::Duration;
use edgezero_adapter_axum::{ingress_config::AxumIngressConfig, dev_server::AxumDevServerConfig};

let config = AxumDevServerConfig {
    ingress: AxumIngressConfig::new(64, 16384, 64, Duration::from_secs(5))?,
    ..AxumDevServerConfig::default()
};
```

Manifest defaults can be supplied through adapter-filtered `[[environment.variables]]` entries.
The Axum CLI forwards the selected manifest's defaults to its Cargo child without mutating the
parent environment. An existing parent variable, including an empty one, takes precedence;
duplicate applicable defaults retain declaration order, with the last taking precedence.
Running the binary directly requires setting the runtime variables directly.

### Connection Ownership And Deadlines

Let `C` be `max_connections()`. At most `C` connection futures own parser state, sockets, and
application work, including keep-alive connections. There is no per-connection spawned task or
permit-waiter queue. The accept loop may own **one additional transient accepted socket**; if
capacity is exhausted it closes that socket immediately, without a parser, response, or
background task. Completed connections are reaped before accepting more work. Disconnect,
parser failure, timeout, cancellation, and shutdown drop the single connection owner and release
capacity; unwinding handler failures are isolated to their connection. Shutdown closes the
owned connections rather than waiting indefinitely for a graceful drain.

The first absolute idle-plus-header deadline starts when the accepted socket enters the pool,
before its future is first polled. Each subsequent deadline starts when Hyper first polls the
next request head after the previous exchange permits it. Idle keep-alive time and incomplete-head
time share this budget; a byte arriving does **not** reset it. Pipelined bytes may already be
buffered, so this is not a deadline starting at the network arrival of each pipelined request.
An independent timer wakes without incoming data, and ready I/O and completed heads also check
expiry before admission. Equality is expired. Slow/trickled or idle connections close when the
deadline expires; an HTTP timeout response is not promised.

After a complete head is accepted, the header clock is disarmed for application work. Existing
application body-read and response-write deadlines remain in force. Infrastructure timers use
the native Tokio monotonic clock, independently of an application's injectable clock, so a
frozen application clock cannot disable pre-admission expiry. These timers are cooperatively
scheduled: blocking application code can delay all work on the connection executor.

### Wire Limits And Parser Storage

Let `H = max_raw_head_bytes()` and `N = max_header_count()`. The pinned Hyper 1.12.0 parser
rejects a complete raw head above `H` bytes or above `N` field lines **before** constructing an
admitted request. The byte budget includes the entire request line, field names, colons,
whitespace, line endings, and the final empty line. Duplicate field lines count separately.
Request-line and header bytes share one combined budget, not independent allowances. Exactly
`H` bytes and `N` fields pass the size/count checks; other protocol, type, or application validation
can still reject an in-budget head. Hyper additionally limits request targets to 65534 bytes
(414). Overflow closes the connection and Hyper attempts a bounded 431 response; other malformed
syntax may receive a bounded 400 response. Expiry or disconnect can prevent response delivery.
Application admission and rendering hooks do not run for raw parser rejections.

`parser_buffer_bytes()` is Hyper's logical threshold `H`, **not** a hard allocation ceiling.
The receive scratch audit pins Hyper 1.12.0, bytes 1.12.1, http 1.4.2, and Rust 1.95.0:

- `parser_read_allocation_bytes()` reports the conservative requested-capacity bound `R = 4H`
  for each receive backing allocation. Adaptive reads expose spare capacity and shared-buffer
  splits can cause a larger replacement allocation than the logical threshold.
- `parser_trailer_allocation_bytes()` reports `T = next_power_of_two(H)` for the separate raw
  chunked-trailer buffer. Hyper applies its byte budget separately to trailers, whose completed
  raw field section must be **strictly smaller** than `H`; this is not a combined head-plus-trailer
  allowance. The trailer field count remains at most `N`.
- Conservative simultaneous receive scratch, including an outstanding channel frame, replacement
  allocation, raw trailer growth, and copied field bytes, is `3R + 2T + H`, **plus** parser/header
  metadata, URI/method storage, and fixed connection state. These are requested capacities, not
  allocator-resident bytes or a complete per-request memory certificate.
- `parser_header_map_allocation_bytes()` reports `M`, a conservative per-map requested-capacity
  charge covering both ordinary and adversarial collision-driven growth, duplicate-value storage,
  and capacity retained across clear/reuse. For `P = max(100, N) + 1`, raw index capacity is bounded
  by the largest power of two at most `10P`, bucket capacity by three quarters of that number,
  and duplicate capacity by `next_power_of_two(max(4, P - 1))`. The getter uses public header-type
  sizes plus conservative machine-word padding for private map nodes. Charge up to `1.5M` for
  one map's reallocating peak, or `3.5M` for overlapping request, response/cache, and trailer maps.
  An application-retained request map or admission's temporary header clone is an additional
  owner, not included in those three maps.
- Raw parse workspaces additionally retain the default 100-field inline arrays and, above that,
  bounded heap arrays. On 64-bit native builds a conservative payload charge for these two
  workspaces is `64 * max(100, N)` bytes, plus their fixed container state. Parsed URI/method data
  and field-name/value backing bytes are separate charges bounded by the raw input sizes.

Response header maps become reusable request-parser storage in Hyper. Before handoff, EdgeZero
therefore bounds application response fields to `max_response_header_count() = max(100, N)`
and `max_response_header_bytes() = H`. Field-byte accounting is the sum of name bytes, value
bytes, and four bytes per field (`: ` and CRLF), including duplicates. EdgeZero copies the map
and field data into fresh storage, discarding application-supplied spare capacity and large
shared backing buffers; duplicate values and sensitive flags are preserved. Custom Hyper
reason phrases are removed in favor of canonical status phrases. Hyper's own status line,
framing, connection, and date fields need additional fixed overhead. An oversized response head
closes the connection and reports a transport error to any active egress attempt; it does not
allocate a replacement error response.

The pool bounds retention of receive scratch, parser-generated error responses, and parser
metadata, not arbitrary application allocations. A budget must additionally charge parsed
request/response metadata and copies, application-held body frames, response buffers, handler
state, stores, outbound exchanges, and allocator overhead. Kernel socket buffers, the OS listen
backlog, and resources outside EdgeZero are excluded. The connection bound is not a published
platform memory ceiling or a whole-process RSS guarantee.

This combined HTTP/1 resource policy does not implement the stricter independent raw-target /
field-section accounting or raw framing-rejection policy. Admission still receives
`IngressHeadAccounting::HostManaged` and `IngressFraming::HostManaged`; both raw-ingress
capabilities remain `Unsupported`. See [Capabilities](/guide/capabilities).

## Outbound HTTP

The Axum adapter injects `AxumOutboundClient`, backed by `reqwest`. Application handlers use the
portable client from `RequestContext::http_client()`; direct wiring and tests can construct the
adapter client with `AxumOutboundClient::try_new()`.

Axum provides native total deadlines, completion-order batching, pending-future cancellation,
batch slot isolation, cache bypass, wire-authority override, elastic phase budgeting, header
fidelity, and streamed upload cancellation. Downstream responses run on a connection-local Hyper
HTTP/1 executor, so portable non-`Send` streams remain lazy and are polled under Hyper demand.
The connection supervisor enforces the absolute response-write deadline, but frame acceptance is
not proof of socket or client receipt; response-egress capabilities therefore remain BestEffort.
See [Capabilities](/guide/capabilities).

For streamed responses with `Content-Length`, Axum retains only the final chunk until source EOF
validates, because Hyper stops polling once the declared length is satisfied. A bad tail is not
sent, and a pending tail remains subject to the write deadline. A declared zero-length stream is
validated before handoff. Suppressed HEAD/status bodies are dropped without polling. Clean source
EOF remains `HostHandoff`, not proof of socket flush or client receipt.

## Logging

The Axum adapter's `run_app` helper installs `simple_logger` at the level read from
`EDGEZERO__LOGGING__LEVEL`, falling back to `info` when the variable is unset or
unparseable. It does not read `edgezero.toml`, and `echo_stdout` has no effect on
the runtime. To install a different logger, set `owns_logging = true` on your `app!`
declaration so `run_app` skips its own logger, then install yours in `main` **before** `run_app`.
Logger setup now precedes application configuration, and setup failures are returned rather than
ignored. For embedding startup validation, `run_app_with_preflight::<App, _>(check)` runs `check`
after logging is ready and before configuration or listener binding. The bundled `edgezero demo`
uses this for its capability gate instead of preinstalling a second CLI logger.

Managed boot warnings use `edgezero::boot` at `Warn`, even when runtime logging is `Off`.
Application-owned loggers can reuse the shared resolver and constants:

```rust
use edgezero_core::{BOOT_LOG_LEVEL, BOOT_LOG_TARGET, resolve_logging_level};
use edgezero_core::env_config::EnvConfig;

let env = EnvConfig::from_env();
simple_logger::SimpleLogger::new()
    .with_level(resolve_logging_level(&env))
    .with_module_level(BOOT_LOG_TARGET, BOOT_LOG_LEVEL)
    .init()?;
// Then call run_app::<App>() with owns_logging = true.
```

Wiring
`edgezero_core::app::App::build::<App>(edgezero_adapter_axum::AXUM_PLATFORM)` and `AxumDevServer` by hand remains the fallback if you also need
to control the bind address or store setup.

::: tip Logging status
`run_app` wires logging automatically; custom entrypoints should install a logger explicitly.
:::

## Testing

The Axum adapter enables standard Rust testing:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::App as EdgeZeroApp;
    use edgezero_core::http::{Request, Method};

    #[tokio::test]
    async fn test_handler() {
        let app = EdgeZeroApp::build::<App>(edgezero_adapter_axum::AXUM_PLATFORM)
            .expect("configured app");
        let router = app.router();

        let request = Request::builder()
            .method(Method::GET)
            .uri("/hello")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
    }
}
```

Run tests:

```bash
cargo test -p my-app-core
cargo test -p my-app-adapter-axum
```

## KV Storage

Each declared `[stores.kv]` id resolves to a `redb`-backed store on disk under
`.edgezero/`, so values persist across dev-server restarts. The file name is
derived from the platform store name, which comes from
`EDGEZERO__STORES__KV__<ID>__NAME` or defaults to the logical id:

```
.edgezero/kv-<slug>-<hash>.redb
```

The database file grows over time and does not shrink after deletions. To reclaim
space, delete the file in `.edgezero/`; the data is lost. See [KV Storage](/guide/kv)
for the portable API.

## Secret Store

A declared `[stores.secrets]` id resolves to an `EnvSecretStore`, which looks up each
secret name verbatim in the process environment. Axum lists `secrets` in its
`single_store_kinds`, so only one secrets id may be declared:

```bash
API_KEY=mysecret edgezero serve --adapter axum
```

## Config Store

For local development, each declared `[stores.config]` id resolves to a
local-file config store backed by `.edgezero/local-config-<id>.json`.
The standard runner loads immutable snapshots before binding its listener.
Missing files create empty declared stores. Malformed files retain typed failing
bindings, while allocation-limit violations fail startup. Changes made by `config
push` take effect after restart, not on the next request.
The portable manifest carries no inline defaults — the
pre-rewrite `[stores.config.defaults]` table is gone (see
[the migration guide](../manifest-store-migration.md)).

```toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

```jsonc
// .edgezero/local-config-app_config.json — what `<your-cli> config push` writes.
// The outer object maps the selected config key (defaults to the logical store
// id; override with `--key`, e.g. for a `staging` blob) to ONE BlobEnvelope,
// JSON-encoded as a string (see the blob migration guide for the envelope's
// fields). `data` holds the typed config verbatim; `#[secret]` fields store
// their key NAMES, which the runtime resolves at request time — never the
// secret values.
{
  "app_config": "{\"version\":1,\"generated_at\":\"…\",\"sha256\":\"…\",\"data\":{\"greeting\":\"hello\",\"feature\":{\"new_checkout\":false},\"service\":{\"timeout_ms\":1500},\"api_token\":\"demo_api_token\",\"vault\":\"default\"}}",
}
```

Typed apps read the whole config in one shot with the `AppConfig<C>` extractor,
which parses the envelope and resolves `#[secret]` fields before handing you `cfg`:

```rust
async fn handler(AppConfig(cfg): AppConfig<MyConfig>) -> Result<Response, EdgeError> {
    let greeting = &cfg.greeting;
    // …
}
```

The lower-level `Config` extractor / `ctx.config_store(id)` exposes the raw
key/value store — `store.get("app_config")` returns the envelope string, and a
hand-seeded flat file returns individual values. Do not pass raw user input
straight to `store.get(…)` in production handlers; validate or allowlist keys
first.

Seed the per-id files with `<your-cli> config push --adapter axum` (the typed
flow — the bundled `edgezero config push` errors), which writes the same
`.edgezero/local-config-<id>.json` files the runtime reads — no shell-out, no
server to authenticate against.

### Snapshot Allocation Limits

Set `EDGEZERO__ADAPTER__CONFIG_STORE__<SETTING>` through the normal environment
configuration/startup path. Absent settings use finite defaults; explicit invalid,
zero, overflowing or inconsistent settings fail before listener binding.

| Setting              | Default | Supported Range / Scope                                                    |
| -------------------- | ------- | -------------------------------------------------------------------------- |
| `MAX_FILE_BYTES`     | 16 MiB  | 1 byte to 256 MiB per file, including JSON syntax and escaping             |
| `MAX_ENTRIES`        | 1024    | 1 to 65536 entries per file, including duplicate-key attempts              |
| `MAX_KEY_BYTES`      | 1024    | 1 byte to 64 KiB decoded UTF-8 per key, no larger than the file cap        |
| `MAX_VALUE_BYTES`    | 8 MiB   | 1 byte to 256 MiB decoded UTF-8 per value, no larger than the file cap     |
| `MAX_RESIDENT_BYTES` | 32 MiB  | 1 byte to 256 MiB across every config snapshot in the startup registry     |
| `MAX_STARTUP_BYTES`  | 112 MiB | 1 byte to 1 GiB across resident snapshots plus one file's reserved staging |

These are requested-allocation charges, not an RSS ceiling. Stores load sequentially.
For wire cap `W`, staging reserves `5 * max(W, 32)` bytes: input growth, escaped-string
decoder growth (including old/new allocation overlap), and temporary decoded key/value
strings. A separate 4096-byte read buffer lives on the stack. The startup limit must
cover staging; every index/payload reservation also checks residency plus staging.
This decoder allowance is audited against pinned `serde_json 1.0.150` and Rust 1.95.0;
changing those allocation implementations requires repeating the audit.

The sorted snapshot index reserves `MAX_ENTRIES * size_of::<(String, ConfigValue)>()`
bytes per store. Each entry additionally charges key UTF-8 bytes, value UTF-8 bytes,
two `Arc` reference counts and up to `size_of::<usize>() - 1` alignment bytes. Duplicate
keys are rejected. Keys/values are size-checked before their owned copies; actual reads
detect file growth and reject the first excess byte without appending it. Non-string
values are rejected without constructing a JSON tree. Failed loads drop staging and
partial snapshots. Diagnostics never contain file contents.

`get` and `get_bounded` return `ConfigValue`, an independently owned, shared UTF-8
payload. Clones allocate no payload and never retain the whole file. Bounded reads
check both extraction caps and the injected deadline before returning an `Arc` clone;
there is no request-time file read. Borrow with `as_str()` or `as_ref()`; `to_string()`
is an explicit allocating boundary. Already-owned `from_map` inputs are caller allocations
outside these startup-reader charges.

Budget fixed manifest/registry/handle metadata separately, as well as allocator overhead,
environment-backed secret reads, concurrently parsed config trees, typed outputs and
application retention of returned handles. Canonical hashing streams into SHA-256 rather
than staging a second full JSON string. The extractor drops its raw handle before resolving
secrets. Kernel buffers, filesystem page cache, allocator fragmentation and total RSS are
not guaranteed. Local file reads are synchronous; no finite startup I/O interruption is
claimed. The broad `config-read-allocation-bounds` capability remains `Unsupported`;
this bounded local config-snapshot path does not certify every secret/provider allocation.

## Container Deployment

Build and deploy as a standard container:

```dockerfile
FROM rust:1.95.0 as builder
WORKDIR /app
COPY . .
RUN cargo build -p my-app-adapter-axum --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/my-app-adapter-axum /usr/local/bin/
EXPOSE 8787
CMD ["my-app-adapter-axum"]
```

## Configuration

Configure the Axum adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full
manifest reference.

The `axum.toml` file is used by the Axum CLI helper to locate the crate and carry a
default port. `edgezero serve --adapter axum` resolves the bind address with this
precedence, highest first: the `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`
environment variables, then `[adapters.axum.adapter]` in `edgezero.toml`, then
`axum.toml`, then `127.0.0.1:8787`. The CLI passes the resolved address to the child
process as `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`. Running the binary
directly bypasses that resolution: it reads only those two environment variables and
otherwise falls back to `127.0.0.1:8787`.

## Development Workflow

A typical development workflow:

1. **Run locally**: `edgezero serve --adapter axum`
2. **Make changes** to handlers in `my-app-core`
3. **Test locally** with curl or browser
4. **Run tests**: `cargo test`
5. **Build for edge**: `edgezero build --adapter fastly`
6. **Deploy**: `edgezero deploy --adapter fastly`

## Differences from Edge Adapters

| Aspect      | Axum                            | Fastly/Cloudflare |
| ----------- | ------------------------------- | ----------------- |
| Compilation | Native                          | Wasm              |
| Cold start  | ~0ms                            | ~0ms (Wasm)       |
| Memory      | Operator configured             | Provider quota    |
| Filesystem  | Full access                     | Sandboxed         |
| Network     | Direct                          | Backend/fetch     |
| Concurrency | Bounded, connection-local async | Runtime dependent |

::: tip Development Parity
While Axum provides a convenient development environment, always test on actual edge platforms before deploying. Provider-specific behaviour such as store backends and request context differs on the real targets.
:::

## Application lifetime

`dev_server::run_app` already constructs one app per server startup and serves
clones of its router. No additional reuse option is needed. Retained application
state must support concurrent requests; each request gets its own metadata,
extensions, and body. Restarting the server creates a fresh app. Native Axum
measurements are a useful ownership reference but do not establish WASM runtime
performance or resource lifetimes.

## Next Steps

- Deploy to [Fastly Compute](/guide/adapters/fastly) for production
- Deploy to [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative
- Explore [Configuration](/guide/configuration) for manifest options
