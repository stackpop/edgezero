# Secret Store Abstraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provide a provider-neutral `SecretStore` trait so applications can access sensitive values (API keys, signing keys, tokens) without coupling to platform-specific secret APIs.

**Architecture:** A thin `SecretStore` trait with a single `get_bytes` method is wrapped by a `SecretHandle` that validates inputs and adds `get_str`/`require_*` helpers. Each adapter implements the trait using the platform's native secret mechanism: Fastly `SecretStore`, Cloudflare `Env::secret()`, and Axum `std::env::var`. The handle is stored in request extensions and retrieved via the `Secrets` extractor — identical pattern to the existing `KvHandle`/`Kv` extractor.

**Tech Stack:** Rust 1.91, `async-trait`, `bytes`, `thiserror`, `anyhow`, Fastly 0.11 crate, Cloudflare `worker` 0.7 crate

---

## File Map

### New files
| File | Responsibility |
|------|----------------|
| `crates/edgezero-core/src/secret_store.rs` | `SecretStore` trait, `SecretHandle`, `SecretError`, `NoopSecretStore` (test-utils), `InMemorySecretStore` (test-utils), `secret_store_contract_tests!` macro |
| `crates/edgezero-adapter-fastly/src/secret_store.rs` | `FastlySecretStore` — wraps `fastly::secret_store::SecretStore` |
| `crates/edgezero-adapter-cloudflare/src/secret_store.rs` | `CloudflareSecretStore` — wraps `worker::Env` for on-demand lookup |
| `crates/edgezero-adapter-axum/src/secret_store.rs` | `EnvSecretStore` — reads from `std::env::var` |

### Modified files
| File | Change |
|------|--------|
| `crates/edgezero-core/src/lib.rs` | Add `pub mod secret_store` + re-exports |
| `crates/edgezero-core/src/context.rs` | Add `secret_handle()` method |
| `crates/edgezero-core/src/extractor.rs` | Add `Secrets` extractor |
| `crates/edgezero-core/src/manifest.rs` | Add `ManifestSecretsConfig`, `ManifestSecretsAdapterConfig`, update `ManifestStores`, add `secret_store_name()`, add `DEFAULT_SECRET_STORE_NAME` constant |
| `crates/edgezero-adapter-fastly/src/lib.rs` | Add `pub mod secret_store`, exports, update `run_app` |
| `crates/edgezero-adapter-fastly/src/request.rs` | Add `dispatch_with_secrets`, `dispatch_with_kv_and_secrets` |
| `crates/edgezero-adapter-cloudflare/src/lib.rs` | Add `pub mod secret_store`, exports, update `run_app` |
| `crates/edgezero-adapter-cloudflare/src/request.rs` | Add `dispatch_with_secrets`, `dispatch_with_kv_and_secrets` |
| `crates/edgezero-adapter-axum/src/lib.rs` | Add `pub mod secret_store`, exports |
| `crates/edgezero-adapter-axum/src/service.rs` | Add `secret_handle` field, `with_secret_handle()` method |
| `crates/edgezero-adapter-axum/src/dev_server.rs` | Add `secret_handle_from_manifest()`, wire into `run_app` and `serve_with_listener_and_kv_handle` → renamed `serve_with_listener_and_stores` |
| `crates/edgezero-cli/src/main.rs` | Add secret store binding info logging in `handle_build` |

---

## Design Context

### Relationship to `[[environment.secrets]]`

The manifest already has `[[environment.secrets]]` — **these are completely different concepts**:

| Concept | Section | When it runs | Purpose |
|---------|---------|-------------|---------|
| Build-time secret declaration | `[[environment.secrets]]` | `edgezero build` / `edgezero deploy` | Declares which env vars must be present for CLI commands. Aborts with an error if any are missing. Not related to request handling. |
| Runtime secret store | `[stores.secrets]` | Every HTTP request | Registers a platform-specific vault (Fastly SecretStore, Cloudflare Worker Secrets, env vars) for handlers to read during request processing. |

Both coexist. An app may use `[[environment.secrets]]` to ensure that `API_KEY` is set for the build pipeline, and `[stores.secrets]` so that a handler can call `secrets.require_str("API_KEY").await` at request time.

### Error model

A missing secret (`SecretError::NotFound`) is a **server-side misconfiguration**, not a client error. The `From<SecretError> for EdgeError` impl maps `NotFound` to `EdgeError::internal` (HTTP 500), not `EdgeError::not_found` (404). Client-visible 404s would leak information about server configuration and would be incorrect HTTP semantics.

### Manifest `name` semantics per adapter

The `[stores.secrets] name` field has different meaning per adapter — this asymmetry is intentional:

| Adapter | `name` usage |
|---------|-------------|
| **Fastly** | Name of the `fastly::secret_store::SecretStore` resource declared in `fastly.toml`. Required for `SecretStore::open(name)` to succeed. |
| **Cloudflare** | Informational only. Cloudflare Worker Secrets are individually bound as separate `[vars]` entries in `wrangler.toml`; there is no namespace concept. The `_secrets_required` flag has no runtime effect since `CloudflareSecretStore::from_env(env)` always succeeds. |
| **Axum (dev)** | Informational only. Secrets are read from env vars by name. `EnvSecretStore` is always successfully constructed. |

The `secrets_required` flag is only meaningful for Fastly (where `SecretStore::open()` can fail). On Cloudflare and Axum, constructing the store always succeeds and individual secret misses are surfaced at access time via `SecretError::NotFound`.

---

## Task 1: Core `SecretStore` trait and `SecretHandle`

Closes #60.

**Files:**
- Create: `crates/edgezero-core/src/secret_store.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

- [ ] **Step 1.1: Declare the module in `crates/edgezero-core/src/lib.rs` first**

Add `pub mod secret_store;` after the `key_value_store` line (before adding the implementation file):

```rust
pub mod secret_store;
```

Also add to the `pub use` block:
```rust
pub use secret_store::{SecretError, SecretHandle, SecretStore};
```

This makes the compiler look for `secret_store.rs` — which doesn't exist yet — so the next step's tests will produce the expected error.

- [ ] **Step 1.2: Run to confirm compile error (module file missing)**

```bash
cargo build -p edgezero-core 2>&1 | head -5
```

Expected: `error[E0583]: file not found for module 'secret_store'`

- [ ] **Step 1.3: Write failing tests for `SecretError` and `SecretHandle` validation**

Create `crates/edgezero-core/src/secret_store.rs` containing **only** the test module (no implementation yet):

```rust
// In crates/edgezero-core/src/secret_store.rs

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::executor::block_on;

    // Test-only in-memory store
    use std::collections::HashMap;
    struct SimpleStore(HashMap<String, Bytes>);
    #[async_trait(?Send)]
    impl SecretStore for SimpleStore {
        async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
            Ok(self.0.get(name).cloned())
        }
    }

    fn store_with(entries: &[(&str, &str)]) -> SecretHandle {
        let map: HashMap<String, Bytes> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), Bytes::from(v.to_string())))
            .collect();
        SecretHandle::new(std::sync::Arc::new(SimpleStore(map)))
    }

    #[test]
    fn validate_name_rejects_empty() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.get_bytes("").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn validate_name_rejects_control_chars() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.get_bytes("bad\x00name").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn validate_name_rejects_oversized() {
        block_on(async {
            let h = store_with(&[]);
            let name = "x".repeat(SecretHandle::MAX_NAME_LEN + 1);
            let err = h.get_bytes(&name).await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn get_bytes_returns_none_for_missing() {
        block_on(async {
            let h = store_with(&[]);
            assert_eq!(h.get_bytes("missing").await.unwrap(), None);
        });
    }

    #[test]
    fn get_bytes_returns_value_for_existing() {
        block_on(async {
            let h = store_with(&[("api_key", "secret123")]);
            assert_eq!(
                h.get_bytes("api_key").await.unwrap(),
                Some(Bytes::from("secret123"))
            );
        });
    }

    #[test]
    fn get_str_decodes_utf8() {
        block_on(async {
            let h = store_with(&[("token", "bearer xyz")]);
            assert_eq!(
                h.get_str("token").await.unwrap(),
                Some("bearer xyz".to_string())
            );
        });
    }

    #[test]
    fn require_bytes_fails_for_missing() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.require_bytes("missing").await.unwrap_err();
            assert!(matches!(err, SecretError::NotFound { .. }));
        });
    }

    #[test]
    fn require_str_returns_value() {
        block_on(async {
            let h = store_with(&[("key", "value")]);
            assert_eq!(h.require_str("key").await.unwrap(), "value");
        });
    }
}
```

- [ ] **Step 1.4: Run tests — verify they fail (symbols not defined yet)**

```bash
cargo test -p edgezero-core 2>&1 | head -20
```

Expected: compile error `use of undeclared type 'SecretStore'` or similar — the test body references symbols not yet defined.

- [ ] **Step 1.5: Prepend the full implementation to `secret_store.rs`**

The `#[cfg(test)]` module from Step 1.3 must stay at the bottom of the file — do not delete it. **Insert everything below before the existing `#[cfg(test)] mod tests {` line.** The final file will be: module doc + use declarations + impl code + (unchanged) test module from Step 1.3.

```rust
//! Provider-neutral secret store abstraction.
//!
//! # Architecture
//!
//! ```text
//!  Handler code         SecretHandle (get_str / require_str)
//!      │                       │
//!      └── Secrets extractor ─►│  UTF-8 / bytes layer
//!                              │
//!                     Arc<dyn SecretStore>  (object-safe, Bytes)
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!     EnvSecretStore  FastlySecretStore  CloudflareSecretStore
//! ```
//!
//! Secrets are read-only — this API only retrieves values,
//! it never writes or deletes them. Provisioning secrets is the
//! responsibility of each platform's deployment toolchain.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::EdgeError;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned by secret store operations.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The requested secret was not found.
    #[error("secret not found: {name}")]
    NotFound { name: String },

    /// The secret store backend is temporarily unavailable.
    #[error("secret store unavailable")]
    Unavailable,

    /// A validation error (e.g., invalid secret name).
    #[error("validation error: {0}")]
    Validation(String),

    /// A general internal error.
    #[error("secret store error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<SecretError> for EdgeError {
    fn from(err: SecretError) -> Self {
        match err {
            // NotFound = server misconfiguration, never a client 404.
            // A missing API key means the platform isn't set up correctly,
            // not that the request was invalid.
            SecretError::NotFound { name } => EdgeError::internal(anyhow::anyhow!(
                "required secret '{}' is not configured -- check platform secret store bindings",
                name
            )),
            SecretError::Unavailable => {
                EdgeError::service_unavailable("secret store unavailable")
            }
            // Validation errors are programming errors (bad secret name in code),
            // not client errors.
            SecretError::Validation(e) => {
                EdgeError::internal(anyhow::anyhow!("secret name validation error: {e}"))
            }
            SecretError::Internal(e) => EdgeError::internal(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Object-safe interface for secret store backends.
///
/// All methods take `&self` — backends handle their own access model.
///
/// This trait is always called through [`SecretHandle`], which validates
/// inputs before delegating here. Implementations may therefore assume:
/// - Names are non-empty and within [`SecretHandle::MAX_NAME_LEN`]
/// - Names contain no control characters
#[async_trait(?Send)]
pub trait SecretStore: Send + Sync {
    /// Retrieve a secret as raw bytes. Returns `Ok(None)` if not found.
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError>;
}

// ---------------------------------------------------------------------------
// Test-only no-op store
// ---------------------------------------------------------------------------

/// A no-op [`SecretStore`] for tests that only need a [`SecretHandle`] to exist.
///
/// All reads return `None`.
///
/// Available in `#[cfg(test)]` builds and via the `test-utils` feature:
/// ```toml
/// [dev-dependencies]
/// edgezero-core = { path = "...", features = ["test-utils"] }
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct NoopSecretStore;

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl SecretStore for NoopSecretStore {
    async fn get_bytes(&self, _name: &str) -> Result<Option<Bytes>, SecretError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// In-memory store (test-utils)
// ---------------------------------------------------------------------------

/// An in-memory [`SecretStore`] pre-populated with known secrets.
///
/// Useful for contract tests and unit tests that need deterministic secret values.
///
/// Available in `#[cfg(test)]` builds and via the `test-utils` feature.
#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySecretStore {
    secrets: std::collections::HashMap<String, Bytes>,
}

#[cfg(any(test, feature = "test-utils"))]
impl InMemorySecretStore {
    /// Create a new in-memory store with the given entries.
    ///
    /// # Example
    /// ```rust,ignore
    /// let store = InMemorySecretStore::new([
    ///     ("api_key", Bytes::from("secret123")),
    ///     ("signing_key", Bytes::from("keyvalue")),
    /// ]);
    /// ```
    pub fn new(
        entries: impl IntoIterator<Item = (impl Into<String>, impl Into<Bytes>)>,
    ) -> Self {
        Self {
            secrets: entries
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl SecretStore for InMemorySecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        Ok(self.secrets.get(name).cloned())
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A cloneable, ergonomic handle to a secret store.
///
/// Provides typed helpers (`get_str`, `require_bytes`, `require_str`)
/// while delegating to the object-safe `SecretStore` trait underneath.
///
/// # Example
///
/// ```ignore
/// #[action]
/// async fn handler(Secrets(secrets): Secrets) -> Result<Response, EdgeError> {
///     let api_key: String = secrets.require_str("API_KEY").await?;
///     // use api_key ...
/// }
/// ```
#[derive(Clone)]
pub struct SecretHandle {
    store: Arc<dyn SecretStore>,
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretHandle").finish_non_exhaustive()
    }
}

impl SecretHandle {
    /// Maximum secret name length in bytes.
    pub const MAX_NAME_LEN: usize = 512;

    /// Create a new handle wrapping a secret store implementation.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }

    fn validate_name(name: &str) -> Result<(), SecretError> {
        if name.is_empty() {
            return Err(SecretError::Validation("secret name cannot be empty".to_string()));
        }
        if name.len() > Self::MAX_NAME_LEN {
            return Err(SecretError::Validation(format!(
                "secret name length {} exceeds limit of {} bytes",
                name.len(),
                Self::MAX_NAME_LEN
            )));
        }
        if name.chars().any(|c| c.is_control()) {
            return Err(SecretError::Validation(
                "secret name contains invalid control characters".to_string(),
            ));
        }
        Ok(())
    }

    /// Retrieve a secret as raw bytes. Returns `Ok(None)` if not found.
    pub async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        Self::validate_name(name)?;
        self.store.get_bytes(name).await
    }

    /// Retrieve a secret as a UTF-8 string. Returns `Ok(None)` if not found.
    pub async fn get_str(&self, name: &str) -> Result<Option<String>, SecretError> {
        let bytes = self.get_bytes(name).await?;
        bytes
            .map(|b| {
                String::from_utf8(b.to_vec()).map_err(|e| {
                    SecretError::Internal(anyhow::anyhow!(
                        "secret '{}' is not valid UTF-8: {e}",
                        name
                    ))
                })
            })
            .transpose()
    }

    /// Retrieve a secret as raw bytes. Returns `SecretError::NotFound` if absent.
    pub async fn require_bytes(&self, name: &str) -> Result<Bytes, SecretError> {
        self.get_bytes(name)
            .await?
            .ok_or_else(|| SecretError::NotFound { name: name.to_string() })
    }

    /// Retrieve a secret as a UTF-8 string. Returns `SecretError::NotFound` if absent.
    pub async fn require_str(&self, name: &str) -> Result<String, SecretError> {
        let bytes = self.require_bytes(name).await?;
        String::from_utf8(bytes.to_vec()).map_err(|e| {
            SecretError::Internal(anyhow::anyhow!(
                "secret '{}' is not valid UTF-8: {e}",
                name
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Contract test macro
// ---------------------------------------------------------------------------

/// Generate a suite of contract tests for any [`SecretStore`] implementation.
///
/// The factory expression must produce a store pre-populated with:
/// - `"contract_key"` → `Bytes::from("contract_value")`
/// - `"contract_key_2"` → `Bytes::from("another_value")`
/// - `"missing_key"` must NOT be present.
///
/// # Example
///
/// ```rust,ignore
/// edgezero_core::secret_store_contract_tests!(in_memory_contract, {
///     InMemorySecretStore::new([
///         ("contract_key", Bytes::from("contract_value")),
///         ("contract_key_2", Bytes::from("another_value")),
///     ])
/// });
/// ```
#[macro_export]
macro_rules! secret_store_contract_tests {
    ($mod_name:ident, $factory:expr) => {
        mod $mod_name {
            use super::*;
            use bytes::Bytes;
            use $crate::secret_store::SecretStore;

            fn run<F: std::future::Future>(f: F) -> F::Output {
                futures::executor::block_on(f)
            }

            #[test]
            fn contract_get_existing_returns_bytes() {
                let store = $factory;
                run(async {
                    let result = store.get_bytes("contract_key").await.unwrap();
                    assert_eq!(result, Some(Bytes::from("contract_value")));
                });
            }

            #[test]
            fn contract_get_second_key_returns_bytes() {
                let store = $factory;
                run(async {
                    let result = store.get_bytes("contract_key_2").await.unwrap();
                    assert_eq!(result, Some(Bytes::from("another_value")));
                });
            }

            #[test]
            fn contract_get_missing_returns_none() {
                let store = $factory;
                run(async {
                    let result = store.get_bytes("missing_key").await.unwrap();
                    assert!(result.is_none());
                });
            }
        }
    };
}

// ← Stop here. The #[cfg(test)] mod tests { ... } block from Step 1.3 already
// sits below this point in the file and must be preserved as-is.
```

The `#[cfg(test)]` test module you created in Step 1.3 remains unchanged at the bottom of the file.

- [ ] **Step 1.6: Run tests — verify they pass**

```bash
cargo test -p edgezero-core 2>&1 | tail -20
```

Expected: all tests pass including the new `secret_store::tests::*` tests.

- [ ] **Step 1.7: Run clippy**

```bash
cargo clippy -p edgezero-core --all-features -- -D warnings
```

Expected: no warnings.

- [ ] **Step 1.8: Commit**

```bash
git add crates/edgezero-core/src/secret_store.rs crates/edgezero-core/src/lib.rs
git commit -m "feat(core): add SecretStore trait, SecretHandle, and contract test macro"
```

---

## Task 2: `RequestContext::secret_handle()` + `Secrets` extractor

Closes #64.

**Files:**
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/extractor.rs`

- [ ] **Step 2.1: Write failing test in `context.rs`**

Add to `crates/edgezero-core/src/context.rs` `#[cfg(test)]` block:

```rust
#[test]
fn secret_handle_is_retrieved_when_present() {
    use crate::secret_store::{NoopSecretStore, SecretHandle};
    use std::sync::Arc;

    let mut request = request_builder()
        .method(Method::GET)
        .uri("/secrets")
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(SecretHandle::new(Arc::new(NoopSecretStore)));

    let ctx = RequestContext::new(request, PathParams::default());
    assert!(ctx.secret_handle().is_some());
}

#[test]
fn secret_handle_returns_none_when_absent() {
    let ctx = ctx("/test", Body::empty(), PathParams::default());
    assert!(ctx.secret_handle().is_none());
}
```

- [ ] **Step 2.2: Run tests — verify they fail**

```bash
cargo test -p edgezero-core context::tests 2>&1 | grep -E "FAILED|error"
```

Expected: error `no method named 'secret_handle'`

- [ ] **Step 2.3: Add `secret_handle()` method to `RequestContext`**

In `crates/edgezero-core/src/context.rs`, add import at top:
```rust
use crate::secret_store::SecretHandle;
```

Add method after `kv_handle()`:
```rust
pub fn secret_handle(&self) -> Option<SecretHandle> {
    self.request.extensions().get::<SecretHandle>().cloned()
}
```

- [ ] **Step 2.4: Write failing test for `Secrets` extractor in `extractor.rs`**

Add to `crates/edgezero-core/src/extractor.rs` `#[cfg(test)]` block:

```rust
#[test]
fn secrets_extractor_returns_handle_when_present() {
    use crate::secret_store::{NoopSecretStore, SecretHandle};
    use std::sync::Arc;

    let mut request = request_builder()
        .method(Method::GET)
        .uri("/secrets")
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(SecretHandle::new(Arc::new(NoopSecretStore)));
    let ctx = RequestContext::new(request, PathParams::default());
    let result = block_on(Secrets::from_request(&ctx));
    assert!(result.is_ok());
}

#[test]
fn secrets_extractor_errors_when_absent() {
    let request = request_builder()
        .method(Method::GET)
        .uri("/secrets")
        .body(Body::empty())
        .expect("request");
    let ctx = RequestContext::new(request, PathParams::default());
    let err = block_on(Secrets::from_request(&ctx)).unwrap_err();
    assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
```

- [ ] **Step 2.5: Add `Secrets` extractor to `extractor.rs`**

After the `Kv` extractor block (around line 449), add:

```rust
/// Extracts the [`SecretHandle`] from the request context.
///
/// Returns `EdgeError::Internal` if no secret store was configured for this request.
///
/// # Example
/// ```ignore
/// #[action]
/// pub async fn handler(Secrets(secrets): Secrets) -> Result<Response, EdgeError> {
///     let key = secrets.require_str("API_KEY").await.map_err(EdgeError::from)?;
///     // use key ...
/// }
/// ```
#[derive(Debug)]
pub struct Secrets(pub crate::secret_store::SecretHandle);

#[async_trait(?Send)]
impl FromRequest for Secrets {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.secret_handle().map(Secrets).ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no secret store configured -- check [stores.secrets] in edgezero.toml and platform bindings"
            ))
        })
    }
}

impl std::ops::Deref for Secrets {
    type Target = crate::secret_store::SecretHandle;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Secrets {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Secrets {
    pub fn into_inner(self) -> crate::secret_store::SecretHandle {
        self.0
    }
}
```

- [ ] **Step 2.6: Run tests**

```bash
cargo test -p edgezero-core 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 2.7: Commit**

```bash
git add crates/edgezero-core/src/context.rs crates/edgezero-core/src/extractor.rs
git commit -m "feat(core): add secret_handle() to RequestContext and Secrets extractor"
```

---

## Task 3: Manifest schema for `[stores.secrets]`

Closes #65.

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`

- [ ] **Step 3.1: Write failing tests for manifest parsing**

Add to `manifest.rs` `#[cfg(test)]` block (search for the existing test module):

```rust
#[test]
fn secret_store_name_defaults_to_constant_when_absent() {
    let manifest = ManifestLoader::load_from_str("[app]\nname = \"x\"\n");
    assert_eq!(
        manifest.manifest().secret_store_name("fastly"),
        DEFAULT_SECRET_STORE_NAME
    );
}

#[test]
fn secret_store_name_uses_global_name_when_declared() {
    let manifest = ManifestLoader::load_from_str(
        "[stores.secrets]\nname = \"MY_SECRETS\"\n",
    );
    assert_eq!(manifest.manifest().secret_store_name("fastly"), "MY_SECRETS");
    assert_eq!(
        manifest.manifest().secret_store_name("cloudflare"),
        "MY_SECRETS"
    );
}

#[test]
fn secret_store_name_uses_per_adapter_override() {
    let manifest = ManifestLoader::load_from_str(
        "[stores.secrets]\nname = \"MY_SECRETS\"\n\
         [stores.secrets.adapters.fastly]\nname = \"FASTLY_STORE\"\n",
    );
    assert_eq!(
        manifest.manifest().secret_store_name("fastly"),
        "FASTLY_STORE"
    );
    assert_eq!(
        manifest.manifest().secret_store_name("cloudflare"),
        "MY_SECRETS"
    );
}

#[test]
fn secrets_required_is_false_when_absent() {
    let manifest = ManifestLoader::load_from_str("[app]\nname = \"x\"\n");
    assert!(manifest.manifest().stores.secrets.is_none());
}

#[test]
fn secrets_required_is_true_when_declared() {
    let manifest = ManifestLoader::load_from_str(
        "[stores.secrets]\nname = \"MY_SECRETS\"\n",
    );
    assert!(manifest.manifest().stores.secrets.is_some());
}
```

- [ ] **Step 3.2: Run tests — verify they fail**

```bash
cargo test -p edgezero-core manifest 2>&1 | grep -E "FAILED|error"
```

Expected: errors about `secret_store_name` and `DEFAULT_SECRET_STORE_NAME` not existing.

- [ ] **Step 3.3: Add manifest structs and `secret_store_name()` method**

In `crates/edgezero-core/src/manifest.rs`:

After the `DEFAULT_KV_STORE_NAME` and `default_kv_name` declarations, add:

```rust
/// Default secret store / binding name used when `[stores.secrets]` is omitted.
pub const DEFAULT_SECRET_STORE_NAME: &str = "EDGEZERO_SECRETS";

fn default_secret_name() -> String {
    DEFAULT_SECRET_STORE_NAME.to_string()
}
```

Update `ManifestStores` struct to add the `secrets` field:

```rust
#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestStores {
    /// KV store configuration.
    #[serde(default)]
    #[validate(nested)]
    pub kv: Option<ManifestKvConfig>,

    /// Secret store configuration. When absent, the default
    /// name `EDGEZERO_SECRETS` is used.
    #[serde(default)]
    #[validate(nested)]
    pub secrets: Option<ManifestSecretsConfig>,
}
```

Add after `ManifestKvAdapterConfig`:

```rust
/// Global secret store configuration.
#[derive(Debug, Deserialize, Validate)]
pub struct ManifestSecretsConfig {
    /// Store / binding name (default: `"EDGEZERO_SECRETS"`).
    #[serde(default = "default_secret_name")]
    #[validate(length(min = 1))]
    pub name: String,

    /// Per-adapter name overrides.
    #[serde(default)]
    #[validate(nested)]
    pub adapters: BTreeMap<String, ManifestSecretsAdapterConfig>,
}

/// Per-adapter secret store name override.
#[derive(Debug, Deserialize, Validate)]
pub struct ManifestSecretsAdapterConfig {
    #[validate(length(min = 1))]
    pub name: String,
}
```

Add `secret_store_name()` method to `impl Manifest`, after `kv_store_name()`:

```rust
/// Returns the secret store name for a given adapter.
///
/// Resolution order:
/// 1. Per-adapter override (`[stores.secrets.adapters.<adapter>]`)
/// 2. Global name (`[stores.secrets] name = "..."`)
/// 3. Default: `"EDGEZERO_SECRETS"`
pub fn secret_store_name(&self, adapter: &str) -> &str {
    match &self.stores.secrets {
        Some(secrets) => {
            let adapter_lower = adapter.to_ascii_lowercase();
            if let Some(adapter_cfg) = secrets
                .adapters
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&adapter_lower))
            {
                return &adapter_cfg.1.name;
            }
            &secrets.name
        }
        None => DEFAULT_SECRET_STORE_NAME,
    }
}
```

- [ ] **Step 3.4: Run tests**

```bash
cargo test -p edgezero-core 2>&1 | tail -20
```

Expected: all tests pass including the new manifest tests.

- [ ] **Step 3.5: Commit**

```bash
git add crates/edgezero-core/src/manifest.rs
git commit -m "feat(core): add [stores.secrets] manifest schema and secret_store_name()"
```

---

## Task 4: Fastly secret adapter

Closes #61.

**Files:**
- Create: `crates/edgezero-adapter-fastly/src/secret_store.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`

- [ ] **Step 4.1: Create `FastlySecretStore`**

Create `crates/edgezero-adapter-fastly/src/secret_store.rs`:

```rust
//! Fastly SecretStore adapter.
//!
//! Wraps `fastly::secret_store::SecretStore` to implement
//! `edgezero_core::secret_store::SecretStore`.

#[cfg(feature = "fastly")]
use async_trait::async_trait;
#[cfg(feature = "fastly")]
use bytes::Bytes;
#[cfg(feature = "fastly")]
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store backed by Fastly's SecretStore API.
#[cfg(feature = "fastly")]
pub struct FastlySecretStore {
    store: fastly::secret_store::SecretStore,
}

#[cfg(feature = "fastly")]
impl FastlySecretStore {
    /// Open a Fastly SecretStore by name.
    ///
    /// Returns `SecretError::Internal` if the store does not exist or cannot
    /// be opened. Unlike `KVStore::open`, the Fastly SecretStore API returns
    /// `Result<Self, OpenError>` (not `Result<Option<Self>, _>`), so there
    /// is no `ok_or` unwrap here.
    pub fn open(name: &str) -> Result<Self, SecretError> {
        let store = fastly::secret_store::SecretStore::open(name)
            .map_err(|e| {
                SecretError::Internal(anyhow::anyhow!("failed to open secret store '{}': {e}", name))
            })?;
        Ok(Self { store })
    }
}

#[cfg(feature = "fastly")]
#[async_trait(?Send)]
impl SecretStore for FastlySecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        match self.store.get(name) {
            Some(secret) => Ok(Some(secret.plaintext())),
            None => Ok(None),
        }
    }
}
```

- [ ] **Step 4.2: Add `dispatch_with_secrets` and `dispatch_with_kv_and_secrets` to `request.rs`**

In `crates/edgezero-adapter-fastly/src/request.rs`, add imports:
```rust
use edgezero_core::secret_store::SecretHandle;
```

Add after `dispatch_with_kv`:

```rust
/// Dispatch a Fastly request with a secret store attached.
pub fn dispatch_with_secrets(
    app: &App,
    req: FastlyRequest,
    secret_store_name: &str,
    secrets_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let mut core_request = into_core_request(req).map_err(map_edge_error)?;

    match crate::secret_store::FastlySecretStore::open(secret_store_name) {
        Ok(store) => {
            let handle = SecretHandle::new(std::sync::Arc::new(store));
            core_request.extensions_mut().insert(handle);
        }
        Err(e) => {
            if secrets_required {
                return Err(FastlyError::msg(format!(
                    "secret store '{}' is explicitly configured but could not be opened: {}",
                    secret_store_name, e
                )));
            }
            warn_missing_secret_store_once(secret_store_name, &e);
        }
    }

    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
}

/// Dispatch a Fastly request with both KV and secret stores attached.
pub fn dispatch_with_kv_and_secrets(
    app: &App,
    req: FastlyRequest,
    kv_store_name: &str,
    kv_required: bool,
    secret_store_name: &str,
    secrets_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let mut core_request = into_core_request(req).map_err(map_edge_error)?;

    match FastlyKvStore::open(kv_store_name) {
        Ok(store) => {
            let handle = KvHandle::new(std::sync::Arc::new(store));
            core_request.extensions_mut().insert(handle);
        }
        Err(e) => {
            if kv_required {
                return Err(FastlyError::msg(format!(
                    "KV store '{}' is explicitly configured but could not be opened: {}",
                    kv_store_name, e
                )));
            }
            warn_missing_kv_store_once(kv_store_name, &e);
        }
    }

    match crate::secret_store::FastlySecretStore::open(secret_store_name) {
        Ok(store) => {
            let handle = SecretHandle::new(std::sync::Arc::new(store));
            core_request.extensions_mut().insert(handle);
        }
        Err(e) => {
            if secrets_required {
                return Err(FastlyError::msg(format!(
                    "secret store '{}' is explicitly configured but could not be opened: {}",
                    secret_store_name, e
                )));
            }
            warn_missing_secret_store_once(secret_store_name, &e);
        }
    }

    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
}

fn warn_missing_secret_store_once(name: &str, error: &impl std::fmt::Display) {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(BTreeSet::new()));
    match warned.lock() {
        Ok(mut warned) => {
            if !warned.insert(name.to_string()) {
                return;
            }
            log::warn!("secret store '{}' not available: {}", name, error);
        }
        Err(_) => {
            log::warn!("secret store '{}' not available: {}", name, error);
        }
    }
}
```

- [ ] **Step 4.3: Update `run_app` and `run_app_with_logging` in `lib.rs` to handle secrets**

In `crates/edgezero-adapter-fastly/src/lib.rs`, update `run_app`:

```rust
#[cfg(feature = "fastly")]
pub fn run_app<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: fastly::Request,
) -> Result<fastly::Response, fastly::Error> {
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let manifest = manifest_loader.manifest();
    let logging = manifest.logging_or_default("fastly");
    let kv_name = manifest.kv_store_name("fastly").to_string();
    let kv_required = manifest.stores.kv.is_some();
    let secret_name = manifest.secret_store_name("fastly").to_string();
    let secrets_required = manifest.stores.secrets.is_some();
    run_app_with_logging::<A>(
        logging.into(),
        req,
        &kv_name,
        kv_required,
        &secret_name,
        secrets_required,
    )
}
```

Update `run_app_with_logging` signature and body:

```rust
#[cfg(feature = "fastly")]
pub(crate) fn run_app_with_logging<A: edgezero_core::app::Hooks>(
    logging: FastlyLogging,
    req: fastly::Request,
    kv_store_name: &str,
    kv_required: bool,
    secret_store_name: &str,
    secrets_required: bool,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout).expect("init fastly logger");
    }
    let app = A::build_app();
    dispatch_with_kv_and_secrets(
        &app,
        req,
        kv_store_name,
        kv_required,
        secret_store_name,
        secrets_required,
    )
}
```

Add `pub mod secret_store` and export `dispatch_with_secrets`, `dispatch_with_kv_and_secrets` to the `pub use request::` line.

Update `lib.rs` exports:
```rust
#[cfg(feature = "fastly")]
pub mod secret_store;
// ...
#[cfg(feature = "fastly")]
pub use request::{
    dispatch, dispatch_with_kv, dispatch_with_kv_and_secrets, dispatch_with_secrets,
    into_core_request, DEFAULT_KV_STORE_NAME,
};
```

Also add:
```rust
#[cfg(feature = "fastly")]
pub use secret_store::FastlySecretStore;
```

- [ ] **Step 4.4: Verify existing tests in `lib.rs` still pass**

The only test in `lib.rs` is `fastly_logging_from_manifest_converts_defaults`, which tests `FastlyLogging::from(...)` and does **not** call `run_app_with_logging` at all. No test changes are needed here.

- [ ] **Step 4.5: Build check (WASM — compile only)**

```bash
cargo check -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 2>&1 | tail -10
```

Expected: no errors.

- [ ] **Step 4.6: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/secret_store.rs \
        crates/edgezero-adapter-fastly/src/request.rs \
        crates/edgezero-adapter-fastly/src/lib.rs
git commit -m "feat(fastly): add FastlySecretStore adapter and dispatch_with_secrets"
```

---

## Task 5: Cloudflare secret adapter

Closes #62.

**Files:**
- Create: `crates/edgezero-adapter-cloudflare/src/secret_store.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs`
- Modify: `crates/edgezero-adapter-cloudflare/src/request.rs`

- [ ] **Step 5.1: Create `CloudflareSecretStore`**

Create `crates/edgezero-adapter-cloudflare/src/secret_store.rs`:

```rust
//! Cloudflare Workers secret adapter.
//!
//! Reads secrets from `worker::Env::secret()`. Each call to `get_bytes(name)`
//! invokes `env.secret(name)` to retrieve the value. The `Env` is cloned at
//! dispatch time to outlive `into_core_request`'s ownership of the original.
//!
//! Note: Cloudflare Workers Secrets have no namespace concept — each secret
//! is an individual `[vars]` / Secrets binding in `wrangler.toml`. The
//! `[stores.secrets] name` in `edgezero.toml` is used only for Fastly;
//! Cloudflare accesses all secrets via this adapter regardless of name.

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use async_trait::async_trait;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store backed by Cloudflare Workers `Env`.
///
/// Reads secrets via `env.secret(name)`. Clones the `Env` handle at dispatch
/// time so secrets remain accessible throughout the request lifetime.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub struct CloudflareSecretStore {
    env: worker::Env,
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl CloudflareSecretStore {
    /// Create a secret store from a cloned `Env`.
    pub fn from_env(env: worker::Env) -> Self {
        Self { env }
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SecretStore for CloudflareSecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        match self.env.secret(name) {
            Ok(secret) => {
                let value = secret.to_string();
                Ok(Some(Bytes::from(value.into_bytes())))
            }
            // Workers returns an error when a secret binding is absent
            Err(_) => Ok(None),
        }
    }
}
```

- [ ] **Step 5.2: Add `dispatch_with_secrets` and `dispatch_with_kv_and_secrets` to `request.rs`**

In `crates/edgezero-adapter-cloudflare/src/request.rs`, add import:
```rust
use edgezero_core::secret_store::SecretHandle;
```

Add after `dispatch_with_kv`:

```rust
/// Dispatch a Cloudflare Worker request with a secret store attached.
///
/// Note: `_secrets_required` is intentionally unused. Cloudflare Worker Secrets
/// are individually bound in `wrangler.toml`; there is no namespace to "open"
/// that could fail. The store is always successfully constructed from `Env`.
/// Individual missing secrets surface as `SecretError::NotFound` at access time.
pub async fn dispatch_with_secrets(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    _secrets_required: bool,
) -> Result<CfResponse, WorkerError> {
    // Clone env before consuming it in into_core_request.
    // Env wraps a JsValue reference; cloning increments the JS reference count.
    let secret_store =
        crate::secret_store::CloudflareSecretStore::from_env(env.clone());
    let secret_handle = SecretHandle::new(std::sync::Arc::new(secret_store));

    let mut core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;
    core_request.extensions_mut().insert(secret_handle);

    let svc = app.router().clone();
    let response = svc.oneshot(core_request).await;
    from_core_response(response).map_err(edge_error_to_worker)
}

/// Dispatch a Cloudflare Worker request with both KV and secret stores attached.
pub async fn dispatch_with_kv_and_secrets(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    kv_binding: &str,
    kv_required: bool,
    _secret_binding: &str,      // unused: CF secrets have no namespace concept
    _secrets_required: bool,    // unused: CloudflareSecretStore always constructs OK
) -> Result<CfResponse, WorkerError> {
    // Open KV by borrowing env
    let kv_handle = match crate::key_value_store::CloudflareKvStore::from_env(&env, kv_binding) {
        Ok(store) => Some(KvHandle::new(std::sync::Arc::new(store))),
        Err(e) => {
            if kv_required {
                return Err(WorkerError::RustError(format!(
                    "KV binding '{}' is explicitly configured but could not be opened: {}",
                    kv_binding, e
                )));
            }
            warn_missing_kv_binding_once(kv_binding, &e);
            None
        }
    };

    // Clone env for secrets before consuming it
    let secret_store =
        crate::secret_store::CloudflareSecretStore::from_env(env.clone());
    let secret_handle = SecretHandle::new(std::sync::Arc::new(secret_store));

    let mut core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;

    if let Some(handle) = kv_handle {
        core_request.extensions_mut().insert(handle);
    }
    core_request.extensions_mut().insert(secret_handle);

    let svc = app.router().clone();
    let response = svc.oneshot(core_request).await;
    from_core_response(response).map_err(edge_error_to_worker)
}
```

- [ ] **Step 5.3: Update `run_app` in `lib.rs` to handle secrets**

In `crates/edgezero-adapter-cloudflare/src/lib.rs`, update `run_app`:

```rust
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let manifest = manifest_loader.manifest();
    let kv_binding = manifest.kv_store_name("cloudflare");
    let kv_required = manifest.stores.kv.is_some();
    let secret_binding = manifest.secret_store_name("cloudflare");
    let secrets_required = manifest.stores.secrets.is_some();
    let app = A::build_app();
    dispatch_with_kv_and_secrets(
        &app, req, env, ctx, kv_binding, kv_required, secret_binding, secrets_required,
    )
    .await
}
```

Update `lib.rs` exports to include `secret_store` module and new dispatch functions:

```rust
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod secret_store;

// in pub use request::{ ... } line:
pub use request::{
    dispatch, dispatch_with_kv, dispatch_with_kv_and_secrets, dispatch_with_secrets,
    into_core_request, DEFAULT_KV_BINDING,
};

// add:
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use secret_store::CloudflareSecretStore;
```

- [ ] **Step 5.4: Build check (WASM — compile only)**

```bash
cargo check -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown 2>&1 | tail -10
```

Expected: no errors.

- [ ] **Step 5.5: Commit**

```bash
git add crates/edgezero-adapter-cloudflare/src/secret_store.rs \
        crates/edgezero-adapter-cloudflare/src/request.rs \
        crates/edgezero-adapter-cloudflare/src/lib.rs
git commit -m "feat(cloudflare): add CloudflareSecretStore adapter and dispatch_with_secrets"
```

---

## Task 6: Axum secret adapter + dev server integration

Closes #63.

**Files:**
- Create: `crates/edgezero-adapter-axum/src/secret_store.rs`
- Modify: `crates/edgezero-adapter-axum/src/lib.rs`
- Modify: `crates/edgezero-adapter-axum/src/service.rs`
- Modify: `crates/edgezero-adapter-axum/src/dev_server.rs`

- [ ] **Step 6.1: Write failing tests for `EnvSecretStore`**

Create `crates/edgezero-adapter-axum/src/secret_store.rs` with tests first:

```rust
//! Environment variable secret store for local development.
//!
//! Reads secrets from `std::env::var(name)`. Set secrets as environment
//! variables before starting the dev server:
//!
//! ```bash
//! API_KEY=mysecret cargo edgezero dev
//! ```
//!
//! Or load them from a `.env` file using `dotenvy::dotenv()` in your
//! application entry point before calling `run_app`.

// ... (implementation here, see Step 6.2)

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::executor::block_on;

    #[test]
    fn get_bytes_returns_none_when_var_not_set() {
        // Use a name that's very unlikely to be set in the environment
        let store = EnvSecretStore::new();
        let result = block_on(store.get_bytes("__EDGEZERO_TEST_MISSING_VAR_XYZ__")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_bytes_returns_value_when_var_set() {
        std::env::set_var("__EDGEZERO_TEST_SECRET__", "test_value_123");
        let store = EnvSecretStore::new();
        let result = block_on(store.get_bytes("__EDGEZERO_TEST_SECRET__")).unwrap();
        assert_eq!(result, Some(Bytes::from("test_value_123")));
        std::env::remove_var("__EDGEZERO_TEST_SECRET__");
    }
}
```

- [ ] **Step 6.2: Implement `EnvSecretStore` (insert before the test module)**

```rust
use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store for local development that reads secrets from environment variables.
///
/// When `[stores.secrets]` is declared in `edgezero.toml`, the dev server
/// creates an `EnvSecretStore` that reads secrets from the process environment.
///
/// Populate secrets by setting environment variables before starting the server:
/// ```bash
/// MY_API_KEY=secret cargo edgezero dev
/// ```
pub struct EnvSecretStore;

impl EnvSecretStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnvSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for EnvSecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(os_str)) => {
                Err(SecretError::Internal(anyhow::anyhow!(
                    "secret '{}' contains non-UTF-8 bytes: {:?}",
                    name,
                    os_str
                )))
            }
        }
    }
}
```

- [ ] **Step 6.3: Run tests for `EnvSecretStore`**

```bash
cargo test -p edgezero-adapter-axum secret_store 2>&1 | tail -15
```

Expected: all 2 tests pass.

- [ ] **Step 6.4: Add `with_secret_handle()` to `EdgeZeroAxumService`**

In `crates/edgezero-adapter-axum/src/service.rs`:

Add import:
```rust
use edgezero_core::secret_store::SecretHandle;
```

Add field to `EdgeZeroAxumService`:
```rust
pub struct EdgeZeroAxumService {
    router: RouterService,
    kv_handle: Option<KvHandle>,
    secret_handle: Option<SecretHandle>,  // NEW
}
```

Update `new()`:
```rust
pub fn new(router: RouterService) -> Self {
    Self {
        router,
        kv_handle: None,
        secret_handle: None,
    }
}
```

Add method after `with_kv_handle`:
```rust
/// Attach a shared secret store to this service.
///
/// The handle is cloned into every request's extensions, making
/// the `Secrets` extractor available in handlers.
#[must_use]
pub fn with_secret_handle(mut self, handle: SecretHandle) -> Self {
    self.secret_handle = Some(handle);
    self
}
```

Update `call()` to inject secret handle after kv handle:
```rust
let secret_handle = self.secret_handle.clone();
// ... in the async block, after kv handle injection:
if let Some(handle) = secret_handle {
    core_request.extensions_mut().insert(handle);
}
```

Write a test for `with_secret_handle`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_secret_handle_injects_into_request() {
    use crate::secret_store::EnvSecretStore;
    use edgezero_core::secret_store::SecretHandle;
    use std::sync::Arc;

    std::env::set_var("__EDGEZERO_SERVICE_TEST_SECRET__", "injected_value");

    let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));
    let router = RouterService::builder()
        .get("/check", |ctx: RequestContext| async move {
            let secrets = ctx.secret_handle().expect("secret handle should be present");
            let val = secrets
                .get_str("__EDGEZERO_SERVICE_TEST_SECRET__")
                .await
                .unwrap()
                .unwrap_or_default();
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::from(val))
                .expect("response");
            Ok::<_, EdgeError>(response)
        })
        .build();
    let mut service = EdgeZeroAxumService::new(router).with_secret_handle(handle);

    let request = Request::builder().uri("/check").body(AxumBody::empty()).unwrap();
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"injected_value");

    std::env::remove_var("__EDGEZERO_SERVICE_TEST_SECRET__");
}
```

- [ ] **Step 6.5: Wire `EnvSecretStore` into `dev_server.rs`**

In `crates/edgezero-adapter-axum/src/dev_server.rs`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecretInitRequirement {
    Optional,
    Required,
}

fn secret_init_requirement(
    manifest: &edgezero_core::manifest::Manifest,
) -> SecretInitRequirement {
    if manifest.stores.secrets.is_some() {
        SecretInitRequirement::Required
    } else {
        SecretInitRequirement::Optional
    }
}

fn secret_handle_from_env(
    store_name: &str,
    requirement: SecretInitRequirement,
) -> Option<edgezero_core::secret_store::SecretHandle> {
    let store = std::sync::Arc::new(crate::secret_store::EnvSecretStore::new());
    let handle = edgezero_core::secret_store::SecretHandle::new(store);
    if requirement == SecretInitRequirement::Required {
        log::info!("Secret store '{}': reading from environment variables", store_name);
    }
    Some(handle)
}
```

Update `serve_with_listener_and_kv_handle` — rename it to `serve_with_listener_and_stores` and add secret handle parameter:

```rust
async fn serve_with_listener_and_stores(
    router: RouterService,
    listener: tokio::net::TcpListener,
    enable_ctrl_c: bool,
    kv_handle: Option<edgezero_core::key_value_store::KvHandle>,
    secret_handle: Option<edgezero_core::secret_store::SecretHandle>,
) -> anyhow::Result<()> {
    let mut service = EdgeZeroAxumService::new(router);
    if let Some(kv_handle) = kv_handle {
        service = service.with_kv_handle(kv_handle);
    }
    if let Some(secret_handle) = secret_handle {
        service = service.with_secret_handle(secret_handle);
    }
    // ... rest same as current serve_with_listener_and_kv_handle
}
```

Update all three callers of `serve_with_listener_and_kv_handle` — enumerate them explicitly so nothing is missed:

1. `serve_with_listener_and_kv_path` (line ~199) — change its call to `serve_with_listener_and_stores(..., None)` (no secrets; this path is used by the manifest-unaware `AxumDevServer::run` embedding API)
2. `serve_with_listener` (line ~187) — delegates to `serve_with_listener_and_kv_path`, no change needed here beyond the above
3. `run_app` (line ~297) — updated below to pass a real `secret_handle`

The private test helper `AxumDevServer::run_with_listener` calls `serve_with_listener_and_kv_path`, so updating #1 above covers it automatically.

Update `run_app` to initialize and pass the secret handle:

```rust
pub fn run_app<A: Hooks>(manifest_src: &str) -> anyhow::Result<()> {
    let manifest = ManifestLoader::load_from_str(manifest_src);
    let manifest = manifest.manifest();
    // ... existing kv setup ...
    let secret_init_requirement = secret_init_requirement(manifest);
    let secret_store_name = manifest.secret_store_name("axum").to_string();

    // ... in async block, after kv_handle:
    let secret_handle =
        secret_handle_from_env(&secret_store_name, secret_init_requirement);
    serve_with_listener_and_stores(router, listener, config.enable_ctrl_c, kv_handle, secret_handle).await
}
```

- [ ] **Step 6.6: Update `lib.rs` to export `EnvSecretStore` and `secret_store` module**

In `crates/edgezero-adapter-axum/src/lib.rs`, add:
```rust
#[cfg(feature = "axum")]
pub mod secret_store;

// in pub use:
#[cfg(feature = "axum")]
pub use secret_store::EnvSecretStore;
```

- [ ] **Step 6.7: Run all axum adapter tests**

```bash
cargo test -p edgezero-adapter-axum 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 6.8: Commit**

```bash
git add crates/edgezero-adapter-axum/src/secret_store.rs \
        crates/edgezero-adapter-axum/src/lib.rs \
        crates/edgezero-adapter-axum/src/service.rs \
        crates/edgezero-adapter-axum/src/dev_server.rs
git commit -m "feat(axum): add EnvSecretStore for local dev and wire into service/dev server"
```

---

## Task 7: CLI build-time validation

Closes #66.

**Files:**
- Modify: `crates/edgezero-cli/src/main.rs`

This task adds informational output during `edgezero build` so developers know what secret store bindings they need to configure on each platform.

- [ ] **Step 7.1: Write test for secret store info message**

Add to `crates/edgezero-cli/src/main.rs` test block:

```rust
#[test]
fn secret_store_name_is_readable_from_manifest() {
    let manifest_with_secrets = r#"
[app]
name = "demo-app"
entry = "crates/demo-core"

[stores.secrets]
name = "MY_SECRETS"

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
"#;
    let loader = ManifestLoader::load_from_str(manifest_with_secrets);
    assert_eq!(
        loader.manifest().secret_store_name("fastly"),
        "MY_SECRETS"
    );
    assert!(loader.manifest().stores.secrets.is_some());
}
```

- [ ] **Step 7.2: Add `log_store_bindings` function and call it from `handle_build`**

In `crates/edgezero-cli/src/main.rs`, add this function:

```rust
#[cfg(feature = "cli")]
fn log_store_bindings(adapter_name: &str, manifest: &ManifestLoader) {
    let m = manifest.manifest();
    if let Some(ref secrets) = m.stores.secrets {
        let binding_name = m.secret_store_name(adapter_name);
        println!(
            "[edgezero] secret store '{binding_name}' declared -- \
             ensure it is provisioned on the {adapter_name} platform \
             (global name: '{}')",
            secrets.name
        );
    }
}
```

Update `handle_build` to call it:

```rust
#[cfg(feature = "cli")]
fn handle_build(adapter_name: &str, adapter_args: &[String]) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    if let Some(ref m) = manifest {
        log_store_bindings(adapter_name, m);
    }
    adapter::execute(
        adapter_name,
        adapter::Action::Build,
        manifest.as_ref(),
        adapter_args,
    )
}
```

- [ ] **Step 7.3: Run tests**

```bash
cargo test -p edgezero-cli 2>&1 | tail -15
```

Expected: all tests pass.

- [ ] **Step 7.4: Commit**

```bash
git add crates/edgezero-cli/src/main.rs
git commit -m "feat(cli): log secret store binding info during edgezero build"
```

---

## Task 8: Secret store trait contract tests and compile-time adapter type checks

Closes #67.

**What this task actually verifies:**
- `InMemorySecretStore` in `edgezero-core`: proves the `SecretStore` trait contract (get existing, get missing, read two different keys)
- Axum contract invocation: runs the same macro against `InMemorySecretStore` to prove the macro is importable from adapter crates
- `EnvSecretStore` behavior: tested independently in Task 6 unit tests (env var present/absent scenarios)
- Fastly / Cloudflare: compile-time checks only — `FastlySecretStore` and `CloudflareSecretStore` implement `SecretStore` (platform calls cannot run in CI)

**Files:**
- Modify: `crates/edgezero-adapter-axum/Cargo.toml` (add `edgezero-core` with `test-utils` to dev-dependencies)
- Modify: `crates/edgezero-adapter-axum/src/secret_store.rs` (add contract tests using InMemorySecretStore)
- Modify: `crates/edgezero-core/src/secret_store.rs` (add contract test invocation for InMemorySecretStore)
- Modify: `crates/edgezero-adapter-fastly/tests/contract.rs` (add compile-only secret store stub)
- Modify: `crates/edgezero-adapter-cloudflare/tests/contract.rs` (add compile-only secret store stub)

- [ ] **Step 8.1: Enable `test-utils` in axum adapter's dev-dependencies**

`InMemorySecretStore` is gated behind `#[cfg(any(test, feature = "test-utils"))]` in `edgezero-core`. The `test` cfg only applies within `edgezero-core`'s own test compilation — external crates cannot see it unless the feature is explicitly enabled. Add to `crates/edgezero-adapter-axum/Cargo.toml`:

```toml
[dev-dependencies]
# ... existing dev-dependencies ...
edgezero-core = { workspace = true, features = ["test-utils"] }
```

Check if `edgezero-core` already appears in `[dev-dependencies]`; if so, just add `features = ["test-utils"]` to the existing entry.

- [ ] **Step 8.2: Add `InMemorySecretStore` contract test in `edgezero-core`**

In `crates/edgezero-core/src/secret_store.rs`, inside the `#[cfg(test)]` module, add:

```rust
use crate::secret_store_contract_tests;

secret_store_contract_tests!(in_memory_contract, {
    InMemorySecretStore::new([
        ("contract_key", Bytes::from("contract_value")),
        ("contract_key_2", Bytes::from("another_value")),
    ])
});
```

- [ ] **Step 8.3: Add `EnvSecretStore` contract test in axum adapter**

In `crates/edgezero-adapter-axum/src/secret_store.rs`, in the `#[cfg(test)]` module, add after the existing tests:

```rust
// Contract tests: use InMemorySecretStore since EnvSecretStore needs
// real env vars, which are unsafe in parallel tests.
// The EnvSecretStore is tested individually above.
use edgezero_core::secret_store::InMemorySecretStore;
use edgezero_core::secret_store_contract_tests;

secret_store_contract_tests!(env_secret_contract, {
    InMemorySecretStore::new([
        ("contract_key", Bytes::from("contract_value")),
        ("contract_key_2", Bytes::from("another_value")),
    ])
});
```

Note: We test `EnvSecretStore` behavior directly in its own unit tests (Steps 6.1–6.2). The contract tests use `InMemorySecretStore` to verify interface contract without env var race conditions.

- [ ] **Step 8.4: Add compile-only secret store stubs to Fastly contract tests**

In `crates/edgezero-adapter-fastly/tests/contract.rs`, add at the bottom:

```rust
// Secret store contract tests for Fastly require a running Fastly Compute
// environment and cannot be executed in CI. The FastlySecretStore type is
// verified at compile time here.
#[cfg(all(feature = "fastly", target_arch = "wasm32"))]
mod secret_store_compile_check {
    use edgezero_adapter_fastly::FastlySecretStore;
    use edgezero_core::secret_store::SecretStore;

    // Compile-time check: FastlySecretStore implements SecretStore
    fn _assert_impl<T: SecretStore>() {}
    fn _check() {
        // This function is never called; it only verifies trait impl at compile time.
        _assert_impl::<FastlySecretStore>();
    }
}
```

- [ ] **Step 8.5: Add compile-only secret store stubs to Cloudflare contract tests**

In `crates/edgezero-adapter-cloudflare/tests/contract.rs`, add at the bottom:

```rust
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod secret_store_compile_check {
    use edgezero_adapter_cloudflare::CloudflareSecretStore;
    use edgezero_core::secret_store::SecretStore;

    fn _assert_impl<T: SecretStore>() {}
    fn _check() {
        _assert_impl::<CloudflareSecretStore>();
    }
}
```

- [ ] **Step 8.6: Run full test suite**

```bash
cargo test --workspace --all-targets 2>&1 | tail -30
```

Expected: all tests pass.

- [ ] **Step 8.7: Run full CI gate checks**

```bash
cargo fmt --all -- --check && \
cargo clippy --workspace --all-targets --all-features -- -D warnings && \
cargo test --workspace --all-targets && \
cargo check --workspace --all-targets --features "fastly cloudflare"
```

Expected: all four pass.

- [ ] **Step 8.7: Commit**

```bash
git add crates/edgezero-core/src/secret_store.rs \
        crates/edgezero-adapter-axum/src/secret_store.rs \
        crates/edgezero-adapter-fastly/tests/contract.rs \
        crates/edgezero-adapter-cloudflare/tests/contract.rs
git commit -m "test: add secret store contract tests across all adapters"
```

---

## Summary

| Task | Files Changed | Tests Added | Closes |
|------|---------------|-------------|--------|
| 1 | `core/secret_store.rs`, `core/lib.rs` | 8 unit tests | #60 |
| 2 | `core/context.rs`, `core/extractor.rs` | 4 unit tests | #64 |
| 3 | `core/manifest.rs` | 5 unit tests | #65 |
| 4 | `fastly/secret_store.rs`, `fastly/request.rs`, `fastly/lib.rs` | compile check | #61 |
| 5 | `cloudflare/secret_store.rs`, `cloudflare/request.rs`, `cloudflare/lib.rs` | compile check | #62 |
| 6 | `axum/secret_store.rs`, `axum/lib.rs`, `axum/service.rs`, `axum/dev_server.rs` | 3 unit + 1 service test | #63 |
| 7 | `cli/main.rs` | 1 unit test | #66 |
| 8 | multiple | 3 contract tests + 2 compile checks | #67 |

**Usage example** (after implementation):

```toml
# edgezero.toml
[stores.secrets]
name = "MY_APP_SECRETS"

[stores.secrets.adapters.fastly]
name = "MY_APP_SECRETS"  # Fastly SecretStore name in fastly.toml
```

```rust
// handler
#[action]
pub async fn fetch_data(Secrets(secrets): Secrets) -> Result<Response, EdgeError> {
    let api_key = secrets.require_str("API_KEY").await.map_err(EdgeError::from)?;
    // use api_key ...
}
```
