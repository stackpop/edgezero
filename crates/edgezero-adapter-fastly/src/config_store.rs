//! Fastly adapter config store: wraps `fastly::ConfigStore`.

#[cfg(test)]
use std::collections::HashMap;

use edgezero_core::config_store::{ConfigStore, ConfigStoreError};

/// Config store backed by a Fastly Config Store resource link.
pub struct FastlyConfigStore {
    inner: FastlyConfigStoreBackend,
}

enum FastlyConfigStoreBackend {
    Fastly(fastly::ConfigStore),
    #[cfg(test)]
    InMemory(HashMap<String, String>),
}

impl FastlyConfigStore {
    /// Open a Fastly Config Store by resource link name.
    ///
    /// Returns an error if the configured store cannot be opened.
    pub fn try_open(name: &str) -> Result<Self, fastly::config_store::OpenError> {
        fastly::ConfigStore::try_open(name).map(|inner| Self {
            inner: FastlyConfigStoreBackend::Fastly(inner),
        })
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: FastlyConfigStoreBackend::InMemory(entries.into_iter().collect()),
        }
    }
}

impl ConfigStore for FastlyConfigStore {
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            FastlyConfigStoreBackend::Fastly(inner) => inner.try_get(key).map_err(map_lookup_error),
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => Ok(data.get(key).cloned()),
        }
    }
}

fn map_lookup_error(err: fastly::config_store::LookupError) -> ConfigStoreError {
    match err {
        fastly::config_store::LookupError::KeyInvalid
        | fastly::config_store::LookupError::KeyTooLong => {
            ConfigStoreError::invalid_key("invalid config key")
        }
        fastly::config_store::LookupError::ConfigStoreInvalid
        | fastly::config_store::LookupError::TooManyLookups
        | fastly::config_store::LookupError::ValueTooLong
        | fastly::config_store::LookupError::Other => {
            ConfigStoreError::unavailable(format!("Fastly config store lookup failed: {err}"))
        }
        _ => ConfigStoreError::unavailable(format!("Fastly config store lookup failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    edgezero_core::config_store_contract_tests!(fastly_config_store_contract, {
        FastlyConfigStore::from_entries([
            ("contract.key.a".to_string(), "value_a".to_string()),
            ("contract.key.b".to_string(), "value_b".to_string()),
        ])
    });

    #[test]
    fn key_invalid_maps_to_invalid_key_error() {
        let err = map_lookup_error(fastly::config_store::LookupError::KeyInvalid);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }

    #[test]
    fn key_too_long_maps_to_invalid_key_error() {
        let err = map_lookup_error(fastly::config_store::LookupError::KeyTooLong);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }
}
