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

Spin config is **KV-backed and multi-store** — each logical id in
`[stores.config].ids` opens a separate `spin_sdk::key_value::Store` at
runtime. The store accepts arbitrary UTF-8 keys, so the canonical dotted
key (`service.timeout_ms`) is read back verbatim — no key translation.
Override the label per id with `EDGEZERO__STORES__CONFIG__<ID>__NAME`;
with the variable unset the label defaults to the logical id.

```toml
# edgezero.toml
[stores.config]
ids     = ["app_config", "feature_flags"]
default = "app_config"
```

```toml
# spin.toml — declare every label in the component's `key_value_stores`
[component.my-app]
key_value_stores = ["app_config", "feature_flags"]
```

```toml
# runtime-config.toml — register each custom label with a backend
# (the default `default` label is auto-provided by Spin; everything
# else needs an entry here, or `spin up` errors with
# "unknown key_value_stores label <name>").
[key_value_store.app_config]
type = "spin"

[key_value_store.feature_flags]
type = "spin"
```

`edgezero new --adapter spin` scaffolds both files; `edgezero serve
--adapter spin` runs `spin up --runtime-config-file runtime-config.toml`
so locally-declared labels resolve to the SQLite-backed Spin KV
implementation. For production, swap `type = "spin"` for a managed
backend (`type = "azure"`, `type = "redis"`, …) per the
[Spin runtime-config docs](https://spinframework.dev/v3/dynamic-configuration#key-value-store-runtime-configuration).

`provision` writes the `[component.<id>].key_value_stores` array for
you (it does NOT touch `runtime-config.toml` — keep that one
hand-edited). To seed
the store from `edgezero.toml` + your typed app-config:

```bash
# Production target — POST against the deployed app's seed handler.
edgezero config push --adapter spin \
  --seed-url https://my-app.fermyon.app/__edgezero/config/seed \
  --seed-token $EDGEZERO_SEED_TOKEN

# Local development — POST against `spin up`'s seed handler.
edgezero config push --adapter spin --local
```

The seed handler (built into `run_app` at `/__edgezero/config/seed`)
authenticates via the `x-edgezero-seed` header and writes entries
atomically; see [config push](/guide/cli-reference#config-push) for the
full URL / token resolution chain.

## Secret Store

Spin secrets use `spin_sdk::variables`, which exposes a **single flat
variable namespace** per component (no notion of multiple named secret
stores). `[stores.secrets].ids.len() > 1` while targeting Spin is caught
by `config validate --strict`. Secret variables are declared manually in
`spin.toml` with `secret = true`:

```toml
# spin.toml
[variables]
api_token = { required = true, secret = true }

[component.my-app.variables]
api_token = "{{ api_token }}"
```

`config validate` runs a within-secrets canonicalisation check: each
`#[secret]` field value is lowercased to mirror the runtime
`SpinSecretStore::get_bytes` lookup, must be a valid Spin variable name
(`^[a-z][a-z0-9_]*$`), and must not collide with another `#[secret]`
value that lowercases to the same form.

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
