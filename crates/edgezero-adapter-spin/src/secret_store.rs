//! Spin adapter secret store: wraps `spin_sdk::variables`.
//!
//! Spin's variable namespace is flat — there is no concept of named stores.
//! The `store_name` parameter is intentionally ignored; provision secrets as
//! application variables in `spin.toml`.

use std::future::Future;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::config_store::BoundedStoreRead;
use edgezero_core::secret_store::{SecretError, SecretStore};
use edgezero_core::time::Deadline;

/// Secret store backed by Spin component variables.
///
/// `store_name` is ignored — Spin's variable namespace is flat.
/// Provision secrets as application variables in `spin.toml`.
pub struct SpinSecretStore;

impl SpinSecretStore {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for SpinSecretStore {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl SecretStore for SpinSecretStore {
    #[inline]
    async fn get_bytes(&self, store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        use spin_sdk::variables;
        if !store_name.is_empty() {
            // Spin's variable namespace is flat; named stores are not supported.
            log::debug!(
                "SpinSecretStore: store_name {store_name:?} is ignored; \
                 Spin uses a single flat variable namespace"
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
            Err(err) => Err(SecretError::Internal(anyhow::anyhow!(
                "secret lookup failed: {err}"
            ))),
        }
    }

    #[inline]
    async fn get_bytes_bounded(
        &self,
        store_name: &str,
        key: &str,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
        bounded_secret_read(
            self.get_bytes(store_name, key),
            deadline,
            max_backend_bytes,
            max_value_bytes,
        )
        .await
    }
}

// Spin Variables returns a complete value, so these bounds are cooperative
// and apply immediately after host materialization rather than during allocation.
pub(crate) async fn bounded_secret_read<F>(
    read: F,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
) -> Result<BoundedStoreRead<Bytes>, SecretError>
where
    F: Future<Output = Result<Option<Bytes>, SecretError>>,
{
    if deadline.is_expired() {
        return Err(SecretError::DeadlineExceeded);
    }

    let result = read.await;
    if deadline.is_expired() {
        drop(result);
        return Err(SecretError::DeadlineExceeded);
    }
    let value = result?;

    let backend_bytes = value.as_ref().map_or(Ok(0_u64), |stored_value| {
        u64::try_from(stored_value.len()).map_err(|_length_error| SecretError::ValueTooLarge)
    })?;
    if backend_bytes > max_backend_bytes || backend_bytes > max_value_bytes {
        drop(value);
        return Err(SecretError::ValueTooLarge);
    }

    Ok(BoundedStoreRead {
        backend_bytes,
        value,
    })
}

// TODO: integration tests require the Spin runtime.
// Test SpinSecretStore as part of a Spin E2E test suite.
