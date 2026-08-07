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
//! Use the [`crate::extractor::Kv`] extractor with the `#[action]`
//! macro and pick a store by id at the call site (portable
//! store API):
//!
//! ```rust,ignore
//! #[action]
//! async fn visit_counter(kv: Kv) -> Result<String, EdgeError> {
//!     let store = kv
//!         .default()
//!         .ok_or_else(|| EdgeError::service_unavailable("no default kv"))?;
//!     let count: i32 = store.read_modify_write("visits", 0, |n| n + 1).await?;
//!     Ok(format!("Visit #{count}"))
//! }
//! ```
//!
//! Or reach the store through [`crate::context::RequestContext`]
//! when you have a context instead of an extractor:
//!
//! ```rust,ignore
//! async fn visit_counter(ctx: RequestContext) -> Result<String, EdgeError> {
//!     let kv = ctx.kv_store_default().expect("default kv configured");
//!     let count: i32 = kv.read_modify_write("visits", 0, |n| n + 1).await?;
//!     Ok(format!("Visit #{count}"))
//! }
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use web_time::Instant;

use crate::error::EdgeError;

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
/// edgezero_core::key_value_store_contract_tests!(persistent_kv_contract, {
///     let db_path = std::env::temp_dir().join(format!(
///         "edgezero-contract-{}-{:?}.redb",
///         std::process::id(),
///         std::thread::current().id()
///     ));
///     PersistentKvStore::new(db_path).unwrap()
/// });
/// ```
#[macro_export]
macro_rules! key_value_store_contract_tests {
    ($mod_name:ident, $factory:expr) => {
        mod $mod_name {
            use super::*;
            use bytes::Bytes;
            use $crate::key_value_store::KvStore;

            fn run<Fut: std::future::Future>(future: Fut) -> Fut::Output {
                ::futures::executor::block_on(future)
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

            #[test]
            fn contract_put_with_ttl_stores_value() {
                let store = $factory;
                run(async {
                    store
                        .put_bytes_with_ttl(
                            "ttl_key",
                            Bytes::from("ttl_val"),
                            std::time::Duration::from_mins(5),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        store.get_bytes("ttl_key").await.unwrap(),
                        Some(Bytes::from("ttl_val"))
                    );
                });
            }

            // `std::thread::sleep` is not available on `wasm32` targets (no
            // thread support). The TTL eviction contract is verified on native
            // targets only; WASM adapters are expected to delegate eviction to
            // the platform runtime (Cloudflare/Fastly), which does not expose a
            // synchronous sleep primitive in test environments.
            #[cfg(not(target_arch = "wasm32"))]
            #[test]
            fn contract_ttl_expires() {
                let store = $factory;
                run(async {
                    // Uses a sub-second TTL intentionally. Contract tests call
                    // `KvStore` directly (not `KvHandle`), so the 60-second
                    // minimum TTL validation is bypassed. This lets us verify
                    // that the backend actually evicts expired entries.
                    store
                        .put_bytes_with_ttl(
                            "ephemeral",
                            Bytes::from("gone_soon"),
                            std::time::Duration::from_millis(1),
                        )
                        .await
                        .unwrap();
                    // Allow the TTL to elapse. 200ms gives the OS scheduler
                    // enough headroom on busy CI runners.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    assert_eq!(store.get_bytes("ephemeral").await.unwrap(), None);
                });
            }

            #[test]
            fn contract_list_keys_page_is_paginated() {
                let store = $factory;
                run(async {
                    let expected = vec![
                        "app/one".to_owned(),
                        "app/two".to_owned(),
                        "other/three".to_owned(),
                    ];
                    for key in &expected {
                        store
                            .put_bytes(key, Bytes::from(key.clone()))
                            .await
                            .unwrap();
                    }

                    let mut cursor = None;
                    let mut seen = std::collections::HashSet::new();
                    let mut collected = Vec::new();

                    for _ in 0..expected.len() {
                        let page = store
                            .list_keys_page("", cursor.as_deref(), 1)
                            .await
                            .unwrap();
                        assert!(page.keys.len() <= 1);
                        for key in &page.keys {
                            assert!(
                                seen.insert(key.clone()),
                                "duplicate key in pagination: {key}"
                            );
                            collected.push(key.clone());
                        }

                        cursor = page.cursor;
                        if cursor.is_none() {
                            break;
                        }
                    }

                    collected.sort();
                    let mut expected_sorted = expected.clone();
                    expected_sorted.sort();
                    assert_eq!(collected, expected_sorted);
                });
            }

            #[test]
            fn contract_list_keys_page_respects_prefix() {
                let store = $factory;
                run(async {
                    store
                        .put_bytes("prefix/a", Bytes::from_static(b"a"))
                        .await
                        .unwrap();
                    store
                        .put_bytes("prefix/b", Bytes::from_static(b"b"))
                        .await
                        .unwrap();
                    store
                        .put_bytes("other/c", Bytes::from_static(b"c"))
                        .await
                        .unwrap();

                    let first = store.list_keys_page("prefix/", None, 1).await.unwrap();
                    assert_eq!(first.keys.len(), 1);
                    assert!(first.keys[0].starts_with("prefix/"));

                    let second = store
                        .list_keys_page("prefix/", first.cursor.as_deref(), 1)
                        .await
                        .unwrap();
                    assert!(second.keys.iter().all(|key| key.starts_with("prefix/")));
                    assert!(
                        first
                            .keys
                            .iter()
                            .chain(second.keys.iter())
                            .all(|key| key.starts_with("prefix/"))
                    );
                });
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct KvCursorEnvelope {
    cursor: String,
    prefix: String,
}

/// Errors returned by KV store operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KvError {
    /// A general internal error.
    #[error("kv store error: {0}")]
    Internal(#[from] anyhow::Error),

    /// A backend listing or paging limit was exceeded (e.g. Spin's
    /// `max_list_keys` cap on `get_keys`).
    #[error("kv backend limit exceeded: {message}")]
    LimitExceeded { message: String },

    /// The requested key was not found (used by `delete` when strict).
    #[error("key not found: {key}")]
    NotFound { key: String },

    /// A serialization or deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The KV store backend is temporarily unavailable.
    #[error("kv store unavailable")]
    Unavailable,

    /// The operation is not supported by the active backend (e.g. TTL writes
    /// on Spin, where `key_value::Store::set` accepts no expiry).
    #[error("kv operation not supported by backend: {operation}")]
    Unsupported { operation: String },

    /// A validation error (e.g., invalid key or value).
    #[error("validation error: {0}")]
    Validation(String),
}

/// A cloneable, ergonomic handle to a KV store.
///
/// Provides generic `get<T>` / `put<T>` helpers that serialize via JSON,
/// while delegating to the object-safe `KvStore` trait underneath.
///
/// # Examples
///
/// ```ignore
/// #[action]
/// async fn handler(kv: Kv) -> Result<String, EdgeError> {
///     let store = kv
///         .default()
///         .ok_or_else(|| EdgeError::service_unavailable("no default kv"))?;
///     let count: i32 = store.get_or("visits", 0).await?;
///     store.put("visits", &(count + 1)).await?;
///     Ok(format!("visits: {}", count + 1))
/// }
/// ```
#[derive(Clone)]
pub struct KvHandle {
    store: Arc<dyn KvStore>,
}

impl fmt::Debug for KvHandle {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvHandle").finish_non_exhaustive()
    }
}

impl KvHandle {
    /// Maximum key size in bytes (Cloudflare limit).
    pub const MAX_KEY_SIZE: usize = 512;

    /// Maximum number of keys returned from a single page.
    pub const MAX_LIST_PAGE_SIZE: usize = 1_000;

    /// Maximum TTL (1 year). Prevents overflow when adding to `SystemTime::now()`.
    #[expect(
        clippy::duration_suboptimal_units,
        reason = "`Duration::from_days` is not stable in const context (1.95)"
    )]
    pub const MAX_TTL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

    /// Maximum value size in bytes (Standard limit).
    pub const MAX_VALUE_SIZE: usize = 25 * 1024 * 1024;

    /// Minimum TTL (Cloudflare limit).
    #[expect(
        clippy::duration_suboptimal_units,
        reason = "`Duration::from_mins` is not stable in const context (1.95)"
    )]
    pub const MIN_TTL: Duration = Duration::from_secs(60);

    fn decode_list_cursor(prefix: &str, cursor: Option<&str>) -> Result<Option<String>, KvError> {
        let Some(encoded) = cursor else {
            return Ok(None);
        };

        let envelope: KvCursorEnvelope = serde_json::from_str(encoded)
            .map_err(|_e| KvError::Validation("list cursor is invalid or corrupted".to_owned()))?;

        if envelope.prefix != prefix {
            return Err(KvError::Validation(
                "list cursor does not match the requested prefix".to_owned(),
            ));
        }
        if envelope.cursor.is_empty() {
            return Err(KvError::Validation(
                "list cursor payload cannot be empty".to_owned(),
            ));
        }

        Ok(Some(envelope.cursor))
    }

    /// Delete a key.
    ///
    /// # Errors
    /// Returns [`KvError`] if the backend rejects the delete.
    #[inline]
    pub async fn delete(&self, key: &str) -> Result<(), KvError> {
        Self::validate_key(key)?;
        let started_at = Self::kv_timing_start();
        let result = self.store.delete(key).await;
        Self::kv_timing_log(started_at, "delete", &result, || {
            format!("key_len={}", key.len())
        });
        result
    }

    fn encode_list_cursor(prefix: &str, cursor: Option<String>) -> Result<Option<String>, KvError> {
        cursor
            .map(|inner| {
                serde_json::to_string(&KvCursorEnvelope {
                    cursor: inner,
                    prefix: prefix.to_owned(),
                })
                .map_err(KvError::from)
            })
            .transpose()
    }

    /// Check whether a key exists without deserializing its value.
    ///
    /// # Errors
    /// Returns [`KvError`] if the backend lookup fails.
    #[inline]
    pub async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Self::validate_key(key)?;
        let started_at = Self::kv_timing_start();
        let result = self.store.exists(key).await;
        Self::kv_timing_log(started_at, "exists", &result, || {
            Self::kv_exists_metadata(key.len(), &result)
        });
        result
    }

    /// Get a value by key, deserializing from JSON.
    ///
    /// Returns `Ok(None)` if the key does not exist.
    ///
    /// # Errors
    /// Returns [`KvError`] if the lookup fails or the stored bytes cannot be deserialized into `T`.
    #[inline]
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, KvError> {
        Self::validate_key(key)?;
        let started_at = Self::kv_timing_start();
        let result = self.store.get_bytes(key).await;
        Self::kv_timing_log(started_at, "get", &result, || {
            Self::kv_read_metadata(key.len(), &result)
        });

        match result? {
            Some(bytes) => {
                let val = serde_json::from_slice(&bytes)?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Get raw bytes for a key.
    ///
    /// # Errors
    /// Returns [`KvError`] if the backend lookup fails.
    #[inline]
    pub async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        Self::validate_key(key)?;
        let started_at = Self::kv_timing_start();
        let result = self.store.get_bytes(key).await;
        Self::kv_timing_log(started_at, "get_bytes", &result, || {
            Self::kv_read_metadata(key.len(), &result)
        });
        result
    }

    /// Get a value by key, returning `default` if the key does not exist.
    ///
    /// # Errors
    /// Returns [`KvError`] if the lookup fails or the stored bytes cannot be deserialized into `T`.
    #[inline]
    pub async fn get_or<T: DeserializeOwned>(&self, key: &str, default: T) -> Result<T, KvError> {
        Ok(self.get(key).await?.unwrap_or(default))
    }

    fn kv_exists_metadata(key_len: usize, result: &Result<bool, KvError>) -> String {
        match result.as_ref() {
            Ok(exists) => format!("key_len={key_len} exists={exists}"),
            Err(_err) => format!("key_len={key_len}"),
        }
    }

    fn kv_hit_metadata(result: &Result<Option<Bytes>, KvError>) -> String {
        match result.as_ref() {
            Ok(Some(bytes)) => format!("hit=true bytes={}", bytes.len()),
            Ok(None) => "hit=false bytes=0".to_owned(),
            Err(_err) => String::new(),
        }
    }

    fn kv_list_metadata(
        prefix_len: usize,
        cursor_present: bool,
        limit: usize,
        result: &Result<KvPage, KvError>,
    ) -> String {
        match result.as_ref() {
            Ok(page) => format!(
                "prefix_len={prefix_len} cursor_present={cursor_present} limit={limit} count={} next_cursor_present={}",
                page.keys.len(),
                page.cursor.is_some()
            ),
            Err(_err) => {
                format!("prefix_len={prefix_len} cursor_present={cursor_present} limit={limit}")
            }
        }
    }

    fn kv_read_metadata(key_len: usize, result: &Result<Option<Bytes>, KvError>) -> String {
        match result {
            Ok(_value) => format!("key_len={key_len} {}", Self::kv_hit_metadata(result)),
            Err(_err) => format!("key_len={key_len}"),
        }
    }

    fn kv_timing_log<ResultValue, Metadata>(
        started_at: Option<Instant>,
        operation: &str,
        result: &Result<ResultValue, KvError>,
        metadata: Metadata,
    ) where
        Metadata: FnOnce() -> String,
    {
        if let Some(start) = started_at {
            let status = if result.is_ok() { "ok" } else { "error" };
            log::debug!(
                "kv operation={operation} elapsed_ms={} status={status} {}",
                start.elapsed().as_millis(),
                metadata()
            );
        }
    }

    fn kv_timing_start() -> Option<Instant> {
        log::log_enabled!(log::Level::Debug).then(Instant::now)
    }

    fn kv_write_metadata(key_len: usize, bytes_len: usize, ttl: Option<Duration>) -> String {
        match ttl {
            Some(duration) => format!(
                "key_len={key_len} bytes={bytes_len} ttl_secs={}",
                duration.as_secs()
            ),
            None => format!("key_len={key_len} bytes={bytes_len}"),
        }
    }

    /// List keys in a bounded, paginated fashion.
    ///
    /// The cursor is opaque, prefix-bound, and should be passed back unchanged
    /// with the same prefix to retrieve the next page. Listings are not atomic
    /// snapshots and may reflect concurrent writes or provider-level eventual
    /// consistency.
    ///
    /// # Errors
    /// Returns [`KvError::Validation`] if `cursor` is malformed or `prefix` exceeds backend limits; [`KvError::Internal`] on backend failure.
    #[inline]
    pub async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        Self::validate_prefix(prefix)?;
        Self::validate_list_limit(limit)?;
        let decoded_cursor = Self::decode_list_cursor(prefix, cursor)?;
        let started_at = Self::kv_timing_start();
        let result = self
            .store
            .list_keys_page(prefix, decoded_cursor.as_deref(), limit)
            .await;
        Self::kv_timing_log(started_at, "list_keys_page", &result, || {
            Self::kv_list_metadata(prefix.len(), cursor.is_some(), limit, &result)
        });
        let page = result?;

        Ok(KvPage {
            cursor: Self::encode_list_cursor(prefix, page.cursor)?,
            keys: page.keys,
        })
    }

    /// Create a new handle wrapping a KV store implementation.
    #[inline]
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self { store }
    }

    /// Put a value, serializing it to JSON.
    ///
    /// # Errors
    /// Returns [`KvError`] if the value cannot be serialized or the backend rejects the write.
    #[inline]
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), KvError> {
        Self::validate_key(key)?;
        let bytes = serde_json::to_vec(value)?;
        Self::validate_value(&bytes)?;
        let bytes_len = bytes.len();
        let started_at = Self::kv_timing_start();
        let result = self.store.put_bytes(key, Bytes::from(bytes)).await;
        Self::kv_timing_log(started_at, "put", &result, || {
            Self::kv_write_metadata(key.len(), bytes_len, None)
        });
        result
    }

    /// Put raw bytes for a key.
    ///
    /// # Errors
    /// Returns [`KvError::Validation`] for invalid keys or oversized values; [`KvError::Internal`] on backend failure.
    #[inline]
    pub async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        Self::validate_key(key)?;
        Self::validate_value(&value)?;
        let bytes_len = value.len();
        let started_at = Self::kv_timing_start();
        let result = self.store.put_bytes(key, value).await;
        Self::kv_timing_log(started_at, "put_bytes", &result, || {
            Self::kv_write_metadata(key.len(), bytes_len, None)
        });
        result
    }

    /// Put raw bytes with a TTL.
    ///
    /// # Errors
    /// Returns [`KvError::Validation`] for invalid input; [`KvError::Internal`] on backend failure.
    #[inline]
    pub async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        Self::validate_key(key)?;
        Self::validate_ttl(ttl)?;
        Self::validate_value(&value)?;
        let bytes_len = value.len();
        let started_at = Self::kv_timing_start();
        let result = self.store.put_bytes_with_ttl(key, value, ttl).await;
        Self::kv_timing_log(started_at, "put_bytes_with_ttl", &result, || {
            Self::kv_write_metadata(key.len(), bytes_len, Some(ttl))
        });
        result
    }

    /// Put a value with a TTL, serializing it to JSON.
    ///
    /// # Errors
    /// Returns [`KvError`] if the value cannot be serialized or the backend rejects the write.
    #[inline]
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
        let bytes_len = bytes.len();
        let started_at = Self::kv_timing_start();
        let result = self
            .store
            .put_bytes_with_ttl(key, Bytes::from(bytes), ttl)
            .await;
        Self::kv_timing_log(started_at, "put_with_ttl", &result, || {
            Self::kv_write_metadata(key.len(), bytes_len, Some(ttl))
        });
        result
    }

    /// Read-modify-write: get the current value (or `default`),
    /// apply `f`, and write the result back.
    ///
    /// Returns the **new** (post-mutation) value. If you also need the
    /// previous value, read it separately before calling this method.
    ///
    /// # Warning
    ///
    /// This operation is **not atomic**. The read and write are separate
    /// calls to the backend. Concurrent calls on the same key may cause
    /// lost writes. Use this only when eventual consistency is acceptable
    /// (e.g., approximate counters).
    ///
    /// # Errors
    /// Returns [`KvError`] if any of the read, mutate, or write steps fail.
    #[inline]
    pub async fn read_modify_write<T, Mutator>(
        &self,
        key: &str,
        default: T,
        mutator: Mutator,
    ) -> Result<T, KvError>
    where
        T: DeserializeOwned + Serialize,
        Mutator: FnOnce(T) -> T,
    {
        // Validation happens in get_or and put
        let current = self.get_or(key, default).await?;
        let updated = mutator(current);
        self.put(key, &updated).await?;
        Ok(updated)
    }

    fn validate_key(key: &str) -> Result<(), KvError> {
        if key.is_empty() {
            return Err(KvError::Validation("key cannot be empty".to_owned()));
        }
        if key.len() > Self::MAX_KEY_SIZE {
            return Err(KvError::Validation(format!(
                "key length {} exceeds limit of {} bytes",
                key.len(),
                Self::MAX_KEY_SIZE
            )));
        }
        if key == "." || key == ".." {
            return Err(KvError::Validation(
                "key cannot be exactly '.' or '..'".to_owned(),
            ));
        }
        if key.chars().any(char::is_control) {
            return Err(KvError::Validation(
                "key contains invalid control characters".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_list_limit(limit: usize) -> Result<(), KvError> {
        if limit == 0 {
            return Err(KvError::Validation(
                "list limit must be greater than zero".to_owned(),
            ));
        }
        if limit > Self::MAX_LIST_PAGE_SIZE {
            return Err(KvError::Validation(format!(
                "list limit {} exceeds maximum of {}",
                limit,
                Self::MAX_LIST_PAGE_SIZE
            )));
        }
        Ok(())
    }

    fn validate_prefix(prefix: &str) -> Result<(), KvError> {
        if prefix.len() > Self::MAX_KEY_SIZE {
            return Err(KvError::Validation(format!(
                "prefix length {} exceeds limit of {} bytes",
                prefix.len(),
                Self::MAX_KEY_SIZE
            )));
        }
        if prefix.chars().any(char::is_control) {
            return Err(KvError::Validation(
                "prefix contains invalid control characters".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_ttl(ttl: Duration) -> Result<(), KvError> {
        if ttl < Self::MIN_TTL {
            return Err(KvError::Validation(format!(
                "TTL {ttl:?} is less than minimum of at least 60 seconds"
            )));
        }
        if ttl > Self::MAX_TTL {
            return Err(KvError::Validation(format!(
                "TTL {ttl:?} exceeds maximum of 1 year"
            )));
        }
        Ok(())
    }

    fn validate_value(value: &[u8]) -> Result<(), KvError> {
        if value.len() > Self::MAX_VALUE_SIZE {
            return Err(KvError::Validation(format!(
                "value size {} exceeds limit of {} bytes",
                value.len(),
                Self::MAX_VALUE_SIZE
            )));
        }
        Ok(())
    }
}

impl From<KvError> for EdgeError {
    #[inline]
    fn from(err: KvError) -> Self {
        match err {
            KvError::NotFound { key } => EdgeError::not_found(format!("kv key: {key}")),
            KvError::Unavailable => EdgeError::service_unavailable("kv store unavailable"),
            KvError::Validation(msg) => {
                EdgeError::bad_request(format!("kv validation error: {msg}"))
            }
            KvError::Serialization(msg) => {
                EdgeError::internal(anyhow::anyhow!("kv serialization error: {msg}"))
            }
            KvError::Internal(source) => EdgeError::internal(source),
            KvError::Unsupported { operation } => EdgeError::not_implemented(format!(
                "kv operation not supported by backend: {operation}"
            )),
            KvError::LimitExceeded { message } => {
                EdgeError::service_unavailable(format!("kv backend limit exceeded: {message}"))
            }
        }
    }
}

/// A single page of keys from a KV listing operation.
///
/// **Termination**: callers must use `cursor.is_none()` to determine
/// completion, **not** `keys.is_empty()`. A page with `keys: vec![]` and
/// `cursor: Some(_)` is a valid intermediate result — it occurs when a
/// scan-cap path skips a long run of expired entries; calling
/// `list_keys_page` again with the returned cursor resumes the listing.
///
/// The `cursor` is opaque. Pass it back to `list_keys_page` to continue
/// listing from the next page. `None` means the current page is the last
/// page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KvPage {
    pub cursor: Option<String>,
    pub keys: Vec<String>,
}

/// Object-safe interface for KV store backends.
///
/// All methods take `&self` — backends handle concurrency internally
/// (e.g., platform APIs, or `Mutex` for in-memory stores).
///
/// # Pre-validation contract
///
/// This trait is always called through [`KvHandle`], which validates all
/// inputs (key length/format, value size, TTL bounds, list limits) before
/// delegating here. Implementations may therefore assume that:
/// - Keys are non-empty and within [`KvHandle::MAX_KEY_SIZE`]
/// - Values are within [`KvHandle::MAX_VALUE_SIZE`]
/// - TTLs are within `[MIN_TTL, MAX_TTL]`
/// - List limits are within `[1, MAX_LIST_PAGE_SIZE]`
///
/// Do **not** call trait methods directly in production code; always go
/// through [`KvHandle`] to ensure validation is applied.
///
/// Implementations exist per adapter:
/// - `PersistentKvStore` (axum adapter) — local dev / tests with persistent storage
/// - `FastlyKvStore` (fastly adapter) — Fastly KV Store
/// - `CloudflareKvStore` (cloudflare adapter) — Cloudflare Workers KV
#[async_trait(?Send)]
pub trait KvStore: Send + Sync {
    /// Delete a key. Returns `Ok(())` even if the key did not exist.
    async fn delete(&self, key: &str) -> Result<(), KvError>;

    /// Check whether a key exists.
    ///
    /// The default implementation delegates to `get_bytes`. Backends that
    /// support a cheaper existence check should override this.
    #[inline]
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.get_bytes(key).await?.is_some())
    }

    /// Retrieve raw bytes for a key. Returns `Ok(None)` if the key does not exist.
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError>;

    /// List keys in lexicographic order, returning at most `limit` keys.
    ///
    /// The `cursor` is opaque. Pass the cursor from a previous page back to
    /// continue listing. Implementations should keep memory usage bounded to a
    /// single page worth of keys.
    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError>;

    /// Store raw bytes for a key, overwriting any existing value.
    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError>;

    /// Store raw bytes with a time-to-live. After `ttl` has elapsed the key
    /// should be treated as expired. Eviction timing is backend-specific:
    /// - **Axum (`PersistentKvStore`)**: lazy eviction — expired keys are removed
    ///   on the next `get_bytes` call for that key. Keys never accessed after
    ///   expiration remain in the database until deleted, so `.edgezero/kv.redb`
    ///   grows without bound on long-running dev servers.
    /// - **Fastly/Cloudflare**: eviction is managed by the platform and is not
    ///   guaranteed to be immediate.
    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError>;
}

// ---------------------------------------------------------------------------
// Test-only no-op store
// ---------------------------------------------------------------------------

/// A no-op [`KvStore`] for tests that only need a [`KvHandle`] to exist
/// without storing real data.
///
/// All reads return `None` / empty; all writes succeed silently.
///
/// Available in `#[cfg(test)]` builds within this crate, and in any downstream
/// crate that enables the `test-utils` feature on `edgezero-core`:
///
/// ```toml
/// [dev-dependencies]
/// edgezero-core = { path = "...", features = ["test-utils"] }
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct NoopKvStore;

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl KvStore for NoopKvStore {
    #[inline]
    async fn delete(&self, _key: &str) -> Result<(), KvError> {
        Ok(())
    }
    #[inline]
    async fn exists(&self, _key: &str) -> Result<bool, KvError> {
        Ok(false)
    }
    #[inline]
    async fn get_bytes(&self, _key: &str) -> Result<Option<Bytes>, KvError> {
        Ok(None)
    }
    #[inline]
    async fn list_keys_page(
        &self,
        _prefix: &str,
        _cursor: Option<&str>,
        _limit: usize,
    ) -> Result<KvPage, KvError> {
        Ok(KvPage::default())
    }
    #[inline]
    async fn put_bytes(&self, _key: &str, _value: Bytes) -> Result<(), KvError> {
        Ok(())
    }
    #[inline]
    async fn put_bytes_with_ttl(
        &self,
        _key: &str,
        _value: Bytes,
        _ttl: Duration,
    ) -> Result<(), KvError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Run the shared contract tests against MockStore.
    crate::key_value_store_contract_tests!(mock_store_contract, MockStore::new());

    use super::*;
    use crate::http::StatusCode;
    use futures::executor::block_on;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::SystemTime;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Counter {
        count: i32,
    }

    // In-memory store with TTL support for contract testing.
    // Uses `SystemTime` instead of `Instant` for WASM compatibility.
    struct MockStore {
        data: Mutex<HashMap<String, (Bytes, Option<SystemTime>)>>,
    }

    #[async_trait(?Send)]
    impl KvStore for MockStore {
        async fn delete(&self, key: &str) -> Result<(), KvError> {
            let mut data = self.data.lock().unwrap();
            data.remove(key);
            Ok(())
        }

        async fn exists(&self, key: &str) -> Result<bool, KvError> {
            Ok(self.get_bytes(key).await?.is_some())
        }

        async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
            let mut data = self.data.lock().unwrap();
            if let Some((_, Some(exp))) = data.get(key)
                && SystemTime::now() >= *exp
            {
                data.remove(key);
                return Ok(None);
            }
            Ok(data.get(key).map(|(value, _)| value.clone()))
        }

        async fn list_keys_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: usize,
        ) -> Result<KvPage, KvError> {
            let mut data = self.data.lock().unwrap();
            let now = SystemTime::now();
            data.retain(|_, (_, expires_at)| expires_at.is_none_or(|exp| now < exp));

            let mut keys = data
                .keys()
                .filter(|key| {
                    key.starts_with(prefix) && cursor.is_none_or(|cur| key.as_str() > cur)
                })
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();

            let has_more = keys.len() > limit;
            keys.truncate(limit);

            Ok(KvPage {
                cursor: has_more.then(|| keys.last().cloned()).flatten(),
                keys,
            })
        }

        async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
            let mut data = self.data.lock().unwrap();
            data.insert(key.to_owned(), (value, None));
            Ok(())
        }

        async fn put_bytes_with_ttl(
            &self,
            key: &str,
            value: Bytes,
            ttl: Duration,
        ) -> Result<(), KvError> {
            let mut data = self.data.lock().unwrap();
            let expires_at = SystemTime::now()
                .checked_add(ttl)
                .ok_or_else(|| KvError::Internal(anyhow::anyhow!("ttl overflows system time")))?;
            data.insert(key.to_owned(), (value, Some(expires_at)));
            Ok(())
        }
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    fn handle() -> KvHandle {
        KvHandle::new(Arc::new(MockStore::new()))
    }

    #[test]
    fn delete_missing_key_is_ok() {
        let kv = handle();
        block_on(async {
            kv.delete("nope").await.unwrap();
        });
    }

    #[test]
    fn delete_removes_key() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("k", Bytes::from("v")).await.unwrap();
            kv.delete("k").await.unwrap();
            assert_eq!(kv.get_bytes("k").await.unwrap(), None);
        });
    }

    #[test]
    fn empty_key_rejected() {
        let kv = handle();
        block_on(async {
            let err = kv.put("", &"empty key").await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("cannot be empty"));
        });
    }

    #[test]
    fn error_metadata_omits_unknown_result_fields() {
        let read_result = Err(KvError::Unavailable);
        assert_eq!(KvHandle::kv_read_metadata(18, &read_result), "key_len=18");

        let exists_result = Err(KvError::Unavailable);
        assert_eq!(
            KvHandle::kv_exists_metadata(18, &exists_result),
            "key_len=18"
        );

        let list_result = Err(KvError::Unavailable);
        assert_eq!(
            KvHandle::kv_list_metadata(4, true, 100, &list_result),
            "prefix_len=4 cursor_present=true limit=100"
        );
    }

    #[test]
    fn exists_returns_false_after_delete() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("ephemeral", Bytes::from("v")).await.unwrap();
            assert!(kv.exists("ephemeral").await.unwrap());
            kv.delete("ephemeral").await.unwrap();
            assert!(!kv.exists("ephemeral").await.unwrap());
        });
    }

    #[test]
    fn exists_returns_false_for_missing() {
        let kv = handle();
        block_on(async {
            assert!(!kv.exists("nope").await.unwrap());
        });
    }

    #[test]
    fn exists_returns_true_for_present() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("k", Bytes::from("v")).await.unwrap();
            assert!(kv.exists("k").await.unwrap());
        });
    }

    #[test]
    fn get_or_with_complex_default() {
        let kv = handle();
        block_on(async {
            let default = Counter { count: 100_i32 };
            let val: Counter = kv.get_or("missing_struct", default).await.unwrap();
            assert_eq!(val.count, 100_i32);
        });
    }

    #[test]
    fn handle_is_cloneable_and_shares_state() {
        let h1 = handle();
        let h2 = h1.clone();
        block_on(async {
            h1.put("shared", &42_i32).await.unwrap();
            let val: i32 = h2.get_or("shared", 0_i32).await.unwrap();
            assert_eq!(val, 42_i32);
        });
    }

    #[test]
    fn kv_error_internal_converts_to_internal() {
        let kv_err = KvError::Internal(anyhow::anyhow!("boom"));
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(edge_err.message().contains("boom"));
    }

    #[test]
    fn kv_error_not_found_converts_to_not_found() {
        let kv_err = KvError::NotFound { key: "test".into() };
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::NOT_FOUND);
        assert!(edge_err.message().contains("kv key"));
    }

    #[test]
    fn kv_error_serialization_converts_to_internal() {
        let json_err: serde_json::Error = serde_json::from_str::<i32>("not json").unwrap_err();
        let kv_err = KvError::Serialization(json_err);
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(edge_err.message().contains("serialization"));
    }

    #[test]
    fn kv_error_unavailable_converts_to_service_unavailable() {
        let kv_err = KvError::Unavailable;
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn kv_error_unsupported_converts_to_not_implemented() {
        let kv_err = KvError::Unsupported {
            operation: "put_bytes_with_ttl".to_owned(),
        };
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(edge_err.message().contains("put_bytes_with_ttl"));
    }

    #[test]
    fn kv_error_limit_exceeded_converts_to_service_unavailable() {
        let kv_err = KvError::LimitExceeded {
            message: "max_list_keys=1000 exceeded".to_owned(),
        };
        let edge_err: EdgeError = kv_err.into();
        assert_eq!(edge_err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(edge_err.message().contains("max_list_keys"));
    }

    #[test]
    fn kv_handle_debug_output() {
        let kv = handle();
        let debug = format!("{kv:?}");
        assert!(debug.contains("KvHandle"));
    }

    #[test]
    fn large_value_roundtrip() {
        let kv = handle();
        block_on(async {
            let large = "x".repeat(1_000_000); // 1MB string
            kv.put("big", &large).await.unwrap();
            let val: Option<String> = kv.get("big").await.unwrap();
            assert_eq!(val.as_deref(), Some(large.as_str()));
        });
    }

    #[test]
    fn list_keys_page_roundtrip() {
        let kv = handle();
        block_on(async {
            kv.put("app/a", &1_i32).await.unwrap();
            kv.put("app/b", &2_i32).await.unwrap();
            kv.put("app/c", &3_i32).await.unwrap();
            kv.put("other/d", &4_i32).await.unwrap();

            let first = kv.list_keys_page("app/", None, 2).await.unwrap();
            assert_eq!(first.keys, vec!["app/a".to_owned(), "app/b".to_owned()]);
            assert!(first.cursor.is_some());
            assert_ne!(first.cursor.as_deref(), Some("app/b"));

            let second = kv
                .list_keys_page("app/", first.cursor.as_deref(), 2)
                .await
                .unwrap();
            assert_eq!(second.keys, vec!["app/c".to_owned()]);
            assert_eq!(second.cursor, None);
        });
    }

    #[test]
    fn put_overwrite_changes_type() {
        let kv = handle();
        block_on(async {
            kv.put("flex", &42_i32).await.unwrap();
            let int_val: i32 = kv.get_or("flex", 0_i32).await.unwrap();
            assert_eq!(int_val, 42_i32);

            // Overwrite with a different type
            kv.put("flex", &"now a string").await.unwrap();
            let str_val: String = kv.get_or("flex", String::new()).await.unwrap();
            assert_eq!(str_val, "now a string");
        });
    }

    #[test]
    fn put_with_ttl_stores_value() {
        let kv = handle();
        block_on(async {
            kv.put_with_ttl("session", &"token123", Duration::from_mins(1))
                .await
                .unwrap();
            let val: Option<String> = kv.get("session").await.unwrap();
            assert_eq!(val, Some("token123".to_owned()));
        });
    }

    #[test]
    fn put_with_ttl_typed_helper() {
        let kv = handle();
        block_on(async {
            let data = Counter { count: 7_i32 };
            kv.put_with_ttl("ttl_key", &data, Duration::from_mins(10))
                .await
                .unwrap();
            let val: Option<Counter> = kv.get("ttl_key").await.unwrap();
            assert_eq!(val, Some(Counter { count: 7_i32 }));
        });
    }

    #[test]
    fn raw_bytes_missing_key_returns_none() {
        let kv = handle();
        block_on(async {
            assert_eq!(kv.get_bytes("missing").await.unwrap(), None);
        });
    }

    #[test]
    fn raw_bytes_overwrite() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("k", Bytes::from("a")).await.unwrap();
            kv.put_bytes("k", Bytes::from("b")).await.unwrap();
            assert_eq!(kv.get_bytes("k").await.unwrap(), Some(Bytes::from("b")));
        });
    }

    #[test]
    fn raw_bytes_roundtrip() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("k", Bytes::from("hello")).await.unwrap();
            assert_eq!(kv.get_bytes("k").await.unwrap(), Some(Bytes::from("hello")));
        });
    }

    #[test]
    fn read_metadata_logs_lengths_not_raw_key_or_value() {
        let key = "super-secret-token";
        let value = Bytes::from_static(b"super-secret-value");
        let result = Ok(Some(value));

        let metadata = KvHandle::kv_read_metadata(key.len(), &result);

        assert_eq!(metadata, "key_len=18 hit=true bytes=18");
        assert!(!metadata.contains(key));
        assert!(!metadata.contains("super-secret-value"));
    }

    #[test]
    fn success_metadata_keeps_stable_field_types() {
        let read_result = Ok(Some(Bytes::from_static(b"abc")));
        assert_eq!(
            KvHandle::kv_read_metadata(1, &read_result),
            "key_len=1 hit=true bytes=3"
        );

        let exists_result = Ok(false);
        assert_eq!(
            KvHandle::kv_exists_metadata(1, &exists_result),
            "key_len=1 exists=false"
        );

        let list_result = Ok(KvPage {
            cursor: Some("cursor".to_owned()),
            keys: vec!["a".to_owned(), "b".to_owned()],
        });
        assert_eq!(
            KvHandle::kv_list_metadata(4, false, 100, &list_result),
            "prefix_len=4 cursor_present=false limit=100 count=2 next_cursor_present=true"
        );
    }

    #[test]
    fn typed_get_bad_json_returns_serialization_error() {
        let kv = handle();
        block_on(async {
            kv.put_bytes("bad", Bytes::from("not json")).await.unwrap();
            let err = kv.get::<Counter>("bad").await.unwrap_err();
            assert!(matches!(err, KvError::Serialization(_)));
        });
    }

    #[test]
    fn typed_get_missing_returns_none() {
        let kv = handle();
        block_on(async {
            let out: Option<Counter> = kv.get("nope").await.unwrap();
            assert_eq!(out, None);
        });
    }

    #[test]
    fn typed_get_or_returns_default() {
        let kv = handle();
        block_on(async {
            let count: i32 = kv.get_or("visits", 0_i32).await.unwrap();
            assert_eq!(count, 0_i32);
        });
    }

    #[test]
    fn typed_get_or_returns_existing() {
        let kv = handle();
        block_on(async {
            kv.put("visits", &99_i32).await.unwrap();
            let count: i32 = kv.get_or("visits", 0_i32).await.unwrap();
            assert_eq!(count, 99_i32);
        });
    }

    #[test]
    fn typed_get_put_roundtrip() {
        let kv = handle();
        block_on(async {
            let data = Counter { count: 42 };
            kv.put("counter", &data).await.unwrap();
            let out: Option<Counter> = kv.get("counter").await.unwrap();
            assert_eq!(out, Some(data));
        });
    }

    #[test]
    fn unicode_key_roundtrip() {
        // "日本語キー" — the literal is written as Unicode escapes so the source
        // file stays ASCII-only. The runtime bytes are identical.
        const JAPANESE_KEY: &str = "\u{65E5}\u{672C}\u{8A9E}\u{30AD}\u{30FC}";
        let kv = handle();
        block_on(async {
            kv.put(JAPANESE_KEY, &"value").await.unwrap();
            let val: Option<String> = kv.get(JAPANESE_KEY).await.unwrap();
            assert_eq!(val, Some("value".to_owned()));
        });
    }

    #[test]
    fn update_increments_counter() {
        let kv = handle();
        block_on(async {
            kv.put("c", &0_i32).await.unwrap();
            let after_first = kv
                .read_modify_write("c", 0_i32, |num| num + 1_i32)
                .await
                .unwrap();
            assert_eq!(after_first, 1_i32);
            let after_second = kv
                .read_modify_write("c", 0_i32, |num| num + 1_i32)
                .await
                .unwrap();
            assert_eq!(after_second, 2_i32);
        });
    }

    #[test]
    fn update_uses_default_when_missing() {
        let kv = handle();
        block_on(async {
            let val = kv
                .read_modify_write("new", 10_i32, |num| num * 2_i32)
                .await
                .unwrap();
            assert_eq!(val, 20_i32);
        });
    }

    #[test]
    fn update_with_struct() {
        let kv = handle();
        block_on(async {
            let after_first = kv
                .read_modify_write("counter_struct", Counter { count: 0_i32 }, |mut counter| {
                    counter.count += 10_i32;
                    counter
                })
                .await
                .unwrap();
            assert_eq!(after_first.count, 10_i32);

            let after_second = kv
                .read_modify_write("counter_struct", Counter { count: 0_i32 }, |mut counter| {
                    counter.count += 5_i32;
                    counter
                })
                .await
                .unwrap();
            assert_eq!(after_second.count, 15_i32);
        });
    }

    #[test]
    fn validation_rejects_control_chars() {
        let kv = handle();
        block_on(async {
            let err = kv.get::<i32>("key\nwith\nnewline").await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("control characters"));
        });
    }

    #[test]
    fn validation_rejects_control_chars_in_prefix() {
        let kv = handle();
        block_on(async {
            let err = kv.list_keys_page("bad\nprefix", None, 1).await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("control characters"));
        });
    }

    #[test]
    fn validation_rejects_cursor_for_different_prefix() {
        let kv = handle();
        block_on(async {
            kv.put("app/a", &1_i32).await.unwrap();
            kv.put("app/b", &2_i32).await.unwrap();

            let page = kv.list_keys_page("app/", None, 1).await.unwrap();
            let err = kv
                .list_keys_page("other/", page.cursor.as_deref(), 1)
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("requested prefix"));
        });
    }

    #[test]
    fn validation_rejects_dot_keys() {
        let kv = handle();
        block_on(async {
            let single_dot_err = kv.get::<i32>(".").await.unwrap_err();
            assert!(matches!(single_dot_err, KvError::Validation(_)));
            assert!(format!("{single_dot_err}").contains("cannot be exactly"));

            let double_dot_err = kv.get::<i32>("..").await.unwrap_err();
            assert!(matches!(double_dot_err, KvError::Validation(_)));
            assert!(format!("{double_dot_err}").contains("cannot be exactly"));
        });
    }

    #[test]
    fn validation_rejects_large_list_limit() {
        let kv = handle();
        block_on(async {
            let err = kv
                .list_keys_page("", None, KvHandle::MAX_LIST_PAGE_SIZE + 1)
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("list limit"));
        });
    }

    #[test]
    fn validation_rejects_large_values() {
        let kv = handle();
        block_on(async {
            let large_val = vec![0_u8; KvHandle::MAX_VALUE_SIZE + 1];
            let err = kv
                .put_bytes("large", Bytes::from(large_val))
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("value size"));
        });
    }

    #[test]
    fn validation_rejects_long_keys() {
        let kv = handle();
        block_on(async {
            let long_key = "a".repeat(KvHandle::MAX_KEY_SIZE + 1);
            let err = kv.get::<i32>(&long_key).await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("key length"));
        });
    }

    #[test]
    fn validation_rejects_long_prefix() {
        let kv = handle();
        block_on(async {
            let prefix = "a".repeat(KvHandle::MAX_KEY_SIZE + 1);
            let err = kv.list_keys_page(&prefix, None, 1).await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("prefix length"));
        });
    }

    #[test]
    fn validation_rejects_long_ttl() {
        let kv = handle();
        block_on(async {
            let err = kv
                .put_with_ttl("long", &"val", KvHandle::MAX_TTL + Duration::from_secs(1))
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("exceeds maximum"));
        });
    }

    #[test]
    fn validation_rejects_malformed_list_cursor() {
        let kv = handle();
        block_on(async {
            let err = kv
                .list_keys_page("app/", Some("not-json"), 1)
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("cursor"));
        });
    }

    #[test]
    fn validation_rejects_short_ttl() {
        let kv = handle();
        block_on(async {
            let err = kv
                .put_with_ttl("short", &"val", Duration::from_secs(10))
                .await
                .unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("at least 60 seconds"));
        });
    }

    #[test]
    fn validation_rejects_zero_list_limit() {
        let kv = handle();
        block_on(async {
            let err = kv.list_keys_page("", None, 0).await.unwrap_err();
            assert!(matches!(err, KvError::Validation(_)));
            assert!(format!("{err}").contains("greater than zero"));
        });
    }
}
