//! Fastly secret store adapter.
//!
//! Implements `edgezero_core::secret_store::SecretStore` via
//! `FastlySecretStore`, which opens a named Fastly `SecretStore` on
//! each lookup.

#[cfg(feature = "fastly")]
use async_trait::async_trait;
#[cfg(feature = "fastly")]
use bytes::Bytes;
#[cfg(feature = "fastly")]
use edgezero_core::secret_store::{SecretError, SecretStore};
#[cfg(feature = "fastly")]
use fastly::secret_store::SecretStore as FastlyNativeSecretStore;

/// Internal helper that opens a single named Fastly `SecretStore`.
#[cfg(feature = "fastly")]
pub struct FastlyNamedStore {
    store: FastlyNativeSecretStore,
}

#[cfg(feature = "fastly")]
impl FastlyNamedStore {
    pub(crate) fn get_bytes_sync(&self, key: &str) -> Result<Option<Bytes>, SecretError> {
        let lookup = self
            .store
            .try_get(key)
            .map_err(|e| SecretError::Internal(anyhow::anyhow!("secret lookup failed: {e}")))?;

        match lookup {
            Some(secret) => secret.try_plaintext().map(Some).map_err(|e| {
                SecretError::Internal(anyhow::anyhow!("secret decryption failed: {e}"))
            }),
            None => Ok(None),
        }
    }

    /// Open a Fastly `SecretStore` by name.
    ///
    /// Returns `SecretError::Internal` if the store does not exist or cannot
    /// be opened. Unlike `KVStore::open`, the Fastly `SecretStore` API returns
    /// `Result<Self, OpenError>` (not `Result<Option<Self>, _>`), so there
    /// is no `ok_or` unwrap here.
    ///
    /// # Errors
    /// Returns [`SecretError::Internal`] if the named secret store cannot be opened.
    pub fn open(name: &str) -> Result<Self, SecretError> {
        let store = FastlyNativeSecretStore::open(name).map_err(|e| {
            SecretError::Internal(anyhow::anyhow!("failed to open secret store '{name}': {e}"))
        })?;
        Ok(Self { store })
    }
}

/// Multi-store provider backed by Fastly's `SecretStore` API.
///
/// Opens the named store per call — `FastlyNamedStore::open` is cheap
/// (no network; just a handle) so there is no caching.
#[cfg(feature = "fastly")]
pub struct FastlySecretStore;

#[cfg(feature = "fastly")]
#[async_trait(?Send)]
impl SecretStore for FastlySecretStore {
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        let store = FastlyNamedStore::open(store_name)?;
        store.get_bytes_sync(key)
    }
}

// TODO: integration tests require the Fastly compute environment.
// Test `FastlyNamedStore` and `FastlySecretStore` as part of the
// Fastly adapter E2E test suite.
