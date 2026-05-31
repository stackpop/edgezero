//! Spin adapter config store: wraps `spin_sdk::variables`.
//!
//! Handlers query the store with the canonical dotted key
//! (`service.timeout_ms`); the Spin backend stores it as a flat variable
//! (`service__timeout_ms`) because Spin variable names must match
//! `^[a-z][a-z0-9_]*$` (no dots; see the [Spin manifest reference][1]).
//! `SpinConfigStore::get` translates the dotted form to the flat form
//! before delegating to the backend so the handler-facing key surface
//! stays platform-neutral.
//!
//! Uppercase keys are passed through unchanged; the real Spin backend
//! will reject them as `InvalidName`. The translation is dot-only.
//!
//! [1]: https://spinframework.dev/manifest-reference

use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
#[cfg(test)]
use std::collections::HashMap;

/// Config store backed by Spin component variables.
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    Spin,
    #[cfg(test)]
    InMemory(HashMap<String, String>),
    /// Never constructed; keeps the enum inhabited outside production Spin and tests.
    #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
    _Uninhabited(std::convert::Infallible),
}

impl SpinConfigStore {
    /// Build an in-memory fixture from `(dotted_key, value)` pairs. The
    /// stored representation mirrors what the real Spin backend would see:
    /// each key is `translate_key`-translated on insert so contract tests
    /// can call `get` with the canonical dotted form and exercise the same
    /// translation path as production.
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: SpinConfigBackend::InMemory(
                entries
                    .into_iter()
                    .map(|(key, value)| (Self::translate_key(&key), value))
                    .collect(),
            ),
        }
    }

    /// Create a new `SpinConfigStore` using the Spin variables API.
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self {
            inner: SpinConfigBackend::Spin,
        }
    }

    /// Translate a canonical handler-facing config key into a Spin variable
    /// name: every `.` becomes `__`. Other characters are passed through.
    /// `pub(crate)` so tests can exercise the translation directly.
    #[inline]
    pub(crate) fn translate_key(key: &str) -> String {
        key.replace('.', "__")
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl Default for SpinConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl ConfigStore for SpinConfigStore {
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        let translated = SpinConfigStore::translate_key(key);
        match &self.inner {
            #[cfg(all(feature = "spin", target_arch = "wasm32"))]
            SpinConfigBackend::Spin => {
                use spin_sdk::variables;
                match variables::get(&translated).await {
                    Ok(value) => Ok(Some(value)),
                    Err(variables::Error::Undefined(_)) => Ok(None),
                    Err(variables::Error::InvalidName(msg)) => {
                        Err(ConfigStoreError::invalid_key(msg))
                    }
                    Err(e) => Err(ConfigStoreError::unavailable(e.to_string())),
                }
            }
            #[cfg(test)]
            SpinConfigBackend::InMemory(data) => Ok(data.get(&translated).cloned()),
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

    // Contract tests exercise the InMemory backend. `from_entries` translates
    // dotted keys on insert, so calling `get("contract.key.a")` here hits the
    // same `SpinConfigStore::translate_key("contract.key.a") = "contract__key__a"` path that
    // production uses against `spin_sdk::variables`.
    edgezero_core::config_store_contract_tests!(spin_config_store_contract, {
        SpinConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });

    #[test]
    fn translate_key_replaces_dots_with_double_underscore() {
        assert_eq!(
            SpinConfigStore::translate_key("service.timeout_ms"),
            "service__timeout_ms"
        );
        assert_eq!(
            SpinConfigStore::translate_key("feature.new_checkout"),
            "feature__new_checkout"
        );
        assert_eq!(SpinConfigStore::translate_key("a.b.c"), "a__b__c");
    }

    #[test]
    fn translate_key_passes_flat_keys_through_unchanged() {
        assert_eq!(SpinConfigStore::translate_key("greeting"), "greeting");
        assert_eq!(SpinConfigStore::translate_key("api_token"), "api_token");
        assert_eq!(SpinConfigStore::translate_key(""), "");
    }

    #[test]
    fn translate_key_does_not_lowercase() {
        // Spec: uppercase keys reaching the backend yield InvalidName;
        // the translation itself is dot-only and case-preserving.
        assert_eq!(
            SpinConfigStore::translate_key("Service.Timeout_Ms"),
            "Service__Timeout_Ms"
        );
    }

    #[test]
    fn dotted_get_resolves_against_flat_storage() {
        // End-to-end proof: a handler-facing dotted key round-trips through
        // the InMemory backend (which stores under the translated form).
        let store = SpinConfigStore::from_entries([
            ("feature.new_checkout".to_owned(), "false".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ]);

        assert_eq!(
            block_on(store.get("feature.new_checkout")).expect("dotted lookup"),
            Some("false".to_owned()),
        );
        assert_eq!(
            block_on(store.get("service.timeout_ms")).expect("dotted lookup"),
            Some("1500".to_owned()),
        );
        // Sanity: the flat form a caller-from-outside-the-translation would
        // use also works, because translation is idempotent on flat keys.
        assert_eq!(
            block_on(store.get("feature__new_checkout")).expect("flat lookup"),
            Some("false".to_owned()),
        );
    }
}
