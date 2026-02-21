//! Provider-neutral Key-Value store abstraction.
//!
//! # Architecture
//!
//! ```text
//!  Handler code          KvHandle (generic get<T>/put<T>)
//!      │                       │
//!      └── Kv extractor ──────►│  serde_json layer
//!                              │
//!                         Arc<dyn KvStore>  (object-safe, Bytes)
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!      PersistentKvStore  FastlyKvStore  CloudflareKvStore
//! ```
//!
//! # Consistency Model
//!
//! Both Fastly and Cloudflare KV stores are **eventually consistent**.
//! A value written at one edge location may not be immediately visible
//! at another. Design handlers accordingly — do not assume
//! read-after-write consistency across locations.
//!
//! # Usage
//!
//! Access the KV store in a handler via [`crate::context::RequestContext::kv_handle`]:
//!
//! ```rust,ignore
//! async fn visit_counter(ctx: RequestContext) -> Result<String, EdgeError> {
//!     let kv = ctx.kv_handle().expect("kv store configured");
//!     let count: i32 = kv.update("visits", 0, |n| n + 1).await?;
//!     Ok(format!("Visit #{count}"))
//! }
//! ```
//!
//! Or use the [`crate::extractor::Kv`] extractor with the `#[action]` macro:
//!
//! ```rust,ignore
//! #[action]
//! async fn visit_counter(Kv(store): Kv) -> Result<String, EdgeError> {
//!     let count: i32 = store.update("visits", 0, |n| n + 1).await?;
//!     Ok(format!("Visit #{count}"))
//! }
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::EdgeError;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned by KV store operations.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// The requested key was not found (used by `delete` when strict).
    #[error("key not found: {key}")]
    NotFound { key: String },

    /// The KV store backend is temporarily unavailable.
    #[error("kv store unavailable")]
    Unavailable,

    /// A validation error (e.g., invalid key or value).
    #[error("validation error: {0}")]
    Validation(String),

    /// A serialization or deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A general internal error.
    #[error("kv store error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<KvError> for EdgeError {
    fn from(err: KvError) -> Self {
        match err {
            KvError::NotFound { key } => EdgeError::not_found(format!("kv key: {key}")),
            KvError::Unavailable => EdgeError::internal(anyhow::anyhow!("kv store unavailable")),
            KvError::Validation(e) => EdgeError::bad_request(format!("kv validation error: {e}")),
            KvError::Serialization(e) => {
                EdgeError::bad_request(format!("kv serialization error: {e}"))
            }
            KvError::Internal(e) => EdgeError::internal(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Object-safe interface for KV store backends.
///
/// All methods take `&self` — backends handle concurrency internally
/// (e.g., platform APIs, or `Mutex` for in-memory stores).
///
/// Implementations exist per adapter:
/// - `PersistentKvStore` (axum adapter) — local dev / tests with persistent storage
/// - `FastlyKvStore` (fastly adapter) — Fastly KV Store
/// - `CloudflareKvStore` (cloudflare adapter) — Cloudflare Workers KV
#[async_trait(?Send)]
pub trait KvStore: Send + Sync {
    /// Retrieve raw bytes for a key. Returns `Ok(None)` if the key does not exist.
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError>;

    /// Store raw bytes for a key, overwriting any existing value.
    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError>;

    /// Store raw bytes with a time-to-live. After `ttl` has elapsed the key
    /// should be treated as expired (exact eviction timing depends on the backend).
    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError>;

    /// Delete a key. Returns `Ok(())` even if the key did not exist.
    async fn delete(&self, key: &str) -> Result<(), KvError>;

    /// List keys that start with `prefix`. Returns an empty vec if none match.
    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError>;

    /// Check whether a key exists.
    ///
    /// The default implementation delegates to `get_bytes`. Backends that
    /// support a cheaper existence check should override this.
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.get_bytes(key).await?.is_some())
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A cloneable, ergonomic handle to a KV store.
///
/// Provides generic `get<T>` / `put<T>` helpers that serialize via JSON,
/// while delegating to the object-safe `KvStore` trait underneath.
///
/// # Examples
///
/// ```ignore
/// #[action]
/// async fn handler(Kv(store): Kv) -> Result<Response, EdgeError> {
///     let count: i32 = store.get_or("visits", 0).await?;
///     store.put("visits", &(count + 1)).await?;
///     Ok(Response::ok(format!("visits: {}", count + 1)))
/// }
/// ```
#[derive(Clone)]
pub struct KvHandle {
    store: Arc<dyn KvStore>,
}

impl fmt::Debug for KvHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvHandle").finish_non_exhaustive()
    }
}

impl KvHandle {
    /// Maximum key size in bytes (Cloudflare limit).
    pub const MAX_KEY_SIZE: usize = 512;

    /// Maximum value size in bytes (Standard limit).
    pub const MAX_VALUE_SIZE: usize = 25 * 1024 * 1024;

    /// Minimum TTL in seconds (Cloudflare limit).
    pub const MIN_TTL: Duration = Duration::from_secs(60);

    /// Create a new handle wrapping a KV store implementation.
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self { store }
    }

    // -- Validation ---------------------------------------------------------

    fn validate_key(key: &str) -> Result<(), KvError> {
        if key.len() > Self::MAX_KEY_SIZE {
            return Err(KvError::Validation(format!(
                "key length {} exceeds limit of {} bytes",
                key.len(),
                Self::MAX_KEY_SIZE
            )));
        }
        if key == "." || key == ".." {
            return Err(KvError::Validation(
                "key cannot be exactly '.' or '..'".to_string(),
            ));
        }
        if key.chars().any(|c| c.is_control()) {
            return Err(KvError::Validation(
                "key contains invalid control characters".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_value(value: &[u8]) -> Result<(), KvError> {
        if value.len() > Self::MAX_VALUE_SIZE {
            return Err(KvError::Validation(format!(
                "value size {} exceeds limit of 25MB",
                value.len()
            )));
        }
        Ok(())
    }

    fn validate_ttl(ttl: Duration) -> Result<(), KvError> {
        if ttl < Self::MIN_TTL {
            return Err(KvError::Validation(format!(
                "TTL {:?} is less than minimum of at least 60 seconds",
                ttl
            )));
        }
        Ok(())
    }

    // -- Typed helpers (JSON) -----------------------------------------------

    /// Get a value by key, deserializing from JSON.
    ///
    /// Returns `Ok(None)` if the key does not exist.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, KvError> {
        Self::validate_key(key)?;
        match self.store.get_bytes(key).await? {
            Some(bytes) => {
                let val = serde_json::from_slice(&bytes)?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Get a value by key, returning `default` if the key does not exist.
    pub async fn get_or<T: DeserializeOwned>(&self, key: &str, default: T) -> Result<T, KvError> {
        Ok(self.get(key).await?.unwrap_or(default))
    }

    /// Put a value, serializing it to JSON.
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), KvError> {
        Self::validate_key(key)?;
        let bytes = serde_json::to_vec(value)?;
        Self::validate_value(&bytes)?;
        self.store.put_bytes(key, Bytes::from(bytes)).await
    }

    /// Put a value with a TTL, serializing it to JSON.
    pub async fn put_with_ttl<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<(), KvError> {
        Self::validate_key(key)?;
        Self::validate_ttl(ttl)?;
        let bytes = serde_json::to_vec(value)?;
        Self::validate_value(&bytes)?;
        self.store
            .put_bytes_with_ttl(key, Bytes::from(bytes), ttl)
            .await
    }

    /// Read-modify-write: get the current value (or `default`),
    /// apply `f`, and write the result back.
    ///
    /// Returns the **new** (post-mutation) value. If you also need the
    /// previous value, read it separately before calling `update`.
    ///
    /// # Warning
    ///
    /// This operation is **not atomic**. The read and write are separate
    /// calls to the backend. Concurrent `update` calls on the same key
    /// may cause lost writes. Use this only when eventual consistency
    /// is acceptable (e.g., approximate counters).
    pub async fn update<T, F>(&self, key: &str, default: T, f: F) -> Result<T, KvError>
    where
        T: DeserializeOwned + Serialize,
        F: FnOnce(T) -> T,
    {
        // Validation happens in get_or and put
        let current = self.get_or(key, default).await?;
        let updated = f(current);
        self.put(key, &updated).await?;
        Ok(updated)
    }

    // -- Raw bytes ----------------------------------------------------------

    /// Get raw bytes for a key.
    pub async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        Self::validate_key(key)?;
        self.store.get_bytes(key).await
    }

    /// Put raw bytes for a key.
    pub async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        Self::validate_key(key)?;
        Self::validate_value(&value)?;
        self.store.put_bytes(key, value).await
    }

    /// Put raw bytes with a TTL.
    pub async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        Self::validate_key(key)?;
        Self::validate_ttl(ttl)?;
        Self::validate_value(&value)?;
        self.store.put_bytes_with_ttl(key, value, ttl).await
    }

    // -- Other operations ---------------------------------------------------

    /// Check whether a key exists without deserializing its value.
    pub async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Self::validate_key(key)?;
        self.store.exists(key).await
    }

    /// Delete a key.
    pub async fn delete(&self, key: &str) -> Result<(), KvError> {
        Self::validate_key(key)?;
        self.store.delete(key).await
    }

    /// List keys with the given prefix.
    pub async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
        // We generally allow validation on list prefixes too in strict environments,
        // but often prefixes are short. We'll strict check it doesn't exceed key limits.
        Self::validate_key(prefix)?;
        self.store.list_keys(prefix).await
    }
}

// ---------------------------------------------------------------------------
// Contract test macro
// ---------------------------------------------------------------------------

/// Generate a suite of contract tests for any [`KvStore`] implementation.
///
/// The macro takes the module name and a factory expression that produces a
/// fresh store instance (implementing `KvStore`). It generates a module
/// containing tests that verify the fundamental behaviours every backend
/// must satisfy.
///
/// # Example
///
/// ```rust,ignore
/// edgezero_core::kv_contract_tests!(persistent_kv_contract, {
///     let temp_dir = tempfile::tempdir().unwrap();
///     let db_path = temp_dir.path().join("test.redb");
///     PersistentKvStore::new(db_path).unwrap()
/// });
/// ```
#[macro_export]
macro_rules! kv_contract_tests {
    ($mod_name:ident, $factory:expr) => {
        mod $mod_name {
            use super::*;
            use bytes::Bytes;
            use $crate::kv::KvStore;

            fn run<F: std::future::Future>(f: F) -> F::Output {
                futures::executor::block_on(f)
            }

            #[test]
            fn contract_put_and_get() {
                let store = $factory;
                run(async {
                    store.put_bytes("k", Bytes::from("v")).await.unwrap();
                    assert_eq!(store.get_bytes("k").await.unwrap(), Some(Bytes::from("v")));
                });
            }

            #[test]
            fn contract_get_missing_returns_none() {
                let store = $factory;
                run(async {
                    assert_eq!(store.get_bytes("missing").await.unwrap(), None);
                });
            }

            #[test]
            fn contract_put_overwrites() {
                let store = $factory;
                run(async {
                    store.put_bytes("k", Bytes::from("first")).await.unwrap();
                    store.put_bytes("k", Bytes::from("second")).await.unwrap();
                    assert_eq!(
                        store.get_bytes("k").await.unwrap(),
                        Some(Bytes::from("second"))
                    );
                });
            }

            #[test]
            fn contract_delete_removes_key() {
                let store = $factory;
                run(async {
                    store.put_bytes("k", Bytes::from("v")).await.unwrap();
                    store.delete("k").await.unwrap();
                    assert_eq!(store.get_bytes("k").await.unwrap(), None);
                });
            }

            #[test]
            fn contract_delete_nonexistent_ok() {
                let store = $factory;
                run(async {
                    store.delete("nope").await.unwrap();
                });
            }

            #[test]
            fn contract_list_keys_prefix() {
                let store = $factory;
                run(async {
                    store.put_bytes("a:1", Bytes::from("v")).await.unwrap();
                    store.put_bytes("a:2", Bytes::from("v")).await.unwrap();
                    store.put_bytes("b:1", Bytes::from("v")).await.unwrap();

                    let mut keys = store.list_keys("a:").await.unwrap();
                    keys.sort();
                    assert_eq!(keys, vec!["a:1", "a:2"]);
                });
            }

            #[test]
            fn contract_list_keys_empty_store() {
                let store = $factory;
                run(async {
                    let keys = store.list_keys("").await.unwrap();
                    assert!(keys.is_empty());
                });
            }

            #[test]
            fn contract_exists() {
                let store = $factory;
                run(async {
                    assert!(!store.exists("k").await.unwrap());
                    store.put_bytes("k", Bytes::from("v")).await.unwrap();
                    assert!(store.exists("k").await.unwrap());
                    store.delete("k").await.unwrap();
                    assert!(!store.exists("k").await.unwrap());
                });
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Minimal in-memory store for testing the handle/trait contract
    struct MockStore {
        data: Mutex<HashMap<String, Bytes>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl KvStore for MockStore {
        async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
            let data = self.data.lock().unwrap();
            Ok(data.get(key).cloned())
        }

        async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
            let mut data = self.data.lock().unwrap();
            data.insert(key.to_string(), value);
            Ok(())
        }

        async fn put_bytes_with_ttl(
            &self,
            key: &str,
            value: Bytes,
            _ttl: Duration,
        ) -> Result<(), KvError> {
            // MockStore ignores TTL for simplicity
            self.put_bytes(key, value).await
        }

        async fn delete(&self, key: &str) -> Result<(), KvError> {
            let mut data = self.data.lock().unwrap();
            data.remove(key);
            Ok(())
        }

        async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
            let data = self.data.lock().unwrap();
            let mut keys: Vec<String> = data
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect();
            keys.sort();
            Ok(keys)
        }
    }

    fn handle() -> KvHandle {
        KvHandle::new(Arc::new(MockStore::new()))
    }

    // -- Raw bytes ----------------------------------------------------------

    #[tokio::test]
    async fn raw_bytes_roundtrip() {
        let h = handle();
        h.put_bytes("k", Bytes::from("hello")).await.unwrap();
        assert_eq!(h.get_bytes("k").await.unwrap(), Some(Bytes::from("hello")));
    }

    #[tokio::test]
    async fn raw_bytes_missing_key_returns_none() {
        let h = handle();
        assert_eq!(h.get_bytes("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn raw_bytes_overwrite() {
        let h = handle();
        h.put_bytes("k", Bytes::from("a")).await.unwrap();
        h.put_bytes("k", Bytes::from("b")).await.unwrap();
        assert_eq!(h.get_bytes("k").await.unwrap(), Some(Bytes::from("b")));
    }

    // -- Typed JSON ---------------------------------------------------------

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Counter {
        count: i32,
    }

    #[tokio::test]
    async fn typed_get_put_roundtrip() {
        let h = handle();
        let data = Counter { count: 42 };
        h.put("counter", &data).await.unwrap();
        let out: Option<Counter> = h.get("counter").await.unwrap();
        assert_eq!(out, Some(data));
    }

    #[tokio::test]
    async fn typed_get_missing_returns_none() {
        let h = handle();
        let out: Option<Counter> = h.get("nope").await.unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn typed_get_or_returns_default() {
        let h = handle();
        let count: i32 = h.get_or("visits", 0).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn typed_get_or_returns_existing() {
        let h = handle();
        h.put("visits", &99).await.unwrap();
        let count: i32 = h.get_or("visits", 0).await.unwrap();
        assert_eq!(count, 99);
    }

    #[tokio::test]
    async fn typed_get_bad_json_returns_serialization_error() {
        let h = handle();
        h.put_bytes("bad", Bytes::from("not json")).await.unwrap();
        let err = h.get::<Counter>("bad").await.unwrap_err();
        assert!(matches!(err, KvError::Serialization(_)));
    }

    // -- Update -------------------------------------------------------------

    #[tokio::test]
    async fn update_increments_counter() {
        let h = handle();
        h.put("c", &0i32).await.unwrap();
        let val = h.update("c", 0i32, |n| n + 1).await.unwrap();
        assert_eq!(val, 1);
        let val = h.update("c", 0i32, |n| n + 1).await.unwrap();
        assert_eq!(val, 2);
    }

    #[tokio::test]
    async fn update_uses_default_when_missing() {
        let h = handle();
        let val = h.update("new", 10i32, |n| n * 2).await.unwrap();
        assert_eq!(val, 20);
    }

    // -- Exists -------------------------------------------------------------

    #[tokio::test]
    async fn exists_returns_false_for_missing() {
        let h = handle();
        assert!(!h.exists("nope").await.unwrap());
    }

    #[tokio::test]
    async fn exists_returns_true_for_present() {
        let h = handle();
        h.put_bytes("k", Bytes::from("v")).await.unwrap();
        assert!(h.exists("k").await.unwrap());
    }

    // -- Delete -------------------------------------------------------------

    #[tokio::test]
    async fn delete_removes_key() {
        let h = handle();
        h.put_bytes("k", Bytes::from("v")).await.unwrap();
        h.delete("k").await.unwrap();
        assert_eq!(h.get_bytes("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_missing_key_is_ok() {
        let h = handle();
        h.delete("nope").await.unwrap();
    }

    // -- List keys ----------------------------------------------------------

    #[tokio::test]
    async fn list_keys_with_prefix() {
        let h = handle();
        h.put_bytes("user:1", Bytes::from("a")).await.unwrap();
        h.put_bytes("user:2", Bytes::from("b")).await.unwrap();
        h.put_bytes("session:1", Bytes::from("c")).await.unwrap();

        let keys = h.list_keys("user:").await.unwrap();
        assert_eq!(keys, vec!["user:1", "user:2"]);
    }

    #[tokio::test]
    async fn list_keys_empty_prefix_returns_all() {
        let h = handle();
        h.put_bytes("a", Bytes::from("1")).await.unwrap();
        h.put_bytes("b", Bytes::from("2")).await.unwrap();

        let keys = h.list_keys("").await.unwrap();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn list_keys_no_matches() {
        let h = handle();
        h.put_bytes("a", Bytes::from("1")).await.unwrap();
        let keys = h.list_keys("zzz").await.unwrap();
        assert!(keys.is_empty());
    }

    // -- TTL ----------------------------------------------------------------

    #[tokio::test]
    async fn put_with_ttl_stores_value() {
        let h = handle();
        h.put_with_ttl("session", &"token123", Duration::from_secs(60))
            .await
            .unwrap();
        let val: Option<String> = h.get("session").await.unwrap();
        assert_eq!(val, Some("token123".to_string()));
    }

    // -- KvError -> EdgeError -----------------------------------------------

    #[test]
    fn kv_error_not_found_converts_to_not_found() {
        let kv_err = KvError::NotFound { key: "test".into() };
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), http::StatusCode::NOT_FOUND);
        assert!(edge_err.message().contains("kv key"));
    }

    #[test]
    fn kv_error_unavailable_converts_to_internal() {
        let kv_err = KvError::Unavailable;
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn kv_error_internal_converts_to_internal() {
        let kv_err = KvError::Internal(anyhow::anyhow!("boom"));
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(edge_err.message().contains("boom"));
    }

    // -- Clone handle -------------------------------------------------------

    #[tokio::test]
    async fn handle_is_cloneable_and_shares_state() {
        let h1 = handle();
        let h2 = h1.clone();
        h1.put("shared", &42i32).await.unwrap();
        let val: i32 = h2.get_or("shared", 0).await.unwrap();
        assert_eq!(val, 42);
    }

    // -- Edge cases ---------------------------------------------------------

    #[tokio::test]
    async fn empty_key_roundtrip() {
        let h = handle();
        h.put("", &"empty key").await.unwrap();
        let val: Option<String> = h.get("").await.unwrap();
        assert_eq!(val, Some("empty key".to_string()));
    }

    #[tokio::test]
    async fn unicode_key_roundtrip() {
        let h = handle();
        h.put("日本語キー", &"value").await.unwrap();
        let val: Option<String> = h.get("日本語キー").await.unwrap();
        assert_eq!(val, Some("value".to_string()));
    }

    #[tokio::test]
    async fn large_value_roundtrip() {
        let h = handle();
        let large = "x".repeat(1_000_000); // 1MB string
        h.put("big", &large).await.unwrap();
        let val: Option<String> = h.get("big").await.unwrap();
        assert_eq!(val.as_deref(), Some(large.as_str()));
    }

    #[tokio::test]
    async fn put_with_ttl_typed_helper() {
        let h = handle();
        let data = Counter { count: 7 };
        h.put_with_ttl("ttl_key", &data, Duration::from_secs(600))
            .await
            .unwrap();
        let val: Option<Counter> = h.get("ttl_key").await.unwrap();
        assert_eq!(val, Some(Counter { count: 7 }));
    }

    #[tokio::test]
    async fn get_or_with_complex_default() {
        let h = handle();
        let default = Counter { count: 100 };
        let val: Counter = h.get_or("missing_struct", default).await.unwrap();
        assert_eq!(val.count, 100);
    }

    #[tokio::test]
    async fn update_with_struct() {
        let h = handle();
        let val = h
            .update("counter_struct", Counter { count: 0 }, |mut c| {
                c.count += 10;
                c
            })
            .await
            .unwrap();
        assert_eq!(val.count, 10);

        let val = h
            .update("counter_struct", Counter { count: 0 }, |mut c| {
                c.count += 5;
                c
            })
            .await
            .unwrap();
        assert_eq!(val.count, 15);
    }

    #[test]
    fn kv_error_serialization_converts_to_bad_request() {
        let json_err: serde_json::Error = serde_json::from_str::<i32>("not json").unwrap_err();
        let kv_err = KvError::Serialization(json_err);
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), http::StatusCode::BAD_REQUEST);
        assert!(edge_err.message().contains("serialization"));
    }

    #[test]
    fn kv_handle_debug_output() {
        let h = handle();
        let debug = format!("{:?}", h);
        assert!(debug.contains("KvHandle"));
    }

    // -- Validation Tests ---------------------------------------------------

    #[tokio::test]
    async fn validation_rejects_long_keys() {
        let h = handle();
        // MAX_KEY_SIZE + 1
        let long_key = "a".repeat(KvHandle::MAX_KEY_SIZE + 1);
        let err = h.get::<i32>(&long_key).await.unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("key length"));
    }

    #[tokio::test]
    async fn validation_rejects_dot_keys() {
        let h = handle();
        let err = h.get::<i32>(".").await.unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("cannot be exactly"));

        let err = h.get::<i32>("..").await.unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("cannot be exactly"));
    }

    #[tokio::test]
    async fn validation_rejects_control_chars() {
        let h = handle();
        let err = h.get::<i32>("key\nwith\nnewline").await.unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("control characters"));
    }

    #[tokio::test]
    async fn validation_rejects_large_values() {
        let h = handle();
        // MAX_VALUE_SIZE + 1 byte
        let large_val = vec![0u8; KvHandle::MAX_VALUE_SIZE + 1];
        let err = h
            .put_bytes("large", Bytes::from(large_val))
            .await
            .unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("value size"));
    }

    #[tokio::test]
    async fn validation_rejects_short_ttl() {
        let h = handle();
        let err = h
            .put_with_ttl("short", &"val", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(matches!(err, KvError::Validation(_)));
        assert!(format!("{}", err).contains("at least 60 seconds"));
    }

    #[tokio::test]
    async fn list_keys_overlapping_prefixes() {
        let h = handle();
        h.put_bytes("app:user:1", Bytes::from("a")).await.unwrap();
        h.put_bytes("app:user:2", Bytes::from("b")).await.unwrap();
        h.put_bytes("app:session:1", Bytes::from("c"))
            .await
            .unwrap();
        h.put_bytes("other:1", Bytes::from("d")).await.unwrap();

        let app_keys = h.list_keys("app:").await.unwrap();
        assert_eq!(app_keys.len(), 3);

        let user_keys = h.list_keys("app:user:").await.unwrap();
        assert_eq!(user_keys, vec!["app:user:1", "app:user:2"]);

        let session_keys = h.list_keys("app:session:").await.unwrap();
        assert_eq!(session_keys, vec!["app:session:1"]);
    }

    #[tokio::test]
    async fn exists_returns_false_after_delete() {
        let h = handle();
        h.put_bytes("ephemeral", Bytes::from("v")).await.unwrap();
        assert!(h.exists("ephemeral").await.unwrap());
        h.delete("ephemeral").await.unwrap();
        assert!(!h.exists("ephemeral").await.unwrap());
    }

    #[tokio::test]
    async fn put_overwrite_changes_type() {
        let h = handle();
        h.put("flex", &42i32).await.unwrap();
        let val: i32 = h.get_or("flex", 0).await.unwrap();
        assert_eq!(val, 42);

        // Overwrite with a different type
        h.put("flex", &"now a string").await.unwrap();
        let val: String = h.get_or("flex", String::new()).await.unwrap();
        assert_eq!(val, "now a string");
    }

    // Run the shared contract tests against MockStore.
    crate::kv_contract_tests!(mock_store_contract, MockStore::new());
}
