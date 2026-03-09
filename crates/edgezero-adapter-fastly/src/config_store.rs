//! Fastly adapter config store: wraps `fastly::ConfigStore`.

#[cfg(test)]
use std::collections::HashMap;

use edgezero_core::config_store::ConfigStore;

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
    /// Returns `None` if the store is not available (e.g. not configured in
    /// `fastly.toml`), allowing graceful fallback without panicking.
    pub fn try_open(name: &str) -> Option<Self> {
        fastly::ConfigStore::try_open(name).ok().map(|inner| Self {
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
    fn get(&self, key: &str) -> Option<String> {
        match &self.inner {
            FastlyConfigStoreBackend::Fastly(inner) => inner.try_get(key).ok().flatten(),
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => data.get(key).cloned(),
        }
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
}
