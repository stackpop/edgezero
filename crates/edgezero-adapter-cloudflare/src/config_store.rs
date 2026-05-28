//! Cloudflare Workers adapter config store: reads from a KV namespace.
//!
//! Each declared config id maps to its own Cloudflare KV namespace binding,
//! resolved at request time from `EDGEZERO__STORES__CONFIG__<ID>__NAME`.
//! Reads are async (`worker::kv::KvStore::get(key).text().await`).
//!
//! ```toml
//! # wrangler.toml
//! [[kv_namespaces]]
//! binding = "app_config"
//! id      = "abc123…"
//! ```
//!
//! This replaces the pre-rewrite `[vars]`-backed JSON-string config store.
//! `[vars]` bindings are restricted to JavaScript identifier syntax, so
//! arbitrary dotted keys had to be JSON-packed inside one variable. The KV
//! backing has no such restriction.

use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
#[cfg(test)]
use std::collections::HashMap;
#[cfg(not(any(all(feature = "cloudflare", target_arch = "wasm32"), test)))]
use std::convert::Infallible;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::kv::KvStore as WorkerKvStore;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::Env;

/// Config store backed by a Cloudflare KV namespace.
///
/// The namespace binding is opened at construction; individual reads are
/// async KV lookups against that namespace.
pub struct CloudflareConfigStore {
    inner: CloudflareConfigBackend,
}

enum CloudflareConfigBackend {
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    Kv(WorkerKvStore),
    #[cfg(test)]
    InMemory(HashMap<String, String>),
    /// Never constructed; keeps the enum inhabited off production/test cfgs.
    #[cfg(not(any(all(feature = "cloudflare", target_arch = "wasm32"), test)))]
    _Uninhabited(Infallible),
}

impl CloudflareConfigStore {
    /// Open the KV namespace bound as `binding_name`.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::Unavailable`] when the binding is missing
    /// or cannot be opened.
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    #[inline]
    pub fn from_env(env: &Env, binding_name: &str) -> Result<Self, ConfigStoreError> {
        let store = env.kv(binding_name).map_err(|err| {
            ConfigStoreError::unavailable(format!(
                "failed to open config KV binding '{binding_name}': {err}"
            ))
        })?;
        Ok(Self {
            inner: CloudflareConfigBackend::Kv(store),
        })
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: CloudflareConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }
}

#[async_trait(?Send)]
impl ConfigStore for CloudflareConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
            CloudflareConfigBackend::Kv(store) => store.get(key).text().await.map_err(|err| {
                ConfigStoreError::internal(anyhow::anyhow!("kv config get failed: {err}"))
            }),
            #[cfg(test)]
            CloudflareConfigBackend::InMemory(data) => Ok(data.get(key).cloned()),
            #[cfg(not(any(all(feature = "cloudflare", target_arch = "wasm32"), test)))]
            CloudflareConfigBackend::_Uninhabited(never) => {
                let _: &str = key;
                match *never {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    edgezero_core::config_store_contract_tests!(cloudflare_config_store_contract, {
        CloudflareConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });
}
