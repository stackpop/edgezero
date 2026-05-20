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
async fn visit_counter(Kv(store): Kv) -> Result<String, EdgeError> {
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

### 1. Configure the Store Name

In your `edgezero.toml`:

```toml
[stores.kv]
name = "EDGEZERO_KV" # Default name for all adapters
```

### 2. Access the Store

You can access the store using the `Kv` extractor (recommended) or via `RequestContext`.

**Using Extractor:**

```rust
async fn handler(Kv(store): Kv) { ... }
```

**Using Context:**

```rust
async fn handler(ctx: RequestContext) {
    let store = ctx.kv_handle().expect("kv configured");
    ...
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

Key listing is paginated by design. This avoids buffering an unbounded number of keys in memory and matches the underlying provider APIs. The Spin adapter returns `KvError::Validation` for key listing because Spin's current `Store::get_keys()` API is unbounded.

## Platform Specifics

### Local Development

- **Axum**: Uses a persistent `redb` embedded database stored under `.edgezero/`. The default store name uses `.edgezero/kv.redb`; custom store names get their own derived file. Data persists across restarts (add `.edgezero/` to your `.gitignore`).
- **Fastly (Viceroy)**: Requires a `[local_server.kv_stores]` entry in `fastly.toml`.

  ```toml
  [[local_server.kv_stores.EDGEZERO_KV]]
  key = "__init__"
  data = ""

  [setup.kv_stores.EDGEZERO_KV]
  description = "Application KV store"
  ```

- **Cloudflare (Workerd)**: Requires a KV namespace and a binding in `wrangler.toml`.
  1. Create the namespace (run once per environment):

     ```sh
     wrangler kv namespace create EDGEZERO_KV
     wrangler kv namespace create EDGEZERO_KV --preview
     ```

     Each command prints an `id` — copy them into `wrangler.toml`:

  2. Add the binding to `wrangler.toml`:
     ```toml
     [[kv_namespaces]]
     binding = "EDGEZERO_KV"
     id = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"       # from step 1
     preview_id = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy" # from step 1 --preview
     ```

  The `binding` name MUST match the store name configured in `edgezero.toml` (default: `"EDGEZERO_KV"`).

- **Spin**: Requires a `key_value_stores` label in `spin.toml`.

  ```toml
  [component.my-app]
  key_value_stores = ["default"]
  ```

  The label MUST match the store name configured in `edgezero.toml`, or the Spin-specific override. Spin's local runtime auto-provisions the `"default"` label; custom labels require a Spin runtime config or cloud link.

  ```toml
  [stores.kv]
  name = "EDGEZERO_KV"

  [stores.kv.adapters.spin]
  name = "default"
  ```

  `edgezero_adapter_spin::run_app` reads `edgezero.toml` and opens the resolved Spin label. Low-level manual dispatch helpers do not read the manifest.

### Consistency

Both Fastly and Cloudflare KV stores are **eventually consistent**.

- A value written at one edge location may not be immediately visible at another.
- `read_modify_write()` is **not atomic**. Concurrent updates to the same key may result in lost writes.
- **TTL**: `put_with_ttl` enforces a minimum of **60 seconds** and a maximum of **1 year** before delegating to an adapter. Spin KV does not support TTL, so the Spin adapter returns `KvError::Validation` without writing the value.

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
