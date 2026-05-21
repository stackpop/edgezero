//! Spin adapter secret store: wraps `spin_sdk::variables`.
//!
//! Spin's variable namespace is flat — there is no concept of named stores.
//! The `store_name` parameter is intentionally ignored; provision secrets as
//! application variables in `spin.toml`.

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use async_trait::async_trait;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::secret_store::{SecretError, SecretStore};

/// Secret store backed by Spin component variables.
///
/// `store_name` is ignored — Spin's variable namespace is flat.
/// Provision secrets as application variables in `spin.toml`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub struct SpinSecretStore;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl SpinSecretStore {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl Default for SpinSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
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
        match variables::get(&lower) {
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
