//! Spin adapter secret store: wraps `spin_sdk::variables`.
//!
//! Spin's variable namespace is flat — there is no concept of named stores.
//! The `store_name` parameter is intentionally ignored; provision secrets as
//! application variables in `spin.toml`.

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store backed by Spin component variables.
///
/// `store_name` is ignored — Spin's variable namespace is flat.
/// Provision secrets as application variables in `spin.toml`.
pub struct SpinSecretStore;

impl SpinSecretStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SpinSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for SpinSecretStore {
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        use spin_sdk::variables;
        if !store_name.is_empty() {
            // Spin's variable namespace is flat; named stores are not supported.
            log::debug!(
                "SpinSecretStore: store_name {:?} is ignored; \
                 Spin uses a single flat variable namespace",
                store_name
            );
        }
        // Spin variable names must be lowercase. Normalise via ascii_lowercase
        // so that SCREAMING_SNAKE_CASE keys (e.g. "STRIPE_KEY" → "stripe_key")
        // work without callers knowing the Spin convention. Note: only
        // UPPER_SNAKE → lower_snake is safe; camelCase or mixed-case keys will
        // be lowercased in a way that may not match any declared variable
        // (e.g. "stripeKey" → "stripekey"). Document accepted key formats at
        // the call site.
        let lower = key.to_ascii_lowercase();
        match variables::get(&lower).await {
            Ok(value) => Ok(Some(Bytes::from(value.into_bytes()))),
            Err(variables::Error::Undefined(_)) => Ok(None),
            Err(variables::Error::InvalidName(msg)) => Err(SecretError::Validation(msg)),
            Err(e) => Err(SecretError::Internal(anyhow::anyhow!(
                "secret lookup failed: {e}"
            ))),
        }
    }
}

// TODO: integration tests require the Spin runtime.
// Test SpinSecretStore as part of a Spin E2E test suite.
