//! Cloudflare Workers KV adapter.
//!
//! Wraps `worker::kv::KvStore` to implement the `edgezero_core::key_value_store::KvStore` trait.
//!
//! # Note
//!
//! This module is only compiled when the `cloudflare` feature is enabled
//! and the target is `wasm32`.

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use async_trait::async_trait;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::key_value_store::{KvError, KvPage, KvStore};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use std::time::Duration;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::kv::KvStore as WorkerKvStore;

/// KV store backed by Cloudflare Workers KV.
///
/// Wraps a `worker::kv::KvStore` handle obtained via the environment binding.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub struct CloudflareKvStore {
    store: WorkerKvStore,
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl CloudflareKvStore {
    /// Create a new Cloudflare KV store from the environment binding name.
    ///
    /// The `binding` must match a KV namespace binding in `wrangler.toml`.
    /// Uses `env.kv(binding)` which is the idiomatic `worker` 0.7+ API.
    ///
    /// # Errors
    /// Returns [`KvError::Internal`] if the named binding is missing from the
    /// Worker environment or otherwise cannot be opened.
    #[inline]
    pub fn from_env(env: &worker::Env, binding: &str) -> Result<Self, KvError> {
        let store = env.kv(binding).map_err(|err| {
            KvError::Internal(anyhow::anyhow!("failed to open kv binding: {err}"))
        })?;
        Ok(Self { store })
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl KvStore for CloudflareKvStore {
    #[inline]
    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("delete failed: {err}")))
    }

    #[inline]
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.get_bytes(key).await?.is_some())
    }

    #[inline]
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        let result = self
            .store
            .get(key)
            .bytes()
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("get failed: {err}")))?;
        Ok(result.map(Bytes::from))
    }

    #[inline]
    async fn list_keys_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KvPage, KvError> {
        let limit_u64 = u64::try_from(limit)
            .map_err(|err| KvError::Validation(format!("list limit exceeds u64: {err}")))?;
        let mut request = self.store.list().limit(limit_u64);

        if !prefix.is_empty() {
            request = request.prefix(prefix.to_owned());
        }
        if let Some(cursor_str) = cursor.filter(|value| !value.is_empty()) {
            request = request.cursor(cursor_str.to_owned());
        }

        let response = request
            .execute()
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("list execute failed: {err}")))?;

        Ok(KvPage {
            keys: response.keys.into_iter().map(|key| key.name).collect(),
            cursor: (!response.list_complete)
                .then_some(response.cursor)
                .flatten()
                .filter(|value| !value.is_empty()),
        })
    }

    #[inline]
    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .put_bytes(key, value.as_ref())
            .map_err(|err| KvError::Internal(anyhow::anyhow!("put failed: {err}")))?
            .execute()
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("put execute failed: {err}")))
    }

    #[inline]
    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        // `KvHandle::validate_ttl` enforces a minimum of 60s, so sub-second
        // truncation via `as_secs()` cannot produce a zero TTL here.
        let ttl_secs = ttl.as_secs();

        self.store
            .put_bytes(key, value.as_ref())
            .map_err(|err| KvError::Internal(anyhow::anyhow!("put failed: {err}")))?
            .expiration_ttl(ttl_secs)
            .execute()
            .await
            .map_err(|err| KvError::Internal(anyhow::anyhow!("put with ttl execute failed: {err}")))
    }
}

// TODO: integration tests require a wasm32 target + wrangler.
// Test `CloudflareKvStore` as part of the Cloudflare adapter E2E test suite.
