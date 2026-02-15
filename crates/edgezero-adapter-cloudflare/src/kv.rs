//! Cloudflare Workers KV adapter.
//!
//! Wraps `worker::kv::KvStore` to implement the `edgezero_core::kv::KvStore` trait.
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
use edgezero_core::kv::{KvError, KvStore};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use std::time::Duration;

/// KV store backed by Cloudflare Workers KV.
///
/// Wraps a `worker::kv::KvStore` handle obtained via the environment binding.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub struct CloudflareKvStore {
    store: worker::kv::KvStore,
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl CloudflareKvStore {
    /// Create a new Cloudflare KV store from the environment binding name.
    ///
    /// The `binding` must match a KV namespace binding in `wrangler.toml`.
    /// Uses `env.kv(binding)` which is the idiomatic `worker` 0.7+ API.
    pub fn from_env(env: &worker::Env, binding: &str) -> Result<Self, KvError> {
        let store = env
            .kv(binding)
            .map_err(|e| KvError::Internal(anyhow::anyhow!("failed to open kv binding: {e}")))?;
        Ok(Self { store })
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl KvStore for CloudflareKvStore {
    async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
        let result = self
            .store
            .get(key)
            .bytes()
            .await
            .map_err(|e| KvError::Internal(anyhow::anyhow!("get failed: {e}")))?;
        Ok(result.map(Bytes::from))
    }

    async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
        self.store
            .put_bytes(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("put failed: {e}")))?
            .execute()
            .await
            .map_err(|e| KvError::Internal(anyhow::anyhow!("put execute failed: {e}")))
    }

    async fn put_bytes_with_ttl(
        &self,
        key: &str,
        value: Bytes,
        ttl: Duration,
    ) -> Result<(), KvError> {
        // Cloudflare KV requires a minimum TTL of 60 seconds.
        let ttl_secs = ttl.as_secs().max(60);

        self.store
            .put_bytes(key, value.as_ref())
            .map_err(|e| KvError::Internal(anyhow::anyhow!("put failed: {e}")))?
            .expiration_ttl(ttl_secs)
            .execute()
            .await
            .map_err(|e| KvError::Internal(anyhow::anyhow!("put with ttl execute failed: {e}")))
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.store
            .delete(key)
            .await
            .map_err(|e| KvError::Internal(anyhow::anyhow!("delete failed: {e}")))
    }

    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
        let mut all_keys = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut builder = self.store.list();
            if !prefix.is_empty() {
                builder = builder.prefix(prefix.to_string());
            }
            if let Some(ref c) = cursor {
                builder = builder.cursor(c.clone());
            }

            let response = builder
                .execute()
                .await
                .map_err(|e| KvError::Internal(anyhow::anyhow!("list failed: {e}")))?;

            for key in response.keys {
                all_keys.push(key.name);
            }

            if response.list_complete {
                break;
            }
            cursor = response.cursor;
        }

        Ok(all_keys)
    }
}

// TODO: integration tests require a wasm32 target + wrangler.
// Test `CloudflareKvStore` as part of the Cloudflare adapter E2E test suite.
