//! Provider-neutral secret store abstraction.
//!
//! # Architecture
//!
//! ```text
//!  Handler code         SecretHandle (get_bytes / require_bytes / require_str)
//!      │                       │
//!      └── Secrets extractor ─►│  UTF-8 / bytes layer
//!                              │
//!                  Arc<dyn SecretStore>
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!         EnvSecretStore  FastlySecretStore  CloudflareSecretStore
//! ```
//!
//! Secrets are read-only — this API only retrieves values,
//! it never writes or deletes them. Provisioning secrets is the
//! responsibility of each platform's deployment toolchain.

#[cfg(any(test, feature = "test-utils"))]
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::EdgeError;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned by secret store operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The requested secret was not found.
    #[error("secret not found: {name}")]
    NotFound { name: String },

    /// The secret store backend is temporarily unavailable.
    #[error("secret store unavailable")]
    Unavailable,

    /// A validation error (e.g., invalid secret name).
    #[error("validation error: {0}")]
    Validation(String),

    /// A general internal error.
    #[error("secret store error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<SecretError> for EdgeError {
    fn from(err: SecretError) -> Self {
        match err {
            SecretError::NotFound { .. } => {
                EdgeError::internal(anyhow::anyhow!("required secret is not configured"))
            }
            SecretError::Unavailable => EdgeError::service_unavailable("secret store unavailable"),
            SecretError::Validation(..) => {
                EdgeError::internal(anyhow::anyhow!("secret lookup failed"))
            }
            SecretError::Internal(..) => {
                EdgeError::internal(anyhow::anyhow!("secret store operation failed"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Maximum name length
// ---------------------------------------------------------------------------

/// Maximum length in bytes for any secret name or store name.
pub const MAX_NAME_LEN: usize = 512;

// ---------------------------------------------------------------------------
// Multi-store provider trait
// ---------------------------------------------------------------------------

/// Access secrets across multiple named stores.
///
/// Platforms with a single flat namespace (env vars, in-memory test stores)
/// implement this by keying on `"{store_name}/{key}"`.
/// Platforms with named stores (Fastly, Spin) open a store-specific handle
/// per `store_name`.
#[async_trait(?Send)]
pub trait SecretStore: Send + Sync {
    /// Retrieve a secret from a named store. Returns `Ok(None)` if not found.
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError>;
}

// ---------------------------------------------------------------------------
// No-op provider (test-utils)
// ---------------------------------------------------------------------------

/// A no-op [`SecretStore`] for tests that don't need secrets.
///
/// All reads return `None`.
#[cfg(any(test, feature = "test-utils"))]
pub struct NoopSecretStore;

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl SecretStore for NoopSecretStore {
    async fn get_bytes(&self, _store_name: &str, _key: &str) -> Result<Option<Bytes>, SecretError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// In-memory provider (test-utils)
// ---------------------------------------------------------------------------

/// An in-memory [`SecretStore`] keyed by `"{store_name}/{key}"`.
///
/// Useful for contract tests and unit tests that need deterministic values
/// across multiple named stores.
#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySecretStore {
    secrets: HashMap<String, Bytes>,
}

#[cfg(any(test, feature = "test-utils"))]
impl InMemorySecretStore {
    /// Build with entries of the form `("{store_name}/{key}", value)`.
    pub fn new<I, K, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Bytes>,
    {
        Self {
            secrets: entries
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl SecretStore for InMemorySecretStore {
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        let compound = format!("{store_name}/{key}");
        Ok(self.secrets.get(&compound).cloned())
    }
}

// ---------------------------------------------------------------------------
// Provider handle
// ---------------------------------------------------------------------------

/// A cloneable, ergonomic handle to a multi-store [`SecretStore`].
///
/// Validates both `store_name` and `key` before delegating to the provider.
#[derive(Clone)]
pub struct SecretHandle {
    provider: Arc<dyn SecretStore>,
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretHandle").finish_non_exhaustive()
    }
}

impl SecretHandle {
    /// Create a new handle wrapping a multi-store provider.
    pub fn new(provider: Arc<dyn SecretStore>) -> Self {
        Self { provider }
    }

    /// Retrieve a secret from a named store. Returns `Ok(None)` if not found.
    ///
    /// # Errors
    /// Returns [`SecretError::Validation`] for invalid `store_name`/`key`, [`SecretError::Unavailable`] if the backend is offline, or [`SecretError::Internal`] on backend failure.
    pub async fn get_bytes(
        &self,
        store_name: &str,
        key: &str,
    ) -> Result<Option<Bytes>, SecretError> {
        validate_name(store_name)?;
        validate_name(key)?;
        self.provider.get_bytes(store_name, key).await
    }

    /// Retrieve a secret as raw bytes. Returns `SecretError::NotFound` if absent.
    ///
    /// # Errors
    /// Returns [`SecretError::NotFound`] if the secret is absent, plus the same errors as [`SecretHandle::get_bytes`].
    pub async fn require_bytes(&self, store_name: &str, key: &str) -> Result<Bytes, SecretError> {
        self.get_bytes(store_name, key)
            .await?
            .ok_or_else(|| SecretError::NotFound {
                name: format!("{store_name}/{key}"),
            })
    }

    /// Retrieve a secret as a UTF-8 string. Returns `SecretError::NotFound` if absent.
    ///
    /// # Errors
    /// Returns [`SecretError::Internal`] if the secret bytes are not valid UTF-8, plus the same errors as [`SecretHandle::require_bytes`].
    pub async fn require_str(&self, store_name: &str, key: &str) -> Result<String, SecretError> {
        let bytes = self.require_bytes(store_name, key).await?;
        String::from_utf8(bytes.into())
            .map_err(|e| SecretError::Internal(anyhow::anyhow!("secret is not valid UTF-8: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Shared validation
// ---------------------------------------------------------------------------

fn validate_name(name: &str) -> Result<(), SecretError> {
    if name.is_empty() {
        return Err(SecretError::Validation(
            "secret name cannot be empty".to_owned(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SecretError::Validation(format!(
            "secret name length {} exceeds limit of {} bytes",
            name.len(),
            MAX_NAME_LEN
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(SecretError::Validation(
            "secret name contains invalid control characters".to_owned(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Contract test macro
// ---------------------------------------------------------------------------

/// Generate a suite of contract tests for any [`SecretStore`] implementation.
///
/// The factory expression must produce a provider pre-populated with these
/// entries in the `"mystore"` store:
/// - `"contract_key"` → `Bytes::from("contract_value")`
/// - `"contract_key_2"` → `Bytes::from("another_value")`
/// - `"missing_key"` must NOT be present.
#[macro_export]
macro_rules! secret_store_contract_tests {
    ($mod_name:ident, $factory:expr) => {
        mod $mod_name {
            use super::*;
            use bytes::Bytes;
            use $crate::secret_store::SecretStore;

            fn run<F: std::future::Future>(f: F) -> F::Output {
                futures::executor::block_on(f)
            }

            #[test]
            fn contract_get_existing_returns_bytes() {
                let provider = $factory;
                run(async {
                    let result = provider.get_bytes("mystore", "contract_key").await.unwrap();
                    assert_eq!(result, Some(Bytes::from("contract_value")));
                });
            }

            #[test]
            fn contract_get_second_key_returns_bytes() {
                let provider = $factory;
                run(async {
                    let result = provider
                        .get_bytes("mystore", "contract_key_2")
                        .await
                        .unwrap();
                    assert_eq!(result, Some(Bytes::from("another_value")));
                });
            }

            #[test]
            fn contract_get_missing_returns_none() {
                let provider = $factory;
                run(async {
                    let result = provider.get_bytes("mystore", "missing_key").await.unwrap();
                    assert!(result.is_none());
                });
            }

            #[test]
            fn contract_wrong_store_returns_none() {
                let provider = $factory;
                run(async {
                    let result = provider
                        .get_bytes("other_store", "contract_key")
                        .await
                        .unwrap();
                    assert!(result.is_none());
                });
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
    use crate::http::StatusCode;
    use bytes::Bytes;
    use futures::executor::block_on;

    // -----------------------------------------------------------------------
    // SecretStoreProvider tests
    // -----------------------------------------------------------------------

    #[test]
    fn provider_in_memory_returns_value_for_existing_key() {
        let provider = InMemorySecretStore::new([("store/key", Bytes::from("hello"))]);
        block_on(async {
            let result = provider.get_bytes("store", "key").await.unwrap();
            assert_eq!(result, Some(Bytes::from("hello")));
        });
    }

    #[test]
    fn provider_in_memory_returns_none_for_missing_key() {
        let provider = InMemorySecretStore::new([("store/key", Bytes::from("hello"))]);
        block_on(async {
            let result = provider.get_bytes("store", "missing").await.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn provider_in_memory_returns_none_for_wrong_store() {
        let provider = InMemorySecretStore::new([("store/key", Bytes::from("hello"))]);
        block_on(async {
            let result = provider.get_bytes("other", "key").await.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn noop_provider_always_returns_none() {
        let provider = NoopSecretStore;
        block_on(async {
            let result = provider.get_bytes("any_store", "any_key").await.unwrap();
            assert!(result.is_none());
        });
    }

    // -----------------------------------------------------------------------
    // SecretProviderHandle tests
    // -----------------------------------------------------------------------

    fn provider_handle_with(entries: &[(&str, &str)]) -> SecretHandle {
        let provider = InMemorySecretStore::new(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Bytes::from((*v).to_owned()))),
        );
        SecretHandle::new(Arc::new(provider))
    }

    #[test]
    fn provider_handle_get_bytes_returns_value() {
        let h = provider_handle_with(&[("signing-keys/current", "abc123")]);
        block_on(async {
            let result = h.get_bytes("signing-keys", "current").await.unwrap();
            assert_eq!(result, Some(Bytes::from("abc123")));
        });
    }

    #[test]
    fn provider_handle_get_bytes_returns_none_for_missing() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let result = h.get_bytes("store", "missing").await.unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn provider_handle_require_bytes_errors_for_missing() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let err = h.require_bytes("store", "missing").await.unwrap_err();
            assert!(matches!(err, SecretError::NotFound { .. }));
        });
    }

    #[test]
    fn provider_handle_require_str_returns_value() {
        let h = provider_handle_with(&[("api-keys/prod", "secret_val")]);
        block_on(async {
            let val = h.require_str("api-keys", "prod").await.unwrap();
            assert_eq!(val, "secret_val");
        });
    }

    #[test]
    fn provider_handle_validates_empty_store_name() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let err = h.get_bytes("", "key").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn provider_handle_validates_empty_key() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let err = h.get_bytes("store", "").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn provider_handle_validates_control_chars_in_store_name() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let err = h.get_bytes("bad\x00store", "key").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn provider_handle_validates_control_chars_in_key() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let err = h.get_bytes("store", "bad\x00key").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn provider_handle_validates_oversized_name() {
        let h = provider_handle_with(&[]);
        block_on(async {
            let name = "x".repeat(MAX_NAME_LEN + 1);
            let err = h.get_bytes(&name, "key").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn secret_error_not_found_does_not_leak_secret_name() {
        let err: EdgeError = SecretError::NotFound {
            name: "API_KEY".to_owned(),
        }
        .into();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!err.message().contains("API_KEY"));
    }

    #[test]
    fn secret_error_validation_does_not_leak_details() {
        let err: EdgeError = SecretError::Validation("bad\x00name".to_owned()).into();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!err.message().contains("bad"));
    }

    secret_store_contract_tests!(in_memory_provider_contract, {
        InMemorySecretStore::new([
            ("mystore/contract_key", Bytes::from("contract_value")),
            ("mystore/contract_key_2", Bytes::from("another_value")),
        ])
    });
}
