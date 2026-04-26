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
    ///
    /// # Errors
    /// Returns the underlying [`fastly::config_store::OpenError`] when the named store does not exist or cannot be opened.
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
            FastlyConfigStoreBackend::Fastly(inner) => {
                inner.try_get(key).map_err(|err| map_lookup_error(&err))
            }
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => Ok(data.get(key).cloned()),
        }
    }
}

fn map_lookup_error(err: &fastly::config_store::LookupError) -> ConfigStoreError {
    // `LookupError` is from the `fastly` crate; using a wildcard arm guards
    // against new variants being added in upstream point releases without
    // forcing us into a breaking match every bump.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "external enum; new variants must remain unavailable→unavailable"
    )]
    match err {
        fastly::config_store::LookupError::KeyInvalid
        | fastly::config_store::LookupError::KeyTooLong => {
            ConfigStoreError::invalid_key("invalid config key")
        }
        _ => {
            log::warn!("Fastly config store lookup failed: {err}");
            ConfigStoreError::unavailable("config store temporarily unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    edgezero_core::config_store_contract_tests!(fastly_config_store_contract, {
        FastlyConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });

    #[test]
    fn key_invalid_maps_to_invalid_key_error() {
        let err = map_lookup_error(&fastly::config_store::LookupError::KeyInvalid);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }

    #[test]
    fn key_too_long_maps_to_invalid_key_error() {
        let err = map_lookup_error(&fastly::config_store::LookupError::KeyTooLong);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }
}
