//! Provider-neutral read-only configuration store abstraction.
//!
//! `ConfigStore::get` is `async` because the Cloudflare config store reads
//! from a KV namespace whose `get` is JS-interop and asynchronous. Other
//! backends complete synchronously and resolve immediately.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error as AnyError;
use async_trait::async_trait;
use thiserror::Error;

use crate::error::EdgeError;
use crate::time::{DEADLINE_FAR_FUTURE, Deadline};

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
///     AxumConfigStore::from_map([
///         ("contract.key.a".to_owned(), "value_a".to_owned()),
///         ("contract.key.b".to_owned(), "value_b".to_owned()),
///     ])
/// });
/// ```
#[macro_export]
macro_rules! config_store_contract_tests {
    ($mod_name:ident, #[$test_attr:meta], $factory:expr $(,)?) => {
        mod $mod_name {
            use super::*;
            use $crate::config_store::ConfigStore;

            fn run<Fut: ::std::future::Future>(future: Fut) -> Fut::Output {
                ::futures::executor::block_on(future)
            }

            #[$test_attr]
            fn contract_get_returns_value_for_existing_key() {
                let store = $factory;
                run(async {
                    assert_eq!(
                        store.get("contract.key.a").await.expect("config value"),
                        Some("value_a".to_owned())
                    );
                });
            }

            #[$test_attr]
            fn contract_get_returns_none_for_missing_key() {
                let store = $factory;
                run(async {
                    assert_eq!(
                        store.get("contract.key.missing").await.expect("config miss"),
                        None
                    );
                });
            }

            #[$test_attr]
            fn contract_multiple_keys_are_independent() {
                let store = $factory;
                run(async {
                    assert_eq!(
                        store.get("contract.key.a").await.expect("first config value"),
                        Some("value_a".to_owned())
                    );
                    assert_eq!(
                        store.get("contract.key.b").await.expect("second config value"),
                        Some("value_b".to_owned())
                    );
                });
            }

            #[$test_attr]
            fn contract_key_lookup_is_case_sensitive() {
                let store = $factory;
                run(async {
                    // lowercase "contract.key.a" exists; uppercase must not match
                    assert_eq!(
                        store.get("CONTRACT.KEY.A").await.expect("case-sensitive miss"),
                        None
                    );
                });
            }

            #[$test_attr]
            fn contract_empty_key_returns_none_or_invalid_key() {
                let store = $factory;
                run(async {
                    // Backends may either return Ok(None) or Err(InvalidKey) for an empty key.
                    // Fastly's Config Store SDK may reject empty keys rather than returning None.
                    match store.get("").await {
                        Ok(None) => {}
                        Ok(Some(_)) => panic!("empty key should not return a value"),
                        Err($crate::config_store::ConfigStoreError::InvalidKey { .. }) => {}
                        Err(err) => panic!("unexpected error for empty key: {}", err),
                    }
                });
            }

            #[$test_attr]
            fn contract_handle_wraps_store() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let handle = ConfigStoreHandle::new(Arc::new($factory));
                run(async {
                    assert_eq!(
                        handle.get("contract.key.a").await.expect("handle value"),
                        Some("value_a".to_owned())
                    );
                    assert_eq!(
                        handle.get("contract.key.missing").await.expect("handle miss"),
                        None
                    );
                });
            }

            #[$test_attr]
            fn contract_cloned_handle_delegates_consistently() {
                use std::sync::Arc;
                use $crate::config_store::ConfigStoreHandle;

                let h1 = ConfigStoreHandle::new(Arc::new($factory));
                let h2 = h1.clone();
                run(async {
                    assert_eq!(
                        h1.get("contract.key.a").await.expect("first handle value"),
                        h2.get("contract.key.a").await.expect("second handle value")
                    );
                    assert_eq!(
                        h1.get("contract.key.missing").await.expect("first handle miss"),
                        h2.get("contract.key.missing").await.expect("second handle miss")
                    );
                });
            }
        }
    };
    ($mod_name:ident, $factory:expr) => {
        $crate::config_store_contract_tests!($mod_name, #[test], $factory);
    };
}

pub const DEFAULT_CONFIG_BACKEND_BYTES: u64 = 0x0100_0000;
pub const DEFAULT_CONFIG_BLOB_BYTES: u64 = 0x0080_0000;
pub const DEFAULT_CONFIG_EXTRACTION_BYTES: u64 = 0x0100_0000;
pub const DEFAULT_CONFIG_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_CONFIG_SECRET_BYTES: u64 = 0x0010_0000;

/// Per-extraction limits shared by the root config blob and every referenced secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigExtractionLimits {
    pub max_backend_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_secret_bytes: u64,
    pub max_total_bytes: u64,
    pub timeout: Duration,
}

impl ConfigExtractionLimits {
    /// Validates this startup policy before an adapter begins serving.
    ///
    /// # Errors
    /// Returns an internal policy error for zero, inconsistent, or unbounded values.
    #[inline]
    pub fn validate(self) -> Result<Self, EdgeError> {
        if self.max_backend_bytes == 0
            || self.max_blob_bytes == 0
            || self.max_secret_bytes == 0
            || self.max_total_bytes == 0
        {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "config extraction byte limits must be nonzero"
            )));
        }
        if self.max_total_bytes < self.max_blob_bytes.max(self.max_secret_bytes) {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "config extraction total byte limit is below a per-value limit"
            )));
        }
        if self.timeout.is_zero() || self.timeout > DEADLINE_FAR_FUTURE {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "config extraction timeout must be finite and nonzero"
            )));
        }
        Ok(self)
    }
}

impl Default for ConfigExtractionLimits {
    #[inline]
    fn default() -> Self {
        Self {
            max_backend_bytes: DEFAULT_CONFIG_BACKEND_BYTES,
            max_blob_bytes: DEFAULT_CONFIG_BLOB_BYTES,
            max_secret_bytes: DEFAULT_CONFIG_SECRET_BYTES,
            max_total_bytes: DEFAULT_CONFIG_EXTRACTION_BYTES,
            timeout: DEFAULT_CONFIG_EXTRACTION_TIMEOUT,
        }
    }
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
    /// The absolute read deadline expired before a complete value was available.
    #[error("config store read deadline exceeded")]
    DeadlineExceeded,
    /// An unexpected backend or provider failure occurred.
    #[error("config store error: {source}")]
    Internal { source: AnyError },
    /// The caller asked for a key that is malformed for the active backend.
    #[error("{message}")]
    InvalidKey { message: String },
    /// The configured backend cannot currently serve requests.
    #[error("config store unavailable: {message}")]
    Unavailable { message: String },
    /// The value or guest-visible backend read exceeded its supplied allowance.
    #[error("config store value exceeds configured byte limit")]
    ValueTooLarge,
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

/// Result of one bounded store lookup, including all bytes exposed to guest code.
#[derive(Debug)]
pub struct BoundedStoreRead<T> {
    pub backend_bytes: u64,
    pub value: Option<T>,
}

/// Object-safe interface for read-only configuration store backends.
///
/// Implementations exist per adapter:
/// - `AxumConfigStore` (axum adapter) — env vars + in-memory defaults for dev
/// - `FastlyConfigStore` (fastly adapter) — Fastly Config Store
/// - `CloudflareConfigStore` (cloudflare adapter) — Cloudflare KV namespace
/// - `SpinConfigStore` (spin adapter) — Spin KV (`spin_sdk::key_value::Store`)
#[async_trait(?Send)]
pub trait ConfigStore: Send + Sync {
    /// Retrieve a config value by key. Returns `None` if the key does not exist.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError`] if `key` is invalid or the backend is unavailable.
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError>;

    /// Retrieves one value under an absolute deadline and independent backend/value caps.
    ///
    /// The default is a cooperative compatibility implementation: it checks before and after
    /// the unbounded provider call, then discards an oversized materialized result. Providers
    /// must override it before claiming native allocation or cancellation guarantees.
    #[inline]
    async fn get_bounded(
        &self,
        key: &str,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
        if deadline.is_expired() {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        let value = self.get(key).await?;
        if deadline.is_expired() {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        let backend_bytes = value.as_ref().map_or(Ok(0_u64), |stored_value| {
            u64::try_from(stored_value.len())
                .map_err(|_length_error| ConfigStoreError::ValueTooLarge)
        })?;
        if backend_bytes > max_backend_bytes || backend_bytes > max_value_bytes {
            return Err(ConfigStoreError::ValueTooLarge);
        }
        Ok(BoundedStoreRead {
            backend_bytes,
            value,
        })
    }
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
    pub async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        self.store.get(key).await
    }

    /// Get a config value under one absolute deadline and two byte limits.
    ///
    /// # Errors
    /// Preserves the provider's typed bounded-read error.
    #[inline]
    pub async fn get_bounded(
        &self,
        key: &str,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
        self.store
            .get_bounded(key, deadline, max_backend_bytes, max_value_bytes)
            .await
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
    #![expect(
        clippy::missing_trait_methods,
        reason = "legacy provider stubs intentionally exercise the bounded-read compatibility default"
    )]

    // Run the shared contract tests against TestConfigStore.
    crate::config_store_contract_tests!(
        test_config_store_contract,
        TestConfigStore::new(&[("contract.key.a", "value_a"), ("contract.key.b", "value_b"),])
    );

    use super::*;
    use crate::time::Deadline;
    use futures::executor::block_on;
    use std::collections::HashMap;
    use std::time::Duration;

    struct FailingConfigStore;

    struct TestConfigStore {
        data: HashMap<String, String>,
    }

    #[async_trait(?Send)]
    impl ConfigStore for FailingConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Err(ConfigStoreError::unavailable("backend offline"))
        }
    }

    #[async_trait(?Send)]
    impl ConfigStore for TestConfigStore {
        async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
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
            block_on(store_handle.get("nonexistent")).expect("missing config"),
            None
        );
    }

    #[test]
    fn config_store_get_returns_value_for_existing_key() {
        let store_handle = handle(&[("feature.checkout", "true")]);
        assert_eq!(
            block_on(store_handle.get("feature.checkout")).expect("config value"),
            Some("true".to_owned())
        );
    }

    #[test]
    fn bounded_exact_cap_succeeds_and_over_cap_discards_value() {
        let store_handle = handle(&[("feature.checkout", "true")]);
        let exact = block_on(store_handle.get_bounded(
            "feature.checkout",
            Deadline::after(Duration::from_secs(1)),
            4,
            4,
        ))
        .expect("exact bounded read");
        assert_eq!(exact.backend_bytes, 4);
        assert_eq!(exact.value.as_deref(), Some("true"));

        let error = block_on(store_handle.get_bounded(
            "feature.checkout",
            Deadline::after(Duration::from_secs(1)),
            3,
            4,
        ))
        .expect_err("backend cap");
        assert!(matches!(error, ConfigStoreError::ValueTooLarge));
    }

    #[test]
    fn bounded_limit_defaults_are_finite_and_validation_rejects_invalid_relationships() {
        let limits = ConfigExtractionLimits::default();
        assert_eq!(limits.max_blob_bytes, DEFAULT_CONFIG_BLOB_BYTES);
        assert_eq!(limits.max_backend_bytes, DEFAULT_CONFIG_BACKEND_BYTES);
        assert_eq!(limits.max_secret_bytes, DEFAULT_CONFIG_SECRET_BYTES);
        assert_eq!(limits.max_total_bytes, DEFAULT_CONFIG_EXTRACTION_BYTES);
        assert_eq!(limits.timeout, DEFAULT_CONFIG_EXTRACTION_TIMEOUT);
        limits.validate().expect("valid defaults");

        let invalid_total = ConfigExtractionLimits {
            max_total_bytes: 1,
            ..limits
        };
        invalid_total
            .validate()
            .expect_err("total below per-value cap");

        let invalid_timeout = ConfigExtractionLimits {
            timeout: Duration::ZERO,
            ..limits
        };
        invalid_timeout.validate().expect_err("zero timeout");
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
            block_on(h1.get("key")).expect("first handle value"),
            block_on(h2.get("key")).expect("second handle value")
        );
    }

    #[test]
    fn config_store_handle_new_accepts_arc() {
        let store = Arc::new(TestConfigStore::new(&[("a", "1")]));
        let store_handle = ConfigStoreHandle::new(store);
        assert_eq!(
            block_on(store_handle.get("a")).expect("arc-backed config"),
            Some("1".to_owned())
        );
    }

    #[test]
    fn config_store_handle_propagates_backend_errors() {
        let handle = ConfigStoreHandle::new(Arc::new(FailingConfigStore));
        let err = block_on(handle.get("feature.checkout")).expect_err("expected backend error");
        assert!(matches!(err, ConfigStoreError::Unavailable { .. }));
    }

    #[test]
    fn config_store_handle_wraps_and_delegates() {
        let store_handle = handle(&[("timeout_ms", "1500")]);
        assert_eq!(
            block_on(store_handle.get("timeout_ms")).expect("config value"),
            Some("1500".to_owned())
        );
        assert_eq!(
            block_on(store_handle.get("missing")).expect("missing config"),
            None
        );
    }
}
