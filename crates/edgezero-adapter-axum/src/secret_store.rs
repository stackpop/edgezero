//! Environment variable secret store for local development.
//!
//! Reads secrets from the process environment. Set secrets as environment
//! variables before starting the dev server:
//!
//! ```bash
//! API_KEY=mysecret cargo edgezero dev
//! ```

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store for local development that reads secrets from environment variables.
///
/// When `[stores.secrets]` is declared in `edgezero.toml`, the dev server
/// creates an `EnvSecretStore` that reads secrets from the process environment.
pub struct EnvSecretStore;

impl EnvSecretStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnvSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for EnvSecretStore {
    async fn get_bytes(&self, _store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;

            match std::env::var_os(key) {
                Some(value) => Ok(Some(Bytes::from(value.into_vec()))),
                None => Ok(None),
            }
        }

        #[cfg(not(unix))]
        {
            match std::env::var(key) {
                Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::Internal(
                    anyhow::anyhow!("secret store returned an invalid Unicode value"),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{env_guard, EnvOverride};
    use bytes::Bytes;
    #[cfg(unix)]
    use std::ffi::OsString;

    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_returns_none_when_var_not_set() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::clear("__EDGEZERO_TEST_MISSING_VAR_XYZ__");
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

    // Contract tests: use InMemorySecretStoreProvider since EnvSecretStore needs
    // real env vars, which are unsafe in parallel tests.
    // The EnvSecretStore is tested individually above.
    use edgezero_core::secret_store_contract_tests;

    secret_store_contract_tests!(env_secret_contract, {
        edgezero_core::InMemorySecretStore::new([
            ("mystore/contract_key", Bytes::from("contract_value")),
            ("mystore/contract_key_2", Bytes::from("another_value")),
        ])
    });
}
