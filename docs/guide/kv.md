# Key-Value Store

EdgeZero provides a unified interface for Key-Value (KV) storage, abstracting differences between Fastly KV Store and Cloudflare Workers KV.

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
- `put_with_ttl(key, value, ttl)`: Stores a value that expires after `ttl`.
- `delete(key)`: Removes a value.
- `exists(key)`: Checks if a key is present.
- `list_keys_page(prefix, cursor, limit)`: Lists keys in a bounded page. Pass the returned cursor back unchanged with the same prefix to fetch the next page.
- `read_modify_write(key, default, f)`: Read-modify-write (non-atomic).

It also supports raw bytes via `get_bytes`, `put_bytes`, etc.

Key listing is paginated by design. This avoids buffering an unbounded number of keys in memory and matches the underlying provider APIs.

## Platform Specifics

### Local Development

- **Axum**: Uses a persistent `redb` embedded database stored at `.edgezero/kv.redb`. Data persists across restarts (add `.edgezero/` to your `.gitignore`).
- **Fastly (Viceroy)**: Requires a `[local_server.kv_stores]` entry in `fastly.toml`.

  ```toml
  [[local_server.kv_stores.EDGEZERO_KV]]
  key = "__init__"
  data = ""

  [setup.kv_stores.EDGEZERO_KV]
  description = "Application KV store"
  ```

- **Cloudflare (Workerd)**: Requires a generic binding in `wrangler.toml`.
  - The `binding` name MUST match the store name configured in `edgezero.toml` (default: "EDGEZERO_KV").
  ```toml
  # inside wrangler.toml
  [[kv_namespaces]]
  binding = "EDGEZERO_KV"
  id = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
  preview_id = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"
  ```

### Consistency

Both Fastly and Cloudflare KV stores are **eventually consistent**.

- A value written at one edge location may not be immediately visible at another.
- `update()` is **not atomic**. Concurrent updates to the same key may result in lost writes.
- **TTL**: `put_with_ttl` enforces a minimum of **60 seconds** and a maximum of **1 year** across all adapters.

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
