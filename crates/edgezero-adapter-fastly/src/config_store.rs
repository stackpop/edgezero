//! Fastly adapter config store: wraps `fastly::ConfigStore`.

#[cfg(test)]
use std::collections::HashMap;

use crate::chunked_config::resolve_fastly_config_value;
use async_trait::async_trait;
use edgezero_core::config_store::{ConfigStore, ConfigStoreError};
use fastly::ConfigStore as FastlyConfigStoreInner;
use fastly::config_store::{LookupError, OpenError};

/// Config store backed by a Fastly Config Store resource link.
pub struct FastlyConfigStore {
    inner: FastlyConfigStoreBackend,
}

enum FastlyConfigStoreBackend {
    Fastly(FastlyConfigStoreInner),
    #[cfg(test)]
    InMemory(HashMap<String, String>),
}

impl FastlyConfigStore {
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: FastlyConfigStoreBackend::InMemory(entries.into_iter().collect()),
        }
    }

    /// Synchronous key lookup used by the chunk-pointer resolver callback.
    /// Returns `Ok(Some(value))`, `Ok(None)` (missing), or `Err(message)`.
    fn get_sync(&self, key: &str) -> Result<Option<String>, String> {
        match &self.inner {
            FastlyConfigStoreBackend::Fastly(inner) => inner.try_get(key).map_err(|err| {
                // The `key` here is a pointer-controlled chunk key; the resolver
                // adds a safe position locator, so it is not echoed. The platform
                // `err` is a fastly SDK type that does not embed the stored value.
                format!("config store lookup failed: {err}")
            }),
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => Ok(data.get(key).cloned()),
        }
    }

    /// Open a Fastly Config Store by resource link name.
    ///
    /// Returns an error if the configured store cannot be opened.
    ///
    /// # Errors
    /// Returns the underlying [`fastly::config_store::OpenError`] when the named store does not exist or cannot be opened.
    #[inline]
    pub fn try_open(name: &str) -> Result<Self, OpenError> {
        FastlyConfigStoreInner::try_open(name).map(|inner| Self {
            inner: FastlyConfigStoreBackend::Fastly(inner),
        })
    }
}

#[async_trait(?Send)]
impl ConfigStore for FastlyConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        let root_value = match &self.inner {
            FastlyConfigStoreBackend::Fastly(inner) => {
                inner.try_get(key).map_err(|err| map_lookup_error(&err))?
            }
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => data.get(key).cloned(),
        };
        let Some(value) = root_value else {
            return Ok(None);
        };
        // Resolve chunk pointers transparently. Direct BlobEnvelope values
        // pass through unchanged; pointer values fan out to chunk entries
        // in the same store. Missing / malformed / hash-mismatched chunks
        // are corrupt platform state — spec 9.3 (line 6272) calls this an
        // internal config-store error with re-push remediation, NOT a
        // transient unavailable. Mapping to `internal` surfaces as HTTP
        // 500 and pushes operators toward `<app-cli> config push` instead
        // of waiting for a 503 to clear.
        let resolved = resolve_fastly_config_value(key, value, |chunk_key| {
            self.get_sync(chunk_key)
        })
        .map_err(|err| {
            log::warn!(
                "Fastly config-store chunk resolution failed for `{key}`: {err}. \
                     Re-run `<app-cli> config push` to repair the store."
            );
            ConfigStoreError::internal(anyhow::anyhow!(
                "config store entry is corrupt or incomplete; re-run config push to repair: {err}"
            ))
        })?;
        Ok(Some(resolved))
    }
}

fn map_lookup_error(err: &LookupError) -> ConfigStoreError {
    // `LookupError` is from the `fastly` crate; using a wildcard arm guards
    // against new variants being added in upstream point releases without
    // forcing us into a breaking match every bump.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "external enum; new variants must remain unavailable→unavailable"
    )]
    match err {
        LookupError::KeyInvalid | LookupError::KeyTooLong => {
            ConfigStoreError::invalid_key("invalid config key")
        }
        _ => {
            log::warn!("Fastly config store lookup failed: {err}");
            ConfigStoreError::unavailable("config store temporarily unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    edgezero_core::config_store_contract_tests!(fastly_config_store_contract, {
        FastlyConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });

    #[test]
    fn key_invalid_maps_to_invalid_key_error() {
        let err = map_lookup_error(&LookupError::KeyInvalid);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }

    #[test]
    fn key_too_long_maps_to_invalid_key_error() {
        let err = map_lookup_error(&LookupError::KeyTooLong);
        assert!(matches!(err, ConfigStoreError::InvalidKey { .. }));
    }

    /// Spec 9.3 (line 6272): missing chunks, hash mismatches, pointer
    /// parse failures, and full-envelope mismatches are CORRUPT PLATFORM
    /// STATE — the runtime returns an internal config-store error with
    /// re-push remediation, NOT a transient `Unavailable` (which would
    /// surface as HTTP 503 and invite operators to wait it out).
    #[test]
    fn corrupt_chunk_pointer_maps_to_internal_not_unavailable() {
        use futures::executor::block_on;
        // A root value that ANNOUNCES our chunk-pointer kind but is malformed.
        // It must be a pointer-kind value: an unrelated raw value is a
        // legitimate Config Store entry and passes through untouched, so it
        // would not exercise the corruption path at all.
        let store = FastlyConfigStore::from_entries([(
            "app_config".to_owned(),
            r#"{"edgezero_kind":"fastly_config_chunks"}"#.to_owned(),
        )]);
        let err = block_on(store.get("app_config"))
            .expect_err("corrupt root must map to a ConfigStoreError");
        assert!(
            matches!(err, ConfigStoreError::Internal { .. }),
            "corrupt platform state must be Internal (not Unavailable / not InvalidKey): {err:?}"
        );
        assert!(
            err.to_string()
                .to_lowercase()
                .contains("re-run config push")
                || err.to_string().to_lowercase().contains("corrupt"),
            "error message must point operators at the remediation: {err}"
        );
    }
}
