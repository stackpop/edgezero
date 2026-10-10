//! Environment variable secret store for local development.
//!
//! Reads secret `key` from the environment variable `EDGEZERO__SECRETS__`
//! followed by `key` in ASCII uppercase. Set secrets before starting the dev
//! server:
//!
//! ```bash
//! EDGEZERO__SECRETS__API_KEY=mysecret edgezero serve --adapter axum
//! ```

use std::env;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore};

use crate::secret_env::secret_env_var;

/// Secret store for local development that reads secrets from environment variables.
///
/// When `[stores.secrets]` is declared in `edgezero.toml`, the dev server
/// creates an `EnvSecretStore` that reads secret `key` from
/// `EDGEZERO__SECRETS__<KEY>` (see [`secret_env_var`]), e.g.
/// `EDGEZERO__SECRETS__API_KEY=mysecret edgezero serve --adapter axum`.
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
        let var = secret_env_var(key).map_err(SecretError::Validation)?;

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;

            match env::var_os(&var) {
                Some(value) => Ok(Some(Bytes::from(value.into_vec()))),
                None => Ok(None),
            }
        }

        #[cfg(not(unix))]
        {
            use std::env::VarError;

            match env::var(&var) {
                Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
                Err(VarError::NotPresent) => Ok(None),
                Err(VarError::NotUnicode(_)) => Err(SecretError::Internal(anyhow::anyhow!(
                    "secret store returned an invalid Unicode value"
                ))),
            }
        }
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
    use edgezero_core::secret_store::InMemorySecretStore;
    use edgezero_core::secret_store_contract_tests;
    use edgezero_core::test_env::EnvOverride;
    use futures::executor::block_on;
    #[cfg(unix)]
    use std::ffi::OsString;

    fn read(key: &str) -> Result<Option<Bytes>, SecretError> {
        block_on(EnvSecretStore::new().get_bytes("env", key))
    }

    #[cfg(unix)]
    #[test]
    fn get_bytes_preserves_non_utf8_secret_values() {
        use std::os::unix::ffi::OsStringExt as _;

        let _guard = block_on(env_guard().lock());
        let _env = EnvOverride::set(
            "EDGEZERO__SECRETS__EDGEZERO_TEST_BINARY_SECRET",
            OsString::from_vec(vec![0xff, 0x61]),
        );
        let result = read("EDGEZERO_TEST_BINARY_SECRET").unwrap();
        assert_eq!(result, Some(Bytes::from_static(&[0xff, 0x61])));
    }

    #[test]
    fn get_bytes_returns_none_when_var_not_set() {
        let _guard = block_on(env_guard().lock());
        let _env = EnvOverride::remove("EDGEZERO__SECRETS__EDGEZERO_TEST_MISSING_SECRET");
        assert!(read("EDGEZERO_TEST_MISSING_SECRET").unwrap().is_none());
    }

    #[test]
    fn get_bytes_returns_value_when_var_set() {
        let _guard = block_on(env_guard().lock());
        let _env = EnvOverride::set("EDGEZERO__SECRETS__EDGEZERO_TEST_SECRET", "test_value_123");
        let result = read("EDGEZERO_TEST_SECRET").unwrap();
        assert_eq!(result, Some(Bytes::from("test_value_123")));
    }

    #[test]
    fn get_bytes_reads_uppercased_prefixed_var_for_lowercase_key() {
        let _guard = block_on(env_guard().lock());
        let _env = EnvOverride::set(
            "EDGEZERO__SECRETS__EDGEZERO_TEST_LOWER_SECRET",
            "lower_value",
        );
        let result = read("edgezero_test_lower_secret").unwrap();
        assert_eq!(result, Some(Bytes::from("lower_value")));
    }

    #[test]
    fn get_bytes_does_not_read_var_named_exactly_like_key() {
        let _guard = block_on(env_guard().lock());
        let _bare = EnvOverride::set("EDGEZERO_TEST_BARE_SECRET", "bare_value");
        let _prefixed = EnvOverride::remove("EDGEZERO__SECRETS__EDGEZERO_TEST_BARE_SECRET");
        assert!(read("EDGEZERO_TEST_BARE_SECRET").unwrap().is_none());
    }

    #[test]
    fn get_bytes_rejects_key_that_cannot_name_a_var() {
        let err = read("api-token").unwrap_err();
        assert!(
            matches!(&err, SecretError::Validation(msg) if msg.contains("api-token")),
            "{err:?}"
        );
    }
}
