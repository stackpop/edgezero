//! Fastly secret store adapter.
//!
//! Implements `edgezero_core::secret_store::SecretStore` via
//! `FastlySecretStore`, which opens a named Fastly `SecretStore` on
//! each lookup.

use crate::chunked_config::{SyncHostCallError, exact_fastly_read, run_sync_host_call};
use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::config_store::BoundedStoreRead;
use edgezero_core::secret_store::{SecretError, SecretStore};
use edgezero_core::{Deadline, MonotonicClock};
use fastly::secret_store::SecretStore as FastlyNativeSecretStore;

/// Internal helper that opens a single named Fastly `SecretStore`.
pub struct FastlyNamedStore {
    store: FastlyNativeSecretStore,
}

impl FastlyNamedStore {
    fn get_bytes_bounded_sync(
        &self,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
        let lookup = run_sync_host_call(clock, deadline, || {
            self.store.try_get(key).map_err(|err| {
                SecretError::Internal(anyhow::anyhow!("secret lookup failed: {err}"))
            })
        })
        .map_err(map_sync_secret_error)?;

        let Some(secret) = lookup else {
            return Ok(BoundedStoreRead {
                backend_bytes: 0,
                value: None,
            });
        };
        let plaintext = run_sync_host_call(clock, deadline, || {
            secret.try_plaintext().map_err(|err| {
                SecretError::Internal(anyhow::anyhow!("secret decryption failed: {err}"))
            })
        })
        .map_err(map_sync_secret_error)?;

        exact_fastly_read(Some(plaintext), max_backend_bytes.min(max_value_bytes))
            .map_err(|_size_error| SecretError::ValueTooLarge)
    }

    pub(crate) fn get_bytes_sync(&self, key: &str) -> Result<Option<Bytes>, SecretError> {
        let lookup = self
            .store
            .try_get(key)
            .map_err(|err| SecretError::Internal(anyhow::anyhow!("secret lookup failed: {err}")))?;

        match lookup {
            Some(secret) => secret.try_plaintext().map(Some).map_err(|err| {
                SecretError::Internal(anyhow::anyhow!("secret decryption failed: {err}"))
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
    #[inline]
    pub fn open(name: &str) -> Result<Self, SecretError> {
        let store = FastlyNativeSecretStore::open(name).map_err(|err| {
            SecretError::Internal(anyhow::anyhow!(
                "failed to open secret store '{name}': {err}"
            ))
        })?;
        Ok(Self { store })
    }

    fn open_bounded(
        name: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
    ) -> Result<Self, SecretError> {
        run_sync_host_call(clock, deadline, || Self::open(name)).map_err(map_sync_secret_error)
    }
}

/// Multi-store provider backed by Fastly's `SecretStore` API.
///
/// Opens the named store per call — `FastlyNamedStore::open` is cheap
/// (no network; just a handle) so there is no caching.
pub struct FastlySecretStore;

#[async_trait(?Send)]
impl SecretStore for FastlySecretStore {
    #[inline]
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        let store = FastlyNamedStore::open(store_name)?;
        store.get_bytes_sync(key)
    }

    #[inline]
    async fn get_bytes_bounded(
        &self,
        store_name: &str,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
        let store = FastlyNamedStore::open_bounded(store_name, clock, deadline)?;
        store.get_bytes_bounded_sync(key, clock, deadline, max_backend_bytes, max_value_bytes)
    }
}

fn map_sync_secret_error(call_error: SyncHostCallError<SecretError>) -> SecretError {
    match call_error {
        SyncHostCallError::Backend(backend_error) => backend_error,
        SyncHostCallError::DeadlineExceeded => SecretError::DeadlineExceeded,
    }
}

// TODO: integration tests require the Fastly compute environment.
// Test `FastlyNamedStore` and `FastlySecretStore` as part of the
// Fastly adapter E2E test suite.

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use edgezero_core::{Deadline, MonotonicClock, MonotonicInstant};

    use super::*;

    #[test]
    fn bounded_secret_read_uses_injected_clock() {
        let process_now = MonotonicInstant::now();
        let injected_now = process_now
            .checked_sub(Duration::from_mins(1))
            .expect("injected instant");
        let clock = MonotonicClock::new(move || injected_now);
        let deadline = Deadline::at_instant(
            injected_now
                .checked_add(Duration::from_secs(1))
                .expect("deadline instant"),
        );
        let value = run_sync_host_call(&clock, deadline, || Ok::<_, SecretError>("value"))
            .expect("the application clock is still before its deadline");
        assert_eq!(value, "value");

        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let advancing_clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = run_sync_host_call(&advancing_clock, Deadline::at_instant(terminal), || {
            *now.lock().expect("clock lock") = terminal;
            Err::<(), _>(SecretError::Unavailable)
        })
        .expect_err("equality expiry must beat a ready provider error");
        assert!(matches!(error, SyncHostCallError::DeadlineExceeded));
    }
}
