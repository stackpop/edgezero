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
//!   with no prefix or cursor support. `list_keys_page` currently returns
//!   [`KvError::LimitExceeded`]; paged listing with a `max_list_keys` cap
//!   lands when the Spin store registry is wired (Task 2.6 follow-on).
//!
//! # Note
//!
//! This module is only compiled when the `spin` feature is enabled and the
//! target is `wasm32`.

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
use std::time::Duration;

/// KV store backed by the Spin KV API.
///
/// Wraps a `spin_sdk::key_value::Store` handle obtained via
/// `Store::open(label)`.
pub struct SpinKvStore {
    store: spin_sdk::key_value::Store,
}

impl SpinKvStore {
    /// Open a Spin KV store by label.
    ///
    /// The `label` must match a `key_value_stores` entry in `spin.toml`.
    /// Returns `KvError::Internal` if the store cannot be opened.
    pub fn open(label: &str) -> Result<Self, KvError> {
        let store = spin_sdk::key_value::Store::open(label)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open kv store: {e}")))?;
        Ok(Self { store })
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
        _prefix: &str,
        _cursor: Option<&str>,
        _limit: usize,
    ) -> Result<KvPage, KvError> {
        Err(KvError::LimitExceeded {
            message: "Spin KV key listing is unbounded; max_list_keys cap is not yet wired"
                .to_owned(),
        })
    }
}

// TODO: integration tests require the Spin runtime.
// Test `SpinKvStore` as part of a Spin E2E test suite.
