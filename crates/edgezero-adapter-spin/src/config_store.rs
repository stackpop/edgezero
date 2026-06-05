//! Spin adapter config store: wraps `SpinSdkKvStore`.
//!
//! KV-backed (was variables-backed up through 2026-Q2). Handlers query
//! the store with the canonical dotted key (`service.timeout_ms`); the
//! Spin KV API accepts arbitrary key bytes, so no `.→__` translation
//! is needed. The per-id platform store name is supplied at construction
//! by [`crate::request::build_config_registry`], which resolves it
//! through `EDGEZERO__STORES__CONFIG__<ID>__NAME`.

use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::key_value::Store as SpinSdkKvStore;
#[cfg(test)]
use std::collections::BTreeMap;

/// Config store backed by a Spin KV store.
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(test)]
    InMemory(BTreeMap<String, bytes::Bytes>),
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    Spin {
        label: String,
        store: SpinSdkKvStore,
    },
    /// Never constructed; keeps the enum inhabited outside production Spin and tests.
    #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
    _Uninhabited(std::convert::Infallible),
}

impl SpinConfigStore {
    /// Build an in-memory fixture from `(key, bytes)` pairs.
    ///
    /// Bytes are stored verbatim — `get` strictly decodes UTF-8, mirroring
    /// the wasm backend's behaviour (the contract `non_utf8_value_returns_unavailable`
    /// test exercises the error path explicitly).
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, bytes::Bytes)>) -> Self {
        Self {
            inner: SpinConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }

    /// Open the platform store once. Called from
    /// [`crate::request::build_config_registry`] during dispatch setup so
    /// missing `key_value_stores = [...]` declarations surface as a clean
    /// dispatch error instead of on first config read.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::internal`] when the underlying
    /// `SpinSdkKvStore::open` fails — typically because the label isn't
    /// declared in the component's `key_value_stores = [...]` AND
    /// registered with a backend in `runtime-config.toml`. This is a
    /// structural / permanent failure (operator config drift), not a
    /// transient backend hiccup, so we report `Internal` rather than
    /// `Unavailable` so observability alerts on it and callers don't
    /// retry pointlessly.
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    #[inline]
    pub async fn open(label: String) -> Result<Self, ConfigStoreError> {
        let store = SpinSdkKvStore::open(&label).await.map_err(|err| {
            ConfigStoreError::internal(anyhow::anyhow!(
                "open `{label}`: {err} (is the label declared in spin.toml's `key_value_stores` AND registered in runtime-config.toml?)"
            ))
        })?;
        Ok(Self {
            inner: SpinConfigBackend::Spin { label, store },
        })
    }
}

#[async_trait(?Send)]
impl ConfigStore for SpinConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(test)]
            SpinConfigBackend::InMemory(map) => match map.get(key) {
                Some(bytes) => String::from_utf8(bytes.to_vec()).map(Some).map_err(|err| {
                    // Strict UTF-8 to match the wasm backend's error path.
                    // `from_utf8_lossy` would silently hide a divergence
                    // between test and prod.
                    ConfigStoreError::unavailable(format!("non-utf8 value for `{key}`: {err}"))
                }),
                None => Ok(None),
            },
            #[cfg(all(feature = "spin", target_arch = "wasm32"))]
            SpinConfigBackend::Spin { label, store } => match store.get(key).await {
                Ok(Some(bytes)) => String::from_utf8(bytes).map(Some).map_err(|err| {
                    ConfigStoreError::unavailable(format!(
                        "store `{label}`: non-utf8 value for `{key}`: {err}"
                    ))
                }),
                Ok(None) => Ok(None),
                Err(err) => Err(ConfigStoreError::unavailable(format!(
                    "store `{label}`: {err}"
                ))),
            },
            #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
            SpinConfigBackend::_Uninhabited(never) => {
                let _: &str = key;
                match *never {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    // Contract tests exercise the InMemory backend with bytes-backed values.
    // KV accepts arbitrary key bytes so the dotted-key form is preserved
    // verbatim end-to-end (no `.→__` translation any more — see module docs).
    edgezero_core::config_store_contract_tests!(spin_config_store_contract, {
        SpinConfigStore::from_entries([
            (
                "contract.key.a".to_owned(),
                bytes::Bytes::from_static(b"value_a"),
            ),
            (
                "contract.key.b".to_owned(),
                bytes::Bytes::from_static(b"value_b"),
            ),
        ])
    });

    #[test]
    fn dotted_get_resolves_verbatim_under_kv() {
        // The KV backend stores keys verbatim — `feature.new_checkout`
        // round-trips without the legacy `.→__` translation.
        let store = SpinConfigStore::from_entries([
            (
                "feature.new_checkout".to_owned(),
                bytes::Bytes::from_static(b"false"),
            ),
            (
                "service.timeout_ms".to_owned(),
                bytes::Bytes::from_static(b"1500"),
            ),
        ]);

        assert_eq!(
            block_on(store.get("feature.new_checkout")).expect("dotted lookup"),
            Some("false".to_owned()),
        );
        assert_eq!(
            block_on(store.get("service.timeout_ms")).expect("dotted lookup"),
            Some("1500".to_owned()),
        );
        // Negative: the legacy flat form is NOT a fallback any more.
        assert_eq!(
            block_on(store.get("feature__new_checkout")).expect("flat lookup"),
            None,
            "KV accepts arbitrary keys; the dotted and flat forms are distinct"
        );
    }

    #[test]
    fn non_utf8_value_returns_unavailable() {
        // Mirrors the wasm backend's strict-UTF-8 path. Documents the
        // contract that binary KV values are NOT silently lossily decoded.
        let store = SpinConfigStore::from_entries([(
            "binary".to_owned(),
            // `0xFF` is not a valid UTF-8 lead byte.
            bytes::Bytes::from_static(&[0xFF_u8, 0xFE_u8]),
        )]);
        let err = block_on(store.get("binary")).expect_err("non-utf8 -> error");
        let msg = err.to_string();
        assert!(
            msg.contains("non-utf8 value for `binary`"),
            "expected non-utf8 message, got: {msg}"
        );
    }

    #[test]
    fn missing_key_returns_none() {
        let store = SpinConfigStore::from_entries([]);
        assert_eq!(block_on(store.get("absent")).expect("ok"), None);
    }
}
