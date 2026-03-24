//! Provider-neutral secret store abstraction.
//!
//! # Architecture
//!
//! ```text
//!  Handler code         SecretHandle (get_str / require_str)
//!      │                       │
//!      └── Secrets extractor ─►│  UTF-8 / bytes layer
//!                              │
//!                     Arc<dyn SecretStore>  (object-safe, Bytes)
//!                              │
//!               ┌──────────────┼──────────────┐
//!               ▼              ▼              ▼
//!     EnvSecretStore  FastlySecretStore  CloudflareSecretStore
//! ```
//!
//! Secrets are read-only — this API only retrieves values,
//! it never writes or deletes them. Provisioning secrets is the
//! responsibility of each platform's deployment toolchain.

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
            // NotFound = server misconfiguration, never a client 404.
            // A missing API key means the platform isn't set up correctly,
            // not that the request was invalid.
            SecretError::NotFound { name } => EdgeError::internal(anyhow::anyhow!(
                "required secret '{}' is not configured -- check platform secret store bindings",
                name
            )),
            SecretError::Unavailable => {
                EdgeError::service_unavailable("secret store unavailable")
            }
            // Validation errors are programming errors (bad secret name in code),
            // not client errors.
            SecretError::Validation(e) => {
                EdgeError::internal(anyhow::anyhow!("secret name validation error: {e}"))
            }
            SecretError::Internal(e) => EdgeError::internal(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Object-safe interface for secret store backends.
///
/// All methods take `&self` — backends handle their own access model.
///
/// This trait is always called through [`SecretHandle`], which validates
/// inputs before delegating here. Implementations may therefore assume:
/// - Names are non-empty and within [`SecretHandle::MAX_NAME_LEN`]
/// - Names contain no control characters
#[async_trait(?Send)]
pub trait SecretStore: Send + Sync {
    /// Retrieve a secret as raw bytes. Returns `Ok(None)` if not found.
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError>;
}

// ---------------------------------------------------------------------------
// Test-only no-op store
// ---------------------------------------------------------------------------

/// A no-op [`SecretStore`] for tests that only need a [`SecretHandle`] to exist.
///
/// All reads return `None`.
///
/// Available in `#[cfg(test)]` builds and via the `test-utils` feature:
/// ```toml
/// [dev-dependencies]
/// edgezero-core = { path = "...", features = ["test-utils"] }
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct NoopSecretStore;

#[cfg(any(test, feature = "test-utils"))]
#[async_trait(?Send)]
impl SecretStore for NoopSecretStore {
    async fn get_bytes(&self, _name: &str) -> Result<Option<Bytes>, SecretError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// In-memory store (test-utils)
// ---------------------------------------------------------------------------

/// An in-memory [`SecretStore`] pre-populated with known secrets.
///
/// Useful for contract tests and unit tests that need deterministic secret values.
///
/// Available in `#[cfg(test)]` builds and via the `test-utils` feature.
#[cfg(any(test, feature = "test-utils"))]
pub struct InMemorySecretStore {
    secrets: std::collections::HashMap<String, Bytes>,
}

#[cfg(any(test, feature = "test-utils"))]
impl InMemorySecretStore {
    pub fn new(
        entries: impl IntoIterator<Item = (impl Into<String>, impl Into<Bytes>)>,
    ) -> Self {
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
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        Ok(self.secrets.get(name).cloned())
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A cloneable, ergonomic handle to a secret store.
///
/// Provides typed helpers (`get_str`, `require_bytes`, `require_str`)
/// while delegating to the object-safe `SecretStore` trait underneath.
#[derive(Clone)]
pub struct SecretHandle {
    store: Arc<dyn SecretStore>,
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretHandle").finish_non_exhaustive()
    }
}

impl SecretHandle {
    /// Maximum secret name length in bytes.
    pub const MAX_NAME_LEN: usize = 512;

    /// Create a new handle wrapping a secret store implementation.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }

    fn validate_name(name: &str) -> Result<(), SecretError> {
        if name.is_empty() {
            return Err(SecretError::Validation("secret name cannot be empty".to_string()));
        }
        if name.len() > Self::MAX_NAME_LEN {
            return Err(SecretError::Validation(format!(
                "secret name length {} exceeds limit of {} bytes",
                name.len(),
                Self::MAX_NAME_LEN
            )));
        }
        if name.chars().any(|c| c.is_control()) {
            return Err(SecretError::Validation(
                "secret name contains invalid control characters".to_string(),
            ));
        }
        Ok(())
    }

    /// Retrieve a secret as raw bytes. Returns `Ok(None)` if not found.
    pub async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        Self::validate_name(name)?;
        self.store.get_bytes(name).await
    }

    /// Retrieve a secret as a UTF-8 string. Returns `Ok(None)` if not found.
    pub async fn get_str(&self, name: &str) -> Result<Option<String>, SecretError> {
        let bytes = self.get_bytes(name).await?;
        bytes
            .map(|b| {
                String::from_utf8(b.to_vec()).map_err(|e| {
                    SecretError::Internal(anyhow::anyhow!(
                        "secret '{}' is not valid UTF-8: {e}",
                        name
                    ))
                })
            })
            .transpose()
    }

    /// Retrieve a secret as raw bytes. Returns `SecretError::NotFound` if absent.
    pub async fn require_bytes(&self, name: &str) -> Result<Bytes, SecretError> {
        self.get_bytes(name)
            .await?
            .ok_or_else(|| SecretError::NotFound { name: name.to_string() })
    }

    /// Retrieve a secret as a UTF-8 string. Returns `SecretError::NotFound` if absent.
    pub async fn require_str(&self, name: &str) -> Result<String, SecretError> {
        let bytes = self.require_bytes(name).await?;
        String::from_utf8(bytes.to_vec()).map_err(|e| {
            SecretError::Internal(anyhow::anyhow!(
                "secret '{}' is not valid UTF-8: {e}",
                name
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Contract test macro
// ---------------------------------------------------------------------------

/// Generate a suite of contract tests for any [`SecretStore`] implementation.
///
/// The factory expression must produce a store pre-populated with:
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
                let store = $factory;
                run(async {
                    let result = store.get_bytes("contract_key").await.unwrap();
                    assert_eq!(result, Some(Bytes::from("contract_value")));
                });
            }

            #[test]
            fn contract_get_second_key_returns_bytes() {
                let store = $factory;
                run(async {
                    let result = store.get_bytes("contract_key_2").await.unwrap();
                    assert_eq!(result, Some(Bytes::from("another_value")));
                });
            }

            #[test]
            fn contract_get_missing_returns_none() {
                let store = $factory;
                run(async {
                    let result = store.get_bytes("missing_key").await.unwrap();
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
    use bytes::Bytes;
    use futures::executor::block_on;

    // Test-only in-memory store
    use std::collections::HashMap;
    struct SimpleStore(HashMap<String, Bytes>);
    #[async_trait(?Send)]
    impl SecretStore for SimpleStore {
        async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
            Ok(self.0.get(name).cloned())
        }
    }

    fn store_with(entries: &[(&str, &str)]) -> SecretHandle {
        let map: HashMap<String, Bytes> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), Bytes::from(v.to_string())))
            .collect();
        SecretHandle::new(std::sync::Arc::new(SimpleStore(map)))
    }

    #[test]
    fn validate_name_rejects_empty() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.get_bytes("").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn validate_name_rejects_control_chars() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.get_bytes("bad\x00name").await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn validate_name_rejects_oversized() {
        block_on(async {
            let h = store_with(&[]);
            let name = "x".repeat(SecretHandle::MAX_NAME_LEN + 1);
            let err = h.get_bytes(&name).await.unwrap_err();
            assert!(matches!(err, SecretError::Validation(_)));
        });
    }

    #[test]
    fn get_bytes_returns_none_for_missing() {
        block_on(async {
            let h = store_with(&[]);
            assert_eq!(h.get_bytes("missing").await.unwrap(), None);
        });
    }

    #[test]
    fn get_bytes_returns_value_for_existing() {
        block_on(async {
            let h = store_with(&[("api_key", "secret123")]);
            assert_eq!(
                h.get_bytes("api_key").await.unwrap(),
                Some(Bytes::from("secret123"))
            );
        });
    }

    #[test]
    fn get_str_decodes_utf8() {
        block_on(async {
            let h = store_with(&[("token", "bearer xyz")]);
            assert_eq!(
                h.get_str("token").await.unwrap(),
                Some("bearer xyz".to_string())
            );
        });
    }

    #[test]
    fn require_bytes_fails_for_missing() {
        block_on(async {
            let h = store_with(&[]);
            let err = h.require_bytes("missing").await.unwrap_err();
            assert!(matches!(err, SecretError::NotFound { .. }));
        });
    }

    #[test]
    fn require_str_returns_value() {
        block_on(async {
            let h = store_with(&[("key", "value")]);
            assert_eq!(h.require_str("key").await.unwrap(), "value");
        });
    }
}
