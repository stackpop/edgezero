# Migrating to the portable store schema

Stage 2 of the CLI-extensions work rewrites `edgezero.toml`'s
`[stores.*]` sections to a portable, non-adapter-specific shape and
moves all adapter-specific runtime knobs to `EDGEZERO__*` environment
variables. This page is referenced by the loader's hard-error message
when it encounters a pre-rewrite manifest; follow it to bring an old
manifest forward.

## TL;DR

```toml
# Before (any of these is now a hard load error)
[stores.kv]
name = "EDGEZERO_KV"            # ← removed
[stores.kv.adapters.spin]       # ← removed (whole subtable)
name = "EDGEZERO_KV"
[stores.config.defaults]        # ← removed
greeting = "hello"

# After
[stores.kv]
ids     = ["sessions", "cache"]
default = "sessions"            # required when ids.len() > 1
[stores.config]
ids     = ["app_config"]        # default optional with a single id
[stores.secrets]
ids     = ["default"]
```

Platform names, tuning, bind host/port, and logging level are read at
runtime from `EDGEZERO__*` environment variables. An adapter binary
runs with **zero env vars set** — each logical id is used as its own
platform name.

## What changed and why

`edgezero.toml` is now portable: it declares what the app _is_, not
how any particular platform runs it. The old per-adapter store and
runtime tables (`[stores.*.adapters.*]`, `[adapters.<name>.adapter]
host`, etc.) coupled the manifest to a specific deployment shape;
keeping them required the manifest to be recompiled every time you
moved between environments.

The new shape lets one manifest cover dev, staging, and production for
the same workload. Per-environment differences (which Cloudflare KV
namespace ID maps to the `sessions` store, what host axum binds to,
what log level the worker uses) live in the environment, not the file.

## Field-by-field

### `[stores.<kind>]`

| Old                                       | New                                                                                       |
| ----------------------------------------- | ----------------------------------------------------------------------------------------- |
| `name = "EDGEZERO_KV"`                    | `ids = ["edgezero_kv"]` (or whatever logical id your code uses)                           |
| `enabled = true`                          | (gone — the kind is enabled by being declared at all)                                     |
| `[stores.<kind>.adapters.<adapter>] name` | `EDGEZERO__STORES__<KIND>__<ID>__NAME` env var at run time (`<ID>` is the upper-case id)  |
| `[stores.config.defaults]`                | (gone — the local axum config store now reads `.edgezero/local-config-<id>.json` instead) |

The portable manifest accepts only `ids` (non-empty) and `default`
(required when `ids.len() > 1`; with a single id it resolves to that
id automatically). Both are validated at load time.

### Capability matrix

Each (adapter, kind) pair is one of two capabilities (full table in
the spec, §6.6):

| Adapter    | KV               | Config                  | Secrets                 |
| ---------- | ---------------- | ----------------------- | ----------------------- |
| axum       | Multi (local)    | Multi (local files)     | Single (env vars)       |
| cloudflare | Multi (KV ns)    | Multi (KV ns)           | Single (worker secrets) |
| fastly     | Multi (KV store) | Multi (config store)    | Multi (secret store)    |
| spin       | Multi (KV label) | Single (flat variables) | Single (flat variables) |

- **Multi**: each logical id resolves to its own platform store.
- **Single**: every logical id maps to the same flat store; per-id
  `NAME` variables are ignored. Declaring more than one id for a
  `Single` (adapter, kind) pair is caught by `config validate` (§10).

### Runtime environment variables

`__` (double underscore) separates segments. Absent variables fall
back to their listed defaults.

| Variable                                | Role                                                       | Default         |
| --------------------------------------- | ---------------------------------------------------------- | --------------- |
| `EDGEZERO__STORES__<KIND>__<ID>__NAME`  | platform name for logical store `<id>`                     | the logical id  |
| `EDGEZERO__STORES__<KIND>__<ID>__<KEY>` | free-form per-adapter tuning (e.g. spin's `MAX_LIST_KEYS`) | —               |
| `EDGEZERO__ADAPTER__HOST`               | bind host (axum)                                           | `127.0.0.1`     |
| `EDGEZERO__ADAPTER__PORT`               | bind port (axum)                                           | `8787`          |
| `EDGEZERO__LOGGING__LEVEL`              | log level                                                  | adapter default |

`<KIND>` ∈ `KV` / `CONFIG` / `SECRETS`; `<ID>` is the upper-case logical id.

## What this means for handler code

`Hooks::config_store()` is gone; the `app!` macro now bakes the
portable store registry into `Hooks::stores()` for all three kinds.

The `Kv` / `Secrets` / `Config` extractors are id-keyed:

```rust
#[action]
pub async fn handler(kv: Kv, secrets: Secrets) -> Result<Response, EdgeError> {
    let sessions = kv.named("sessions")
        .ok_or_else(|| EdgeError::service_unavailable("no `sessions` kv"))?;
    let default_secrets = secrets.default()
        .ok_or_else(|| EdgeError::service_unavailable("no default secrets"))?;
    // …
}
```

`RequestContext` mirrors the same shape:
`ctx.kv_store(id)` / `ctx.kv_store_default()` (and the same for
`config_store` / `secret_store`). The pre-rewrite no-arg accessors
(`ctx.kv_handle()`, `ctx.config_handle()`, `ctx.secret_handle()`)
are **gone** — Stage 10.1 enforced the spec's "no backward
compatibility with the pre-rewrite runtime store API" promise.
Migrating handler code is mechanical: replace each
`ctx.kv_handle()` with `ctx.kv_store_default()`,
`ctx.config_handle()` with `ctx.config_store_default()`, and
`ctx.secret_handle()` with `ctx.secret_store_default()` (the
last one returns a `BoundSecretStore` whose `get_bytes(key)` is
single-arg — the platform store name is bound by the
dispatcher, not passed at the call site).

Adapter setup code still has `with_*_handle` /
`dispatch_with_*_handle` convenience constructors that take a
single bare handle. Internally each dispatcher synthesises a
one-id `KvRegistry` / `ConfigRegistry` / `SecretRegistry`
under the conventional `"default"` id from that handle before
the request reaches the router — so the registry-aware
accessors and the `Kv` / `Config` / `Secrets` extractors
resolve uniformly regardless of which constructor wired the
store.

## What about local config-store seeding?

The pre-rewrite `[stores.config.defaults]` table seeded the axum
config store from the manifest. That table is gone. The axum config
store now reads `.edgezero/local-config-<id>.json` (one file per
declared config id). Use the `edgezero config push --adapter axum`
command (spec §13, [CLI reference](./cli-reference#edgezero-config-push))
to write that file from your typed `<name>.toml` app-config — or
hand-edit the JSON directly when you just need a quick fixture for
local testing.

## Cloudflare config store: `[vars]` → KV namespace

The Cloudflare config store used to read one `[vars]` string binding
containing a JSON object. It now reads from a **KV namespace** binding
asynchronously. To migrate, replace each `[vars] app_config = '{ … }'`
entry with a KV namespace binding:

```toml
# wrangler.toml — before
[vars]
app_config = '{"greeting":"hello","feature.new_checkout":"false"}'

# wrangler.toml — after
[[kv_namespaces]]
binding = "app_config"
id      = "abc123…"
```

Populate the namespace via `wrangler kv:key put`. The binding name
becomes the platform name resolved by
`EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME` (with the default being
the literal id `app_config`).
