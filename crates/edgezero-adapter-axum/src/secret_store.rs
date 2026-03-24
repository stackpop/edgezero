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
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            match std::env::var_os(name) {
                Some(value) => Ok(Some(Bytes::from(value.into_vec()))),
                None => Ok(None),
            }
        }

        #[cfg(not(unix))]
        {
            match std::env::var(name) {
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
    use bytes::Bytes;
    use std::ffi::OsString;
    use std::sync::OnceLock;

    fn env_guard() -> &'static tokio::sync::Mutex<()> {
        static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    struct EnvOverride {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvOverride {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }

        fn clear(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, original }
        }
    }

    impl Drop for EnvOverride {
        fn drop(&mut self) {
            if let Some(ref original) = self.original {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_returns_none_when_var_not_set() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::clear("__EDGEZERO_TEST_MISSING_VAR_XYZ__");
        let store = EnvSecretStore::new();
        let result = store
            .get_bytes("__EDGEZERO_TEST_MISSING_VAR_XYZ__")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_returns_value_when_var_set() {
        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set("__EDGEZERO_TEST_SECRET__", "test_value_123");
        let store = EnvSecretStore::new();
        let result = store.get_bytes("__EDGEZERO_TEST_SECRET__").await.unwrap();
        assert_eq!(result, Some(Bytes::from("test_value_123")));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn get_bytes_preserves_non_utf8_secret_values() {
        use std::os::unix::ffi::OsStringExt;

        let _guard = env_guard().lock().await;
        let _env = EnvOverride::set(
            "__EDGEZERO_TEST_BINARY_SECRET__",
            OsString::from_vec(vec![0xff, 0x61]),
        );
        let store = EnvSecretStore::new();
        let result = store
            .get_bytes("__EDGEZERO_TEST_BINARY_SECRET__")
            .await
            .unwrap();
        assert_eq!(result, Some(Bytes::from_static(&[0xff, 0x61])));
    }

    // Contract tests: use InMemorySecretStore since EnvSecretStore needs
    // real env vars, which are unsafe in parallel tests.
    // The EnvSecretStore is tested individually above.
    use edgezero_core::secret_store::InMemorySecretStore;
    use edgezero_core::secret_store_contract_tests;

    secret_store_contract_tests!(env_secret_contract, {
        InMemorySecretStore::new([
            ("contract_key", Bytes::from("contract_value")),
            ("contract_key_2", Bytes::from("another_value")),
        ])
    });
}
