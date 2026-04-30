//! Provider-neutral read-only configuration store abstraction.
//!
//! All platforms expose config reads as synchronous operations, so no
//! `async_trait` is needed here.

use std::fmt;
use std::sync::Arc;

use anyhow::Error as AnyError;
use thiserror::Error;

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
///             ("contract.key.a".to_owned(), "value_a".to_owned()),
///             ("contract.key.b".to_owned(), "value_b".to_owned()),
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
                    Some("value_a".to_owned())
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
                    Some("value_a".to_owned())
                );
                assert_eq!(
                    store.get("contract.key.b").expect("second config value"),
                    Some("value_b".to_owned())
                );
            }

            #[$test_attr]
            fn contract_key_lookup_is_case_sensitive() {
                let store = $factory;
                // lowercase "contract.key.a" exists; uppercase must not match
                assert_eq!(store.get("CONTRACT.KEY.A").expect("case-sensitive miss"), None);
            }

            #[$test_attr]
            fn contract_empty_key_returns_none_or_invalid_key() {
                let store = $factory;
                // Backends may either return Ok(None) or Err(InvalidKey) for an empty key.
                // Fastly's Config Store SDK may reject empty keys rather than returning None.
                match store.get("") {
                    Ok(None) => {}
                    Ok(Some(_)) => panic!("empty key should not return a value"),
                    Err($crate::config_store::ConfigStoreError::InvalidKey { .. }) => {}
                    Err(err) => panic!("unexpected error for empty key: {}", err),
                }
            }

            #[$test_attr]
            fn contract_handle_wraps_store() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let handle = ConfigStoreHandle::new(Arc::new($factory));
                assert_eq!(
                    handle.get("contract.key.a").expect("handle value"),
                    Some("value_a".to_owned())
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
// Trait
// ---------------------------------------------------------------------------

/// Errors returned by config-store backends.
///
/// Missing keys are represented as `Ok(None)` from [`ConfigStore::get`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigStoreError {
    /// An unexpected backend or provider failure occurred.
    #[error("config store error: {source}")]
    Internal { source: AnyError },
    /// The caller asked for a key that is malformed for the active backend.
    #[error("{message}")]
    InvalidKey { message: String },
    /// The configured backend cannot currently serve requests.
    #[error("config store unavailable: {message}")]
    Unavailable { message: String },
}

impl ConfigStoreError {
    /// Wrap an unexpected backend or provider failure.
    #[inline]
    pub fn internal<E>(error: E) -> Self
    where
        E: Into<AnyError>,
    {
        Self::Internal {
            source: error.into(),
        }
    }

    /// Create an error for malformed or backend-invalid keys.
    #[inline]
    pub fn invalid_key<S: Into<String>>(message: S) -> Self {
        Self::InvalidKey {
            message: message.into(),
        }
    }

    /// Create an error for temporarily unavailable backends.
    #[inline]
    pub fn unavailable<S: Into<String>>(message: S) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }
}

/// Object-safe interface for read-only configuration store backends.
///
/// Implementations exist per adapter:
/// - `AxumConfigStore` (axum adapter) — env vars + in-memory defaults for dev
/// - `FastlyConfigStore` (fastly adapter) — Fastly Config Store
/// - `CloudflareConfigStore` (cloudflare adapter) — Cloudflare env bindings
pub trait ConfigStore: Send + Sync {
    /// Retrieve a config value by key. Returns `None` if the key does not exist.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError`] if `key` is invalid or the backend is unavailable.
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
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigStoreHandle").finish_non_exhaustive()
    }
}

impl ConfigStoreHandle {
    /// Get a config value by key.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError`] if `key` is invalid or the backend is unavailable.
    #[inline]
    pub fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        self.store.get(key)
    }

    /// Create a new handle wrapping a config store implementation.
    #[inline]
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Run the shared contract tests against TestConfigStore.
    crate::config_store_contract_tests!(
        test_config_store_contract,
        TestConfigStore::new(&[("contract.key.a", "value_a"), ("contract.key.b", "value_b"),])
    );

    use super::*;
    use std::collections::HashMap;

    struct FailingConfigStore;

    struct TestConfigStore {
        data: HashMap<String, String>,
    }

    impl ConfigStore for FailingConfigStore {
        fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Err(ConfigStoreError::unavailable("backend offline"))
        }
    }

    impl ConfigStore for TestConfigStore {
        fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(self.data.get(key).cloned())
        }
    }

    impl TestConfigStore {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                data: entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            }
        }
    }

    fn handle(entries: &[(&str, &str)]) -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(TestConfigStore::new(entries)))
    }

    #[test]
    fn config_store_get_returns_none_for_missing_key() {
        let store_handle = handle(&[]);
        assert_eq!(
            store_handle.get("nonexistent").expect("missing config"),
            None
        );
    }

    #[test]
    fn config_store_get_returns_value_for_existing_key() {
        let store_handle = handle(&[("feature.checkout", "true")]);
        assert_eq!(
            store_handle.get("feature.checkout").expect("config value"),
            Some("true".to_owned())
        );
    }

    #[test]
    fn config_store_handle_debug_output() {
        let store_handle = handle(&[]);
        let debug = format!("{store_handle:?}");
        assert!(debug.contains("ConfigStoreHandle"));
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
        let store_handle = ConfigStoreHandle::new(store);
        assert_eq!(
            store_handle.get("a").expect("arc-backed config"),
            Some("1".to_owned())
        );
    }

    #[test]
    fn config_store_handle_propagates_backend_errors() {
        let handle = ConfigStoreHandle::new(Arc::new(FailingConfigStore));
        let err = handle
            .get("feature.checkout")
            .expect_err("expected backend error");
        assert!(matches!(err, ConfigStoreError::Unavailable { .. }));
    }

    #[test]
    fn config_store_handle_wraps_and_delegates() {
        let store_handle = handle(&[("timeout_ms", "1500")]);
        assert_eq!(
            store_handle.get("timeout_ms").expect("config value"),
            Some("1500".to_owned())
        );
        assert_eq!(store_handle.get("missing").expect("missing config"), None);
    }
}
