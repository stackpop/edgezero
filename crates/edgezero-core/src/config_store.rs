//! Provider-neutral read-only configuration store abstraction.
//!
//! All platforms expose config reads as synchronous operations, so no
//! `async_trait` is needed here.

use std::fmt;
use std::sync::Arc;

use anyhow::Error as AnyError;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Object-safe interface for read-only configuration store backends.
///
/// Implementations exist per adapter:
/// - `AxumConfigStore` (axum adapter) — env vars + in-memory defaults for dev
/// - `FastlyConfigStore` (fastly adapter) — Fastly Config Store
/// - `CloudflareConfigStore` (cloudflare adapter) — Cloudflare env bindings
///
/// Errors returned by config-store backends.
///
/// Missing keys are represented as `Ok(None)` from [`ConfigStore::get`].
#[derive(Debug, Error)]
pub enum ConfigStoreError {
    /// The caller asked for a key that is malformed for the active backend.
    #[error("{message}")]
    InvalidKey { message: String },
    /// The configured backend cannot currently serve requests.
    #[error("config store unavailable: {message}")]
    Unavailable { message: String },
    /// An unexpected backend or provider failure occurred.
    #[error("config store error: {source}")]
    Internal { source: AnyError },
}

impl ConfigStoreError {
    /// Create an error for malformed or backend-invalid keys.
    pub fn invalid_key(message: impl Into<String>) -> Self {
        Self::InvalidKey {
            message: message.into(),
        }
    }

    /// Create an error for temporarily unavailable backends.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    /// Wrap an unexpected backend or provider failure.
    pub fn internal<E>(error: E) -> Self
    where
        E: Into<AnyError>,
    {
        Self::Internal {
            source: error.into(),
        }
    }
}

pub trait ConfigStore: Send + Sync {
    /// Retrieve a config value by key. Returns `None` if the key does not exist.
    fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError>;
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
    pub fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
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
    ($mod_name:ident, #[$test_attr:meta], $factory:expr $(,)?) => {
        mod $mod_name {
            use super::*;
            use $crate::config_store::ConfigStore;

            #[$test_attr]
            fn contract_get_returns_value_for_existing_key() {
                let store = $factory;
                assert_eq!(
                    store.get("contract.key.a").expect("config value"),
                    Some("value_a".to_string())
                );
            }

            #[$test_attr]
            fn contract_get_returns_none_for_missing_key() {
                let store = $factory;
                assert_eq!(store.get("contract.key.missing").expect("config miss"), None);
            }

            #[$test_attr]
            fn contract_multiple_keys_are_independent() {
                let store = $factory;
                assert_eq!(
                    store.get("contract.key.a").expect("first config value"),
                    Some("value_a".to_string())
                );
                assert_eq!(
                    store.get("contract.key.b").expect("second config value"),
                    Some("value_b".to_string())
                );
            }

            #[$test_attr]
            fn contract_key_lookup_is_case_sensitive() {
                let store = $factory;
                // lowercase "contract.key.a" exists; uppercase must not match
                assert_eq!(store.get("CONTRACT.KEY.A").expect("case-sensitive miss"), None);
            }

            #[$test_attr]
            fn contract_empty_key_returns_none() {
                let store = $factory;
                assert_eq!(store.get("").expect("empty key miss"), None);
            }

            #[$test_attr]
            fn contract_handle_wraps_store() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let handle = ConfigStoreHandle::new(Arc::new($factory));
                assert_eq!(
                    handle.get("contract.key.a").expect("handle value"),
                    Some("value_a".to_string())
                );
                assert_eq!(handle.get("contract.key.missing").expect("handle miss"), None);
            }

            #[$test_attr]
            fn contract_cloned_handle_delegates_consistently() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let h1 = ConfigStoreHandle::new(Arc::new($factory));
                let h2 = h1.clone();
                assert_eq!(
                    h1.get("contract.key.a").expect("first handle value"),
                    h2.get("contract.key.a").expect("second handle value")
                );
                assert_eq!(
                    h1.get("contract.key.missing").expect("first handle miss"),
                    h2.get("contract.key.missing").expect("second handle miss")
                );
            }
        }
    };
    ($mod_name:ident, $factory:expr) => {
        $crate::config_store_contract_tests!($mod_name, #[test], $factory);
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
        fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(self.data.get(key).cloned())
        }
    }

    fn handle(entries: &[(&str, &str)]) -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(TestConfigStore::new(entries)))
    }

    #[test]
    fn config_store_get_returns_value_for_existing_key() {
        let h = handle(&[("feature.checkout", "true")]);
        assert_eq!(
            h.get("feature.checkout").expect("config value"),
            Some("true".to_string())
        );
    }

    #[test]
    fn config_store_get_returns_none_for_missing_key() {
        let h = handle(&[]);
        assert_eq!(h.get("nonexistent").expect("missing config"), None);
    }

    #[test]
    fn config_store_handle_wraps_and_delegates() {
        let h = handle(&[("timeout_ms", "1500")]);
        assert_eq!(
            h.get("timeout_ms").expect("config value"),
            Some("1500".to_string())
        );
        assert_eq!(h.get("missing").expect("missing config"), None);
    }

    #[test]
    fn config_store_handle_is_cloneable() {
        let h1 = handle(&[("key", "val")]);
        let h2 = h1.clone();
        assert_eq!(
            h1.get("key").expect("first handle value"),
            h2.get("key").expect("second handle value")
        );
    }

    #[test]
    fn config_store_handle_new_accepts_arc() {
        let store = Arc::new(TestConfigStore::new(&[("a", "1")]));
        let h = ConfigStoreHandle::new(store);
        assert_eq!(
            h.get("a").expect("arc-backed config"),
            Some("1".to_string())
        );
    }

    #[test]
    fn config_store_handle_debug_output() {
        let h = handle(&[]);
        let debug = format!("{:?}", h);
        assert!(debug.contains("ConfigStoreHandle"));
    }

    struct FailingConfigStore;

    impl ConfigStore for FailingConfigStore {
        fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Err(ConfigStoreError::unavailable("backend offline"))
        }
    }

    #[test]
    fn config_store_handle_propagates_backend_errors() {
        let handle = ConfigStoreHandle::new(Arc::new(FailingConfigStore));
        let err = handle
            .get("feature.checkout")
            .expect_err("expected backend error");
        assert!(matches!(err, ConfigStoreError::Unavailable { .. }));
    }

    // Run the shared contract tests against TestConfigStore.
    crate::config_store_contract_tests!(
        test_config_store_contract,
        TestConfigStore::new(&[("contract.key.a", "value_a"), ("contract.key.b", "value_b"),])
    );
}
