//! Cloudflare Workers secret adapter.
//!
//! Reads secrets from `worker::Env::secret()`. Each call to `get_bytes(name)`
//! invokes `env.secret(name)` to retrieve the value. The `Env` is cloned at
//! dispatch time to outlive `into_core_request`'s ownership of the original.
//!
//! Note: Cloudflare Workers Secrets have no namespace concept — each secret
//! is an individual `[vars]` / Secrets binding in `wrangler.toml`. The
//! `[stores.secrets] name` in `edgezero.toml` is used only for Fastly;
//! Cloudflare accesses all secrets via this adapter regardless of name.

use std::future::Future;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::config_store::BoundedStoreRead;
use edgezero_core::secret_store::SecretError;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::secret_store::SecretStore;
use edgezero_core::time::Deadline;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::Error as WorkerError;

/// Secret store backed by Cloudflare Workers `Env`.
///
/// Reads secrets via `env.secret(name)`. Clones the `Env` handle at dispatch
/// time so secrets remain accessible throughout the request lifetime.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub struct CloudflareSecretStore {
    env: worker::Env,
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl CloudflareSecretStore {
    /// Create a secret store from a cloned `Env`.
    #[inline]
    #[must_use]
    pub fn from_env(env: worker::Env) -> Self {
        Self { env }
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SecretStore for CloudflareSecretStore {
    #[inline]
    async fn get_bytes(&self, _store_name: &str, key: &str) -> Result<Option<Bytes>, SecretError> {
        match self.env.secret(key) {
            Ok(secret) => {
                let value = secret.to_string();
                Ok(Some(Bytes::from(value.into_bytes())))
            }
            Err(WorkerError::BindingError(_)) => Ok(None),
            Err(WorkerError::JsError(message))
                if message.contains("does not contain binding")
                    || message.contains("is undefined") =>
            {
                Ok(None)
            }
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

// Workers Secrets returns a complete value, so these bounds are cooperative
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
