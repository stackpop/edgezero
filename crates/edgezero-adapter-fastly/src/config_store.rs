//! Fastly adapter config store: wraps `fastly::ConfigStore`.

use edgezero_core::config_store::ConfigStore;

/// Config store backed by a Fastly Config Store resource link.
pub struct FastlyConfigStore {
    inner: fastly::ConfigStore,
}

impl FastlyConfigStore {
    /// Open a Fastly Config Store by resource link name.
    ///
    /// Returns `None` if the store is not available (e.g. not configured in
    /// `fastly.toml`), allowing graceful fallback without panicking.
    pub fn try_open(name: &str) -> Option<Self> {
        fastly::ConfigStore::try_open(name)
            .ok()
            .map(|inner| Self { inner })
    }
}

impl ConfigStore for FastlyConfigStore {
    fn get(&self, key: &str) -> Option<String> {
        self.inner.try_get(key).ok().flatten()
    }
}

// Contract tests cannot run natively: `fastly::ConfigStore::try_open` requires
// the Viceroy runtime. Platform-level contract coverage is provided by the
// smoke test (`scripts/smoke_test_config.sh fastly`) which exercises the same
// keys against a live Viceroy instance.
