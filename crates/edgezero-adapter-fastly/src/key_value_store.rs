//! Fastly KV Store adapter.
//!
//! Wraps `fastly::kv_store::KVStore` to implement the `edgezero_core::key_value_store::KvStore` trait.
//!
//! # Note
//!
//! This module is only compiled when the `fastly` feature is enabled.

#[cfg(feature = "fastly")]
use async_trait::async_trait;
#[cfg(feature = "fastly")]
use bytes::Bytes;
#[cfg(feature = "fastly")]
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
#[cfg(feature = "fastly")]
use fastly::kv_store::{KVStore, KVStoreError};
#[cfg(feature = "fastly")]
use std::time::Duration;

/// KV store backed by Fastly's KV Store API.
///
/// Wraps a `fastly::kv_store::KVStore` handle obtained via `KVStore::open(name)`.
#[cfg(feature = "fastly")]
pub struct FastlyKvStore {
    store: KVStore,
}

#[cfg(feature = "fastly")]
impl FastlyKvStore {
    /// Open a Fastly KV Store by name.
    ///
    /// Returns `KvError::Unavailable` if the store does not exist.
    ///
    /// # Errors
    /// Returns [`KvError::Internal`] if the named KV store cannot be opened.
    pub fn open(name: &str) -> Result<Self, KvError> {
        let store = KVStore::open(name)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("failed to open kv store: {err}")))?
            .ok_or(KvError::Unavailable)?;
        Ok(Self { store })
    }
}

#[cfg(feature = "fastly")]
#[async_trait(?Send)]
impl KvStore for FastlyKvStore {
    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .map_err(|err| KvError::Internal(anyhow::anyhow!("delete failed: {err}")))
    }

    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.get_bytes(key).await?.is_some())
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        match self.store.lookup(key) {
            Ok(mut response) => {
                let bytes = response.take_body_bytes();
                Ok(Some(Bytes::from(bytes)))
            }
            Err(KVStoreError::ItemNotFound) => Ok(None),
            Err(err) => Err(KvError::Internal(anyhow::anyhow!("lookup failed: {err}"))),
        }
    }

    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        let limit_u32 = u32::try_from(limit)
            .map_err(|_e| KvError::Validation("list limit exceeds u32".to_owned()))?;

        let mut request = self.store.build_list().limit(limit_u32);

        if !prefix.is_empty() {
            request = request.prefix(prefix);
        }
        if let Some(token) = cursor.filter(|token| !token.is_empty()) {
            request = request.cursor(token);
        }

        let page = request
            .execute()
            .map_err(|err| KvError::Internal(anyhow::anyhow!("list failed: {err}")))?;
        let next_cursor = page.next_cursor().filter(|token| !token.is_empty());

        Ok(KvPage {
            cursor: next_cursor,
            keys: page.into_keys(),
        })
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .insert(key, value.as_ref())
            .map_err(|err| KvError::Internal(anyhow::anyhow!("insert failed: {err}")))
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
            .map_err(|err| KvError::Internal(anyhow::anyhow!("insert with ttl failed: {err}")))
    }
}

// TODO: integration tests require the Fastly compute environment.
// Test `FastlyKvStore` as part of the Fastly adapter E2E test suite.
