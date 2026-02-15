# Key-Value Store

EdgeZero provides a unified interface for Key-Value (KV) storage, abstracting differences between Fastly KV Store and Cloudflare Workers KV.

## End-to-End Example

This example implements a simple visit counter. It retrieves the current count, increments it, and returns the new value.

```rust
use edgezero_core::action;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::Kv;
use edgezero_core::http::Response;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct VisitData {
    count: u64,
}

#[action]
async fn visit_counter(Kv(store): Kv) -> Result<Response, EdgeError> {
    // Read-modify-write helper (Note: not atomic!)
    let data = store
        .update("visits", VisitData::default(), |mut d| {
            d.count += 1;
            d
        })
        .await?;

    Ok(Response::ok(format!("Visit #{}", data.count)))
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
- `put<T>(key, value)`: Stores a value.
- `delete(key)`: Removes a value.
- `list_keys(prefix)`: Lists keys starting with a prefix.

It also supports raw bytes via `get_bytes`, `put_bytes`, etc.

## Platform Specifics

### Local Development

- **Axum**: Uses an in-memory `HashMap`. Data is lost on restart.
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

## Next Steps

- Check out the [demo app](https://github.com/stackpop/edgezero/tree/main/examples/app-demo) for a full working example.
