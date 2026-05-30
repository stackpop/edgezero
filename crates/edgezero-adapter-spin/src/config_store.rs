//! Spin adapter config store: wraps `spin_sdk::variables`.

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
#[cfg(test)]
use std::collections::HashMap;

/// Config store backed by Spin component variables.
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(test)]
    InMemory(HashMap<String, String>),
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    Spin,
    /// Never constructed; keeps the enum inhabited outside production Spin and tests.
    #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
    _Uninhabited(std::convert::Infallible),
}

impl SpinConfigStore {
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: SpinConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }

    /// Create a new `SpinConfigStore` using the Spin variables API.
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: SpinConfigBackend::Spin,
        }
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl Default for SpinConfigStore {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore for SpinConfigStore {
    #[inline]
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(test)]
            SpinConfigBackend::InMemory(data) => Ok(data.get(key).cloned()),
            #[cfg(all(feature = "spin", target_arch = "wasm32"))]
            SpinConfigBackend::Spin => {
                use spin_sdk::variables;
                match variables::get(key) {
                    Ok(value) => Ok(Some(value)),
                    Err(variables::Error::Undefined(_)) => Ok(None),
                    Err(variables::Error::InvalidName(msg)) => {
                        Err(ConfigStoreError::invalid_key(msg))
                    }
                    Err(err) => Err(ConfigStoreError::unavailable(err.to_string())),
                }
            }
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

    // These contract tests exercise the InMemory backend (not the real Spin
    // variables API). Dotted keys such as "contract.key.a" are valid here but
    // would trigger `InvalidName` on the real Spin backend, which requires
    // lowercase variable names without dots. Real-backend behaviour is
    // verified by the smoke tests in scripts/smoke_test_config.sh.
    edgezero_core::config_store_contract_tests!(spin_config_store_contract, {
        SpinConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });
}
