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
use spin_sdk::key_value::Store as SpinSdkStore;
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
    max_list_keys: usize,
    store: SpinSdkStore,
}

impl SpinKvStore {
    /// Open a Spin KV store by label, using the default `max_list_keys` cap.
    ///
    /// The `label` must match a `key_value_stores` entry in `spin.toml`.
    ///
    /// # Errors
    /// Returns [`KvError::Internal`] if the underlying Spin KV store cannot
    /// be opened (typically when `label` is not declared in `spin.toml`).
    #[inline]
    pub async fn open(label: &str) -> Result<Self, KvError> {
        Self::open_with_max_list_keys(label, DEFAULT_MAX_LIST_KEYS).await
    }

    /// Open a Spin KV store by label with a custom `max_list_keys` cap.
    /// Pass `0` to disable the cap (not recommended in production).
    ///
    /// # Errors
    /// Returns [`KvError::Internal`] if the underlying Spin KV store cannot
    /// be opened (typically when `label` is not declared in `spin.toml`).
    #[inline]
    pub async fn open_with_max_list_keys(
        label: &str,
        max_list_keys: usize,
    ) -> Result<Self, KvError> {
        let store = SpinSdkStore::open(label)
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to open kv store: {err}")))?;
        Ok(Self {
            max_list_keys,
            store,
        })
    }
}

#[async_trait(?Send)]
impl KvStore for SpinKvStore {
    #[inline]
    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("delete failed: {err}")))
    }

    #[inline]
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        self.store
            .exists(key)
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("exists failed: {err}")))
    }

    #[inline]
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        self.store
            .get(key)
            .await
            .map(|opt| opt.map(Bytes::from))
            .map_err(|err| KvError::Internal(anyhow::anyhow!("get failed: {err}")))
    }

    #[inline]
    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        let all_keys = self
            .store
            .get_keys()
            .await
            .collect()
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("get_keys failed: {err}")))?;
        paginate_keys(all_keys, prefix, cursor, limit, self.max_list_keys)
    }

    #[inline]
    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .set(key, value.as_ref())
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("set failed: {err}")))
    }

    #[inline]
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
}

// TODO: integration tests require the Spin runtime.
// Test `SpinKvStore` as part of a Spin E2E test suite.
