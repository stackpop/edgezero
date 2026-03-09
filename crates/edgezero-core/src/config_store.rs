//! Provider-neutral read-only configuration store abstraction.
//!
//! All platforms expose config reads as synchronous operations, so no
//! `async_trait` is needed here.

use std::fmt;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Object-safe interface for read-only configuration store backends.
///
/// Implementations exist per adapter:
/// - `AxumConfigStore` (axum adapter) — env vars + in-memory defaults for dev
/// - `FastlyConfigStore` (fastly adapter) — Fastly Config Store
/// - `CloudflareConfigStore` (cloudflare adapter) — Cloudflare env bindings
pub trait ConfigStore: Send + Sync {
    /// Retrieve a config value by key. Returns `None` if the key does not exist.
    fn get(&self, key: &str) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A cloneable handle to a config store.
#[derive(Clone)]
pub struct ConfigStoreHandle {
    store: Arc<dyn ConfigStore>,
}

impl fmt::Debug for ConfigStoreHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigStoreHandle").finish_non_exhaustive()
    }
}

impl ConfigStoreHandle {
    /// Create a new handle wrapping a config store implementation.
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }

    /// Get a config value by key.
    pub fn get(&self, key: &str) -> Option<String> {
        self.store.get(key)
    }
}

// ---------------------------------------------------------------------------
// Contract test macro
// ---------------------------------------------------------------------------

/// Generate a suite of contract tests for any [`ConfigStore`] implementation.
///
/// The macro takes the module name and a factory expression that produces a
/// store **pre-seeded** with the following well-known contract keys:
///
/// | Key                   | Value       |
/// |-----------------------|-------------|
/// | `"contract.key.a"`    | `"value_a"` |
/// | `"contract.key.b"`    | `"value_b"` |
///
/// # Example
///
/// ```rust,ignore
/// edgezero_core::config_store_contract_tests!(axum_config_store_contract, {
///     AxumConfigStore::new(
///         [
///             ("contract.key.a".to_string(), "value_a".to_string()),
///             ("contract.key.b".to_string(), "value_b".to_string()),
///         ],
///         [],
///     )
/// });
/// ```
#[macro_export]
macro_rules! config_store_contract_tests {
    ($mod_name:ident, $factory:expr) => {
        mod $mod_name {
            use super::*;
            use $crate::config_store::ConfigStore;

            #[test]
            fn contract_get_returns_value_for_existing_key() {
                let store = $factory;
                assert_eq!(store.get("contract.key.a"), Some("value_a".to_string()));
            }

            #[test]
            fn contract_get_returns_none_for_missing_key() {
                let store = $factory;
                assert_eq!(store.get("contract.key.missing"), None);
            }

            #[test]
            fn contract_multiple_keys_are_independent() {
                let store = $factory;
                assert_eq!(store.get("contract.key.a"), Some("value_a".to_string()));
                assert_eq!(store.get("contract.key.b"), Some("value_b".to_string()));
            }

            #[test]
            fn contract_key_lookup_is_case_sensitive() {
                let store = $factory;
                // lowercase "contract.key.a" exists; uppercase must not match
                assert_eq!(store.get("CONTRACT.KEY.A"), None);
            }

            #[test]
            fn contract_empty_key_returns_none() {
                let store = $factory;
                assert_eq!(store.get(""), None);
            }

            #[test]
            fn contract_handle_wraps_store() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let handle = ConfigStoreHandle::new(Arc::new($factory));
                assert_eq!(handle.get("contract.key.a"), Some("value_a".to_string()));
                assert_eq!(handle.get("contract.key.missing"), None);
            }

            #[test]
            fn contract_cloned_handle_delegates_consistently() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let h1 = ConfigStoreHandle::new(Arc::new($factory));
                let h2 = h1.clone();
                assert_eq!(h1.get("contract.key.a"), h2.get("contract.key.a"));
                assert_eq!(
                    h1.get("contract.key.missing"),
                    h2.get("contract.key.missing")
                );
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct TestConfigStore {
        data: HashMap<String, String>,
    }

    impl TestConfigStore {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                data: entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
    }

    impl ConfigStore for TestConfigStore {
        fn get(&self, key: &str) -> Option<String> {
            self.data.get(key).cloned()
        }
    }

    fn handle(entries: &[(&str, &str)]) -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(TestConfigStore::new(entries)))
    }

    #[test]
    fn config_store_get_returns_value_for_existing_key() {
        let h = handle(&[("feature.checkout", "true")]);
        assert_eq!(h.get("feature.checkout"), Some("true".to_string()));
    }

    #[test]
    fn config_store_get_returns_none_for_missing_key() {
        let h = handle(&[]);
        assert_eq!(h.get("nonexistent"), None);
    }

    #[test]
    fn config_store_handle_wraps_and_delegates() {
        let h = handle(&[("timeout_ms", "1500")]);
        assert_eq!(h.get("timeout_ms"), Some("1500".to_string()));
        assert_eq!(h.get("missing"), None);
    }

    #[test]
    fn config_store_handle_is_cloneable() {
        let h1 = handle(&[("key", "val")]);
        let h2 = h1.clone();
        assert_eq!(h1.get("key"), h2.get("key"));
    }

    #[test]
    fn config_store_handle_new_accepts_arc() {
        let store = Arc::new(TestConfigStore::new(&[("a", "1")]));
        let h = ConfigStoreHandle::new(store);
        assert_eq!(h.get("a"), Some("1".to_string()));
    }

    #[test]
    fn config_store_handle_debug_output() {
        let h = handle(&[]);
        let debug = format!("{:?}", h);
        assert!(debug.contains("ConfigStoreHandle"));
    }

    // Run the shared contract tests against TestConfigStore.
    crate::config_store_contract_tests!(
        test_config_store_contract,
        TestConfigStore::new(&[("contract.key.a", "value_a"), ("contract.key.b", "value_b"),])
    );
}
