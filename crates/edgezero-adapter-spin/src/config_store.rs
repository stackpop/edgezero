//! Spin adapter config store: wraps `spin_sdk::variables`.

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};

/// Config store backed by Spin component variables.
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(target_arch = "wasm32")]
    Spin,
    #[cfg(test)]
    InMemory(std::collections::HashMap<String, String>),
    /// Never constructed; keeps the enum inhabited in non-wasm32, non-test builds.
    #[cfg(not(any(target_arch = "wasm32", test)))]
    _Uninhabited(std::convert::Infallible),
}

impl SpinConfigStore {
    /// Create a new `SpinConfigStore` using the Spin variables API.
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self {
            inner: SpinConfigBackend::Spin,
        }
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: SpinConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Default for SpinConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore for SpinConfigStore {
    // `key` is unused in the _Uninhabited arm on native non-test builds
    #[allow(unused_variables)]
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(target_arch = "wasm32")]
            SpinConfigBackend::Spin => {
                use spin_sdk::variables;
                match variables::get(key) {
                    Ok(value) => Ok(Some(value)),
                    Err(variables::Error::Undefined(_)) => Ok(None),
                    Err(variables::Error::InvalidName(msg)) => {
                        Err(ConfigStoreError::invalid_key(msg))
                    }
                    Err(e) => Err(ConfigStoreError::unavailable(e.to_string())),
                }
            }
            #[cfg(test)]
            SpinConfigBackend::InMemory(data) => Ok(data.get(key).cloned()),
            #[cfg(not(any(target_arch = "wasm32", test)))]
            SpinConfigBackend::_Uninhabited(never) => match *never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    edgezero_core::config_store_contract_tests!(spin_config_store_contract, {
        SpinConfigStore::from_entries([
            ("contract.key.a".to_string(), "value_a".to_string()),
            ("contract.key.b".to_string(), "value_b".to_string()),
        ])
    });
}
