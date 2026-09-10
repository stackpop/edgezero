//! Environment variable secret store for local development.
//!
//! Reads secrets from the process environment. Set secrets as environment
//! variables before starting the dev server:
//!
//! ```bash
//! API_KEY=mysecret edgezero serve --adapter axum
//! ```

use std::env;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore};
use edgezero_core::{BoundedStoreRead, Deadline};

/// Secret store for local development that reads secrets from environment variables.
///
/// When `[stores.secrets]` is declared in `edgezero.toml`, the dev server
/// creates an `EnvSecretStore` that reads secrets from the process environment.
pub struct EnvSecretStore;

impl EnvSecretStore {
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnvSecretStore {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for EnvSecretStore {
    #[inline]
    async fn get_bytes(&self, _store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;

            match env::var_os(key) {
                Some(value) => Ok(Some(Bytes::from(value.into_vec()))),
                None => Ok(None),
            }
        }

        #[cfg(not(unix))]
        {
            use std::env::VarError;

            match env::var(key) {
                Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
                Err(VarError::NotPresent) => Ok(None),
                Err(VarError::NotUnicode(_)) => Err(SecretError::Internal(anyhow::anyhow!(
                    "secret store returned an invalid Unicode value"
                ))),
            }
        }
    }

    #[inline]
    async fn get_bytes_bounded(
        &self,
        _store_name: &str,
        key: &str,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
        if deadline.is_expired() {
            return Err(SecretError::DeadlineExceeded);
        }

        #[cfg(unix)]
        let stored_value = env::var_os(key);

        #[cfg(not(unix))]
        let stored_value = env::var(key);

        if deadline.is_expired() {
            return Err(SecretError::DeadlineExceeded);
        }

        #[cfg(not(unix))]
        let stored_value = match stored_value {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(SecretError::Internal(anyhow::anyhow!(
                    "secret store returned an invalid Unicode value"
                )));
            }
        };

        #[cfg(unix)]
        let backend_bytes = {
            use std::os::unix::ffi::OsStrExt as _;

            stored_value.as_ref().map_or(Ok(0_u64), |value| {
                u64::try_from(value.as_os_str().as_bytes().len())
                    .map_err(|_length_error| SecretError::ValueTooLarge)
            })?
        };

        #[cfg(not(unix))]
        let backend_bytes = stored_value.as_ref().map_or(Ok(0_u64), |value| {
            u64::try_from(value.len()).map_err(|_length_error| SecretError::ValueTooLarge)
        })?;

        if backend_bytes > max_backend_bytes || backend_bytes > max_value_bytes {
            return Err(SecretError::ValueTooLarge);
        }

        #[cfg(unix)]
        let value = {
            use std::os::unix::ffi::OsStringExt as _;

            stored_value.map(|value| Bytes::from(value.into_vec()))
        };

        #[cfg(not(unix))]
        let value = stored_value.map(|value| Bytes::from(value.into_bytes()));

        if deadline.is_expired() {
            return Err(SecretError::DeadlineExceeded);
        }

        Ok(BoundedStoreRead {
            backend_bytes,
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    // Contract tests: use InMemorySecretStoreProvider since EnvSecretStore needs
    // real env vars, which are unsafe in parallel tests.
    // The EnvSecretStore is tested individually above.
    secret_store_contract_tests!(env_secret_contract, {
        InMemorySecretStore::new([
            ("mystore/contract_key", Bytes::from("contract_value")),
            ("mystore/contract_key_2", Bytes::from("another_value")),
        ])
    });

    use super::*;
    use crate::test_utils::env_guard;
    use bytes::Bytes;
    use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
    use edgezero_core::secret_store_contract_tests;
    use edgezero_core::test_env::EnvOverride;
    use edgezero_core::{Deadline, MonotonicInstant};
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_get_bytes_accepts_exact_caps_and_reports_exact_backend_bytes() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set("__EDGEZERO_TEST_BOUNDED_SECRET__", "hello");
        let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));

        let read = handle
            .get_bytes_bounded(
                "env",
                "__EDGEZERO_TEST_BOUNDED_SECRET__",
                Deadline::after(Duration::from_secs(1)),
                5,
                5,
            )
            .await
            .expect("exact cap must succeed");

        assert_eq!(read.backend_bytes, 5);
        assert_eq!(read.value, Some(Bytes::from_static(b"hello")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_get_bytes_rejects_either_cap() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set("__EDGEZERO_TEST_OVERSIZE_SECRET__", "hello");
        let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));

        for (max_backend_bytes, max_value_bytes) in [(4, 5), (5, 4)] {
            let error = handle
                .get_bytes_bounded(
                    "env",
                    "__EDGEZERO_TEST_OVERSIZE_SECRET__",
                    Deadline::after(Duration::from_secs(1)),
                    max_backend_bytes,
                    max_value_bytes,
                )
                .await
                .expect_err("over-cap secret must fail");

            assert!(matches!(error, SecretError::ValueTooLarge));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_get_bytes_rejects_an_expired_deadline() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set("__EDGEZERO_TEST_EXPIRED_SECRET__", "hello");
        let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));
        let expired = Deadline::at_instant(MonotonicInstant::now());

        let error = handle
            .get_bytes_bounded("env", "__EDGEZERO_TEST_EXPIRED_SECRET__", expired, 5, 5)
            .await
            .expect_err("expired deadline must fail");

        assert!(matches!(error, SecretError::DeadlineExceeded));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_get_bytes_preserves_handle_name_validation() {
        let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));

        let error = handle
            .get_bytes_bounded("", "key", Deadline::after(Duration::from_secs(1)), 5, 5)
            .await
            .expect_err("empty store name must be rejected");

        assert!(matches!(error, SecretError::Validation(_)));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_preserves_non_utf8_secret_values() {
        use std::os::unix::ffi::OsStringExt as _;

        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set(
            "__EDGEZERO_TEST_BINARY_SECRET__",
            OsString::from_vec(vec![0xff, 0x61]),
        );
        let store = EnvSecretStore::new();
        let result = store
            .get_bytes("env", "__EDGEZERO_TEST_BINARY_SECRET__")
            .await
            .unwrap();
        assert_eq!(result, Some(Bytes::from_static(&[0xff, 0x61])));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_returns_none_when_var_not_set() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::remove("__EDGEZERO_TEST_MISSING_VAR_XYZ__");
        let store = EnvSecretStore::new();
        let result = store
            .get_bytes("env", "__EDGEZERO_TEST_MISSING_VAR_XYZ__")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_returns_value_when_var_set() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set("__EDGEZERO_TEST_SECRET__", "test_value_123");
        let store = EnvSecretStore::new();
        let result = store
            .get_bytes("env", "__EDGEZERO_TEST_SECRET__")
            .await
            .unwrap();
        assert_eq!(result, Some(Bytes::from("test_value_123")));
    }
}
