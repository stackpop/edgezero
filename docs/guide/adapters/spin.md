# Fermyon Spin

Run EdgeZero applications on [Fermyon Spin](https://spinframework.dev/),
a WebAssembly-first application platform with a `wasm32-wasip2` target and
component-scoped KV / variable stores.

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target (`rustup target add wasm32-wasip2`)
- Spin CLI ([install](https://spinframework.dev/install))

## Project Setup

When scaffolding with `edgezero new my-app`, the Spin adapter includes:

```
crates/my-app-adapter-spin/
├── Cargo.toml
├── spin.toml
└── src/
    └── lib.rs
```

### Entrypoint

The Spin entrypoint wires the adapter via `#[http_service]`:

```rust
use spin_sdk::{http::IntoResponse, http::Request, http_service};
use my_app_core::App;

#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    edgezero_adapter_spin::run_app::<App>(req).await
}
```

`run_app` reads the portable store metadata baked into `App` by the `app!`
macro plus `EDGEZERO__*` environment variables; it does not require an
`edgezero.toml` to be present at runtime.

## Building

Build the Spin component:

```bash
# Using the CLI
edgezero build --adapter spin

# Or directly
cargo build --target wasm32-wasip2 --release -p my-app-adapter-spin
```

## Local Development

```bash
# Using the CLI
edgezero serve --adapter spin

# Or directly
spin up --from crates/my-app-adapter-spin
```

## Deployment

```bash
# Using the CLI
edgezero deploy --adapter spin

# Or directly
spin deploy --from crates/my-app-adapter-spin
```

## KV Storage

Spin KV is **label-backed and multi-store** — each logical id in
`[stores.kv].ids` maps to a Spin store label declared in `spin.toml`.
Override the label per id with `EDGEZERO__STORES__KV__<ID>__NAME`; with the
variable unset the label defaults to the logical id.

```toml
# edgezero.toml
[stores.kv]
ids     = ["sessions", "cache"]
default = "sessions"
```

```toml
# spin.toml
[component.my-app]
key_value_stores = ["sessions", "cache"]
```

Two Spin-specific KV constraints (see §6.7 of the design spec for the
full rationale):

- **TTL is unsupported.** Spin's `key_value::Store::set` accepts no
  expiry. `put_bytes_with_ttl` returns
  `KvError::Unsupported { operation: "put_bytes_with_ttl" }` (mapped to
  HTTP 501); never silently strips the TTL.
- **Listing is capped.** `Store::get_keys()` is unbounded, so the
  adapter materialises the key list, filters by prefix, sorts, and pages
  client-side. A `max_list_keys` cap (default `1000`, override via
  `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS`) guards against runaway
  lists and yields `KvError::LimitExceeded` (HTTP 503) when exceeded.

## Config Store

Spin config is backed by `spin_sdk::variables`, which exposes a **single
flat variable namespace** per component (no notion of multiple named
config stores). `[stores.config].ids` must therefore have exactly one id
for any project targeting Spin — `config validate` catches violations.

Spin variable names must match `^[a-z][a-z0-9_]*$` ([Spin manifest
reference](https://spinframework.dev/manifest-reference)). The adapter
translates the canonical dotted key (`service.timeout_ms`) to a Spin
variable (`service__timeout_ms`) on read.

```toml
# spin.toml
[variables]
greeting = { default = "hello from config store" }

[component.my-app.variables]
greeting = "{{ greeting }}"
```

## Secret Store

Spin secrets share the **same flat variable namespace** as Spin config
(single-store, no named overrides). Secret variables are declared
manually in `spin.toml` with `secret = true`:

```toml
# spin.toml
[variables]
api_token = { required = true, secret = true }

[component.my-app.variables]
api_token = "{{ api_token }}"
```

Because Spin's config and secret namespaces share keys, `config validate`
also runs a collision check: the effective Spin variable name set
({flattened config keys} ∪ {`#[secret]` field values}, each
`.`→`__`-translated) must have no duplicates when `spin` is in the
adapter set.

## Spin component discovery

`provision` and `config push` (Stages 6 and 7) write `[component.<id>.*]`
blocks to `spin.toml`, which requires knowing the component id. Resolution:

- The CLI parses `spin.toml` and enumerates `[component.*]` ids.
- If exactly one component exists, it is used.
- If more than one exists, `[adapters.spin.adapter]` must carry an
  explicit `component = "<id>"` field; otherwise the command errors.

`config validate --strict` performs this resolution as part of its
adapter-set checks when `spin` is in the target list, so the failure
surfaces before `provision` / `config push` run.

## Manifest Configuration

Configure the Spin adapter in `edgezero.toml`. See
[Configuration](/guide/configuration) for the full manifest reference.

## Next Steps

- [Migration guide](/guide/manifest-store-migration) — moving from the
  pre-rewrite store schema
- [Adapters overview](/guide/adapters/overview) — cross-adapter contracts
- [Configuration](/guide/configuration) — full manifest reference
