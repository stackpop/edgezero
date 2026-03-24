//! Fastly SecretStore adapter.
//!
//! Wraps `fastly::secret_store::SecretStore` to implement
//! `edgezero_core::secret_store::SecretStore`.

#[cfg(feature = "fastly")]
use async_trait::async_trait;
#[cfg(feature = "fastly")]
use bytes::Bytes;
#[cfg(feature = "fastly")]
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store backed by Fastly's SecretStore API.
#[cfg(feature = "fastly")]
pub struct FastlySecretStore {
    store: fastly::secret_store::SecretStore,
}

#[cfg(feature = "fastly")]
impl FastlySecretStore {
    /// Open a Fastly SecretStore by name.
    ///
    /// Returns `SecretError::Internal` if the store does not exist or cannot
    /// be opened. Unlike `KVStore::open`, the Fastly SecretStore API returns
    /// `Result<Self, OpenError>` (not `Result<Option<Self>, _>`), so there
    /// is no `ok_or` unwrap here.
    pub fn open(name: &str) -> Result<Self, SecretError> {
        let store = fastly::secret_store::SecretStore::open(name).map_err(|e| {
            SecretError::Internal(anyhow::anyhow!(
                "failed to open secret store '{}': {e}",
                name
            ))
        })?;
        Ok(Self { store })
    }
}

#[cfg(feature = "fastly")]
#[async_trait(?Send)]
impl SecretStore for FastlySecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        match self.store.get(name) {
            Some(secret) => Ok(Some(secret.plaintext())),
            None => Ok(None),
        }
    }
}

// TODO: integration tests require the Fastly compute environment.
// Test `FastlySecretStore` as part of the Fastly adapter E2E test suite.
