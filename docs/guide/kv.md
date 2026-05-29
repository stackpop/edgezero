# Key-Value Store

EdgeZero provides a unified interface for Key-Value (KV) storage, abstracting differences between Axum local storage, Fastly KV Store, Cloudflare Workers KV, and Spin KV.

## End-to-End Example

This example implements a simple visit counter. It retrieves the current count, increments it, and returns the new value.

```rust
use edgezero_core::action;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::Kv;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct VisitData {
    count: u64,
}

#[action]
async fn visit_counter(kv: Kv) -> Result<String, EdgeError> {
    let store = kv
        .default()
        .ok_or_else(|| EdgeError::service_unavailable("no default kv configured"))?;

    // Read-modify-write helper (Note: not atomic!)
    let data = store
        .read_modify_write("visits", VisitData::default(), |mut d| {
            d.count += 1;
            d
        })
        .await?;

    Ok(format!("Visit #{}", data.count))
}
```

## Usage

### 1. Declare logical KV store ids

In your `edgezero.toml` — declare one or more logical ids (the portable
fact "this app uses a KV store called `sessions`"). Platform names are
resolved at runtime from `EDGEZERO__STORES__KV__<ID>__NAME`; with the
variable unset, the platform name defaults to the logical id.

```toml
[stores.kv]
ids     = ["sessions", "cache"]
default = "sessions"            # required when ids.len() > 1
```

For a single-store app the `default` field is optional and resolves to
`ids[0]`. Migrating from the pre-rewrite `name` / `[stores.kv.adapters.*]`
form? See [the migration guide](./manifest-store-migration.md).

### 2. Access the Store

Use the id-keyed `Kv` extractor (recommended) or `RequestContext` accessors.

**Using the extractor — pick a store by id at the call site:**

```rust
async fn handler(kv: Kv) -> Result<Response, EdgeError> {
    let sessions = kv
        .named("sessions")
        .ok_or_else(|| EdgeError::service_unavailable("no `sessions` kv"))?;
    // — or, for the single-store common case —
    let default = kv
        .default()
        .ok_or_else(|| EdgeError::service_unavailable("no default kv"))?;
    // …
}
```

**Using context:**

```rust
async fn handler(ctx: RequestContext) {
    let store = ctx.kv_store("sessions").expect("kv `sessions` configured");
    // or: ctx.kv_store_default()
}
```

### 3. Operations

The `KvHandle` provides typed helpers that automatically serialize/deserialize JSON:

- `get<T>(key)`: Returns `Option<T>`.
- `get_or(key, default)`: Returns the value or a fallback.
- `put<T>(key, value)`: Stores a value.
- `put_with_ttl(key, value, ttl)`: Stores a value that expires after `ttl` on adapters that support TTL.
- `delete(key)`: Removes a value.
- `exists(key)`: Checks if a key is present.
- `list_keys_page(prefix, cursor, limit)`: Lists keys in a bounded page. Pass the returned cursor back unchanged with the same prefix to fetch the next page.
- `read_modify_write(key, default, f)`: Read-modify-write (**not atomic** — see warning below).

It also supports raw bytes via `get_bytes`, `put_bytes`, etc.

::: warning Non-atomic read-modify-write
`read_modify_write` performs a read and a write as **two separate backend calls**.
Concurrent calls on the same key from different requests can interleave, causing
lost writes. For example, two requests that both read `counter = 5` and write `6`
will end with `counter = 6` instead of `7`.

Use it only when approximate values are acceptable (e.g. visit counters, feature flags).
For strict correctness, use a transactional data store.
:::

Key listing is paginated by design. This avoids buffering an unbounded number of keys in memory and matches the underlying provider APIs. The Spin adapter materialises `Store::get_keys()` and pages client-side; a `max_list_keys` cap (configurable via `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS`, default `1000`) guards against runaway lists and yields `KvError::LimitExceeded` when exceeded.

## Platform Specifics

### Local Development

- **Axum**: Uses a persistent `redb` embedded database stored under `.edgezero/`. Each declared KV id gets its own derived file; data persists across restarts (add `.edgezero/` to your `.gitignore`).
- **Fastly (Viceroy)**: Requires a `[local_server.kv_stores]` and `[setup.kv_stores]` entry per declared KV id. `edgezero provision --adapter fastly` writes both blocks for you; the example below assumes a `sessions` id.

  ```toml
  [[local_server.kv_stores.sessions]]
  key = "__init__"
  data = ""

  [setup.kv_stores.sessions]
  description = "Application KV store"
  ```

  Override the platform name per environment via
  `EDGEZERO__STORES__KV__SESSIONS__NAME=<other-name>`; provision honours
  the override when it writes the setup blocks.

- **Cloudflare (Workerd)**: `edgezero provision --adapter cloudflare` creates the namespace and appends the `[[kv_namespaces]]` binding using the env-resolved platform name (`EDGEZERO__STORES__KV__<ID>__NAME` or the logical id by default). The example below shows what provision writes for a `sessions` id with no override:

  ```toml
  [[kv_namespaces]]
  binding = "sessions"
  id = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"       # filled by provision
  ```

  The `binding` name MUST match what the runtime opens — by default the logical id, otherwise the env override.

- **Spin**: Requires a `key_value_stores` label in `spin.toml`.

  ```toml
  [component.my-app]
  key_value_stores = ["default"]
  ```

  The label MUST match what `EDGEZERO__STORES__KV__<ID>__NAME` resolves to (or the logical id when the variable is unset). Spin's local runtime auto-provisions the `"default"` label; custom labels require a Spin runtime config or cloud link. Example:

  ```toml
  [stores.kv]
  ids     = ["sessions"]
  # No platform name in the manifest — set EDGEZERO__STORES__KV__SESSIONS__NAME=default
  # at run time (or leave unset to bind the label "sessions").
  ```

  `edgezero_adapter_spin::run_app` reads baked `[stores.*]` metadata + `EDGEZERO__*` env vars and opens the resolved Spin label per id. Low-level manual dispatch helpers (`dispatch`, `dispatch_with_kv_label`) bypass the env-config path.

### Consistency

Both Fastly and Cloudflare KV stores are **eventually consistent**.

- A value written at one edge location may not be immediately visible at another.
- `read_modify_write()` is **not atomic**. Concurrent updates to the same key may result in lost writes.
- **TTL**: `put_with_ttl` enforces a minimum of **60 seconds** and a maximum of **1 year** before delegating to an adapter. Spin KV does not support TTL, so the Spin adapter returns `KvError::Unsupported { operation: "put_bytes_with_ttl" }` without writing the value.

## Limits & Validation

To ensure portability across all providers, `KvHandle` enforces the
strictest common limits:

| Rule          | Limit                                |
| ------------- | ------------------------------------ |
| Key size      | Max **512 bytes** (Cloudflare limit) |
| Value size    | Max **25 MB**                        |
| TTL minimum   | **60 seconds**                       |
| TTL maximum   | **1 year**                           |
| List page max | **1000 keys**                        |
| Empty keys    | Rejected                             |
| Reserved keys | `.` and `..` rejected                |
| Control chars | Rejected in keys                     |

Violating any of these returns a `KvError::Validation`, which maps to
`400 Bad Request`.

## Next Steps

- Check out the [demo app](https://github.com/stackpop/edgezero/tree/main/examples/app-demo) for a full working example.
