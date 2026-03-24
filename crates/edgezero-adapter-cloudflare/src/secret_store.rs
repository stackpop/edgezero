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

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use async_trait::async_trait;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::secret_store::{SecretError, SecretStore};
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
    pub fn from_env(env: worker::Env) -> Self {
        Self { env }
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SecretStore for CloudflareSecretStore {
    async fn get_bytes(&self, name: &str) -> Result<Option<Bytes>, SecretError> {
        match self.env.secret(name) {
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
}
