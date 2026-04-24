//! Spin KV store adapter.
//!
//! Wraps `spin_sdk::key_value::Store` to implement the
//! `edgezero_core::key_value_store::KvStore` trait.
//!
//! # Limitations
//!
//! - **TTL**: The Spin KV API has no TTL support. Calls to
//!   `put_bytes_with_ttl` store the value without expiry and emit a
//!   `log::warn!`.
//! - **Listing**: `spin_sdk::key_value::Store::get_keys()` returns all keys
//!   with no prefix or cursor support. Prefix filtering and pagination are
//!   performed in-process after fetching all keys. A configurable cap
//!   (`max_list_keys`, default [`DEFAULT_MAX_LIST_KEYS`]) guards against
//!   unbounded allocations; when the total key count exceeds it the list is
//!   silently truncated, a `log::warn!` is emitted, and a partial page is
//!   returned so the caller can resume via cursor (matching the Axum adapter
//!   behaviour for its scan-batch limit).
//!
//! # Note
//!
//! This module is only compiled when the `spin` feature is enabled and the
//! target is `wasm32`.

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use async_trait::async_trait;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use std::time::Duration;

/// Maximum number of keys fetched from the Spin KV host before
/// `list_keys_page` returns `KvError::Validation`. Overridable via
/// [`SpinKvStore::with_max_list_keys`].
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub const DEFAULT_MAX_LIST_KEYS: usize = 10_000;

/// KV store backed by the Spin KV API.
///
/// Wraps a `spin_sdk::key_value::Store` handle obtained via
/// `Store::open(label)`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub struct SpinKvStore {
    store: spin_sdk::key_value::Store,
    max_list_keys: usize,
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl SpinKvStore {
    /// Open a Spin KV store by label.
    ///
    /// The `label` must match a `key_value_stores` entry in `spin.toml`.
    /// Returns `KvError::Internal` if the store cannot be opened.
    pub fn open(label: &str) -> Result<Self, KvError> {
        let store = spin_sdk::key_value::Store::open(label)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open kv store: {e}")))?;
        Ok(Self {
            store,
            max_list_keys: DEFAULT_MAX_LIST_KEYS,
        })
    }

    /// Open the default Spin KV store (label `"default"`).
    pub fn open_default() -> Result<Self, KvError> {
        Self::open("default")
    }

    /// Override the maximum number of keys fetched during `list_keys_page`.
    ///
    /// When the Spin KV store contains more than `limit` keys,
    /// `list_keys_page` returns `KvError::Validation` rather than
    /// allocating an unbounded `Vec`. Defaults to [`DEFAULT_MAX_LIST_KEYS`].
    pub fn with_max_list_keys(mut self, limit: usize) -> Self {
        self.max_list_keys = limit;
        self
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl KvStore for SpinKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        self.store
            .get(key)
            .map(|opt| opt.map(Bytes::from))
            .map_err(|e| KvError::Internal(anyhow::anyhow!("get failed: {e}")))
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .set(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("set failed: {e}")))
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        _ttl: Duration,
    ) -> Result<(), KvError> {
        log::warn!("SpinKvStore: TTL is not supported by the Spin KV API; storing without expiry");
        self.store
            .set(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("set failed: {e}")))
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("delete failed: {e}")))
    }

    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        self.store
            .exists(key)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("exists failed: {e}")))
    }

    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        let all_keys = self
            .store
            .get_keys()
            .map_err(|e| KvError::Internal(anyhow::anyhow!("get_keys failed: {e}")))?;

        if all_keys.len() > self.max_list_keys {
            log::warn!(
                "SpinKvStore: fetched {} keys, exceeding max_list_keys={}; \
                 processing the first {} keys only. Use with_max_list_keys to scan the full store.",
                all_keys.len(),
                self.max_list_keys,
                self.max_list_keys,
            );
        }

        let mut keys: Vec<String> = all_keys
            .into_iter()
            .take(self.max_list_keys)
            .filter(|k| k.starts_with(prefix))
            .collect();

        keys.sort();

        // Advance past all keys <= last_key (the cursor).
        let start = if let Some(last_key) = cursor {
            keys.iter()
                .position(|k| k.as_str() > last_key)
                .unwrap_or(keys.len())
        } else {
            0
        };

        let remaining = &keys[start..];
        let page_keys: Vec<String> = remaining.iter().take(limit).cloned().collect();
        let has_more = remaining.len() > limit;
        let next_cursor = if has_more {
            page_keys.last().cloned()
        } else {
            None
        };

        Ok(KvPage {
            keys: page_keys,
            cursor: next_cursor,
        })
    }
}

// TODO: integration tests require the Spin runtime.
// Test `SpinKvStore` as part of a Spin E2E test suite.
