//! Environment variable secret store for local development.
//!
//! Reads secrets from `std::env::var(name)`. Set secrets as environment
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
        match std::env::var(name) {
            Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(os_str)) => Err(SecretError::Internal(
                anyhow::anyhow!("secret '{}' contains non-UTF-8 bytes: {:?}", name, os_str),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::executor::block_on;

    #[test]
    fn get_bytes_returns_none_when_var_not_set() {
        let store = EnvSecretStore::new();
        let result = block_on(store.get_bytes("__EDGEZERO_TEST_MISSING_VAR_XYZ__")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_bytes_returns_value_when_var_set() {
        std::env::set_var("__EDGEZERO_TEST_SECRET__", "test_value_123");
        let store = EnvSecretStore::new();
        let result = block_on(store.get_bytes("__EDGEZERO_TEST_SECRET__")).unwrap();
        assert_eq!(result, Some(Bytes::from("test_value_123")));
        std::env::remove_var("__EDGEZERO_TEST_SECRET__");
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
