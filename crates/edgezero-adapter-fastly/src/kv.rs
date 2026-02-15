//! Fastly KV Store adapter.
//!
//! Wraps `fastly::kv_store::KVStore` to implement the `edgezero_core::kv::KvStore` trait.
//!
//! # Note
//!
//! This module is only compiled when the `fastly` feature is enabled.

#[cfg(feature = "fastly")]
use async_trait::async_trait;
#[cfg(feature = "fastly")]
use bytes::Bytes;
#[cfg(feature = "fastly")]
use edgezero_core::kv::{KvError, KvStore};
#[cfg(feature = "fastly")]
use std::time::Duration;

/// KV store backed by Fastly's KV Store API.
///
/// Wraps a `fastly::kv_store::KVStore` handle obtained via `KVStore::open(name)`.
#[cfg(feature = "fastly")]
pub struct FastlyKvStore {
    store: fastly::kv_store::KVStore,
}

#[cfg(feature = "fastly")]
impl FastlyKvStore {
    /// Open a Fastly KV Store by name.
    ///
    /// Returns `KvError::Unavailable` if the store does not exist.
    pub fn open(name: &str) -> Result<Self, KvError> {
        let store = fastly::kv_store::KVStore::open(name)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open kv store: {e}")))?
            .ok_or(KvError::Unavailable)?;
        Ok(Self { store })
    }
}

#[cfg(feature = "fastly")]
#[async_trait(?Send)]
impl KvStore for FastlyKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        match self.store.lookup(key) {
            Ok(mut response) => {
                let bytes = response.take_body_bytes();
                Ok(Some(Bytes::from(bytes)))
            }
            Err(fastly::kv_store::KVStoreError::ItemNotFound) => Ok(None),
            Err(e) => Err(KvError::Internal(anyhow::anyhow!("lookup failed: {e}"))),
        }
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .insert(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("insert failed: {e}")))
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        self.store
            .build_insert()
            .time_to_live(ttl)
            .execute(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("insert with ttl failed: {e}")))
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("delete failed: {e}")))
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
        let mut keys = Vec::new();

        // Use the ListBuilder's iterator for automatic pagination.
        for page_result in self.store.build_list().prefix(prefix).iter() {
            let page =
                page_result.map_err(|e| KvError::Internal(anyhow::anyhow!("list failed: {e}")))?;
            keys.extend(page.into_keys());
        }

        Ok(keys)
    }
}

// TODO: integration tests require the Fastly compute environment.
// Test `FastlyKvStore` as part of the Fastly adapter E2E test suite.
