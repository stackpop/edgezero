//! Spin KV store adapter.
//!
//! Wraps `spin_sdk::key_value::Store` to implement the
//! `edgezero_core::key_value_store::KvStore` trait.
//!
//! # Limitations
//!
//! - **TTL**: The Spin KV API has no TTL support. Calls to
//!   `put_bytes_with_ttl` return [`KvError::Unsupported`] without writing.
//! - **Listing**: `spin_sdk::key_value::Store::get_keys()` returns all keys
//!   with no prefix, cursor, or limit support. `list_keys_page` materialises
//!   the full key list, filters by prefix, sorts, and pages client-side via
//!   [`crate::kv_pagination::paginate_keys`]. A `max_list_keys` cap guards
//!   against runaway materialisation; exceeding it yields
//!   [`KvError::LimitExceeded`].
//!
//! # Note
//!
//! This module is only compiled when the `spin` feature is enabled and the
//! target is `wasm32`.

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
use std::time::Duration;

use crate::kv_pagination::paginate_keys;

/// Default `max_list_keys` cap. Matches the Cloudflare KV page size ceiling
/// (`KvHandle::MAX_LIST_PAGE_SIZE`) and stays well below the soft per-isolate
/// memory budgets a Spin component is given. Overridable via
/// `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS`.
pub const DEFAULT_MAX_LIST_KEYS: usize = 1_000;

/// KV store backed by the Spin KV API.
///
/// Wraps a `spin_sdk::key_value::Store` handle obtained via
/// `Store::open(label)` plus a `max_list_keys` paging cap.
pub struct SpinKvStore {
    store: spin_sdk::key_value::Store,
    max_list_keys: usize,
}

impl SpinKvStore {
    /// Open a Spin KV store by label, using the default `max_list_keys` cap.
    ///
    /// The `label` must match a `key_value_stores` entry in `spin.toml`.
    /// Returns `KvError::Internal` if the store cannot be opened.
    pub fn open(label: &str) -> Result<Self, KvError> {
        Self::open_with_max_list_keys(label, DEFAULT_MAX_LIST_KEYS)
    }

    /// Open a Spin KV store by label with a custom `max_list_keys` cap.
    /// Pass `0` to disable the cap (not recommended in production).
    pub fn open_with_max_list_keys(label: &str, max_list_keys: usize) -> Result<Self, KvError> {
        let store = spin_sdk::key_value::Store::open(label)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open kv store: {e}")))?;
        Ok(Self {
            store,
            max_list_keys,
        })
    }

    /// Open the default EdgeZero KV store label (`"EDGEZERO_KV"`).
    pub fn open_default() -> Result<Self, KvError> {
        Self::open(edgezero_core::manifest::DEFAULT_KV_STORE_NAME)
    }
}

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
        _key: &str,
        _value: Bytes,
        _ttl: Duration,
    ) -> Result<(), KvError> {
        Err(KvError::Unsupported {
            operation: "put_bytes_with_ttl".to_owned(),
        })
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
        paginate_keys(all_keys, prefix, cursor, limit, self.max_list_keys)
    }
}

// TODO: integration tests require the Spin runtime.
// Test `SpinKvStore` as part of a Spin E2E test suite.
