//! Fastly adapter config store: wraps `fastly::ConfigStore`.

use std::cell::Cell;
#[cfg(test)]
use std::collections::HashMap;

use crate::chunked_config::resolve_fastly_config_value_typed;
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
        // Resolve chunk pointers transparently. Direct BlobEnvelope values and
        // any other raw value pass through; pointer values fan out to chunk
        // entries in the same store.
        //
        // A chunk fetch can fail in two classes that must NOT collapse to one
        // status:
        //   - Corrupt state (a hash mismatch, a bad/oversized derived key) →
        //     Internal (HTTP 500) with re-push remediation, per spec 9.3.
        //     Re-pushing rewrites the generation and fixes it.
        //   - Transient (invalid store handle, lookup exhaustion, an unclassified
        //     error, a value that outgrew the read buffer, OR a referenced chunk
        //     not yet visible at this POP) → Unavailable (HTTP 503, retryable).
        //
        // A MISSING referenced chunk is deliberately transient. Config Store is
        // eventually consistent ACROSS keys, so right after a push the flipped
        // root pointer can be visible at a POP before all of its
        // content-addressed chunks have propagated there. That gap is not
        // corruption — a retry moments later resolves it — so it must read as 503,
        // not a re-push-me 500. (Genuine, lasting corruption then shows as a
        // persistent 503 the operator repairs by re-pushing; that is strictly
        // safer than a spurious 500 during the normal propagation window.)
        // `transient` records whether any chunk fetch hit that class so the outer
        // result can pick the right status.
        //
        // A DIRECT value is returned VERBATIM (the resolver only touches our chunk
        // pointers): the store layer must not parse or judge arbitrary values --
        // that is the typed app-config extractor's job, which gives the
        // upgrade/redeploy remediation for a newer DIRECT envelope. Only a value
        // the resolver actually processes (an unknown `edgezero_kind`, or a
        // future pointer/inner-envelope version -- typed `FutureFormat`) is handled
        // here, because those ARE our reserved namespace.
        let transient = Cell::new(false);
        let outcome = resolve_fastly_config_value_typed(key, value, |chunk_key| {
            let got = match &self.inner {
                FastlyConfigStoreBackend::Fastly(inner) => {
                    inner.try_get(chunk_key).map_err(|err| {
                        if is_transient_lookup(&err) {
                            transient.set(true);
                        }
                        // The pointer-controlled chunk key is not echoed (the resolver
                        // adds a safe position locator); the SDK `err` carries no value.
                        format!("config store lookup failed: {err}")
                    })?
                }
                #[cfg(test)]
                FastlyConfigStoreBackend::InMemory(data) => data.get(chunk_key).cloned(),
            };
            if got.is_none() {
                // Referenced chunk absent at this POP: treat as propagation lag
                // (transient) rather than corruption.
                transient.set(true);
            }
            Ok(got)
        });
        match outcome {
            Ok(resolved) => Ok(Some(resolved)),
            Err(err) if err.is_future_format() => {
                // A NEWER format in OUR reserved namespace (an unknown
                // `edgezero_kind`, or a future pointer/inner-envelope version):
                // re-pushing the same config will not help; the deployed build
                // must be UPGRADED.
                log::warn!(
                    "Fastly config-store value for `{key}` uses a NEWER format than this build \
                     understands: {}. Re-pushing the same config will not help -- redeploy this \
                     service with an updated EdgeZero build.",
                    err.into_message()
                );
                Err(ConfigStoreError::internal(anyhow::anyhow!(
                    "config store value uses a newer format than this build understands; redeploy \
                     this service with an updated build (re-pushing will not help)"
                )))
            }
            Err(err) => {
                let message = err.into_message();
                if transient.get() {
                    log::warn!(
                        "Fastly config-store chunk lookup for `{key}` was transiently \
                         unavailable: {message}"
                    );
                    Err(ConfigStoreError::unavailable(
                        "config store temporarily unavailable",
                    ))
                } else {
                    log::warn!(
                        "Fastly config-store chunk resolution failed for `{key}`: {message}. \
                         Re-run `<app-cli> config push` to repair the store."
                    );
                    Err(ConfigStoreError::internal(anyhow::anyhow!(
                        "config store entry is corrupt or incomplete; re-run config push to \
                         repair: {message}"
                    )))
                }
            }
        }
    }
}

/// Is a CHUNK lookup failure environmental (retry) rather than corrupt config
/// (`config push` to repair)? Only a bad KEY names corrupt state a re-push
/// rewrites; everything else — an invalid store handle, lookup exhaustion, an
/// unclassified/future failure, or a value that outgrew the read buffer — is
/// transient, because re-pushing cannot fix a request-scoped condition.
fn is_transient_lookup(err: &LookupError) -> bool {
    // `ValueTooLong` is TRANSIENT, not corruption: the SDK already retried with
    // the reported buffer size, so a `ValueTooLong` reaching us means the value
    // GREW between host calls (a concurrent write) — a race a retry resolves, not
    // a re-push-me corruption.
    !matches!(err, LookupError::KeyInvalid | LookupError::KeyTooLong)
}

fn map_lookup_error(err: &LookupError) -> ConfigStoreError {
    // `LookupError` is #[non_exhaustive] on the fastly side; every current
    // variant is enumerated so a new upstream variant forces a reviewer
    // decision here rather than silently landing in the unavailable arm.
    match err {
        LookupError::KeyInvalid | LookupError::KeyTooLong => {
            ConfigStoreError::invalid_key("invalid config key")
        }
        LookupError::ConfigStoreInvalid
        | LookupError::ValueTooLong
        | LookupError::TooManyLookups
        | LookupError::Other => {
            log::warn!("Fastly config store lookup failed: {err}");
            ConfigStoreError::unavailable("config store temporarily unavailable")
        }
        _future => {
            log::warn!("Fastly config store lookup failed (unknown variant): {err}");
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

    /// A CHUNK lookup that fails transiently (lookup exhaustion, an invalid
    /// store handle, an unclassified error, or a value that outgrew the read
    /// buffer) must keep Unavailable semantics — re-pushing cannot repair a
    /// request-scoped condition. Only a bad KEY is corrupt state a re-push fixes.
    #[test]
    fn transient_chunk_lookups_are_not_treated_as_corruption() {
        assert!(is_transient_lookup(&LookupError::TooManyLookups));
        assert!(is_transient_lookup(&LookupError::ConfigStoreInvalid));
        assert!(is_transient_lookup(&LookupError::Other));
        // ValueTooLong is TRANSIENT: the SDK already retried at the reported size,
        // so it reaching us means the value grew between host calls (a race).
        assert!(is_transient_lookup(&LookupError::ValueTooLong));
        assert!(!is_transient_lookup(&LookupError::KeyInvalid));
        assert!(!is_transient_lookup(&LookupError::KeyTooLong));
    }

    /// A referenced chunk that is ABSENT maps to Unavailable (HTTP 503), not
    /// Internal. Config Store is eventually consistent across keys, so a flipped
    /// root pointer can reach a POP before all its chunks propagate there; that
    /// window is retryable, not a re-push-me corruption.
    #[test]
    fn a_missing_chunk_maps_to_unavailable_not_internal() {
        use crate::chunked_config::prepare_fastly_config_entries;
        use futures::executor::block_on;

        // A real chunked value, but seed ONLY the root pointer -- the chunks it
        // references are "not yet propagated" to this POP.
        let envelope = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            serde_json::to_string(&BlobEnvelope::new(
                json!({ "pad": "x".repeat(9_000) }),
                "2026-01-01T00:00:00Z".to_owned(),
            ))
            .expect("envelope")
        };
        let entries = prepare_fastly_config_entries("app_config", &envelope).expect("expand");
        let (root_key, pointer_json) = entries.last().expect("pointer").clone();
        let store = FastlyConfigStore::from_entries([(root_key.clone(), pointer_json)]);

        let err = block_on(store.get(&root_key)).expect_err("a missing chunk must error");
        assert!(
            matches!(err, ConfigStoreError::Unavailable { .. }),
            "a not-yet-propagated chunk must be retryable (Unavailable), not Internal: {err:?}"
        );
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

    /// A value written by a NEWER format (an unknown `edgezero_kind`, or a bumped
    /// version) must map to Internal with an UPGRADE remediation, NOT the
    /// re-push-to-repair message: re-pushing the same config cannot help a guest
    /// that is older than the config.
    #[test]
    fn future_format_value_asks_to_redeploy_not_repush() {
        use futures::executor::block_on;
        let store = FastlyConfigStore::from_entries([(
            "app_config".to_owned(),
            r#"{"edgezero_kind":"fastly_config_chunks","version":2,"chunks":[]}"#.to_owned(),
        )]);
        let err = block_on(store.get("app_config")).expect_err("a future format must error");
        assert!(
            matches!(err, ConfigStoreError::Internal { .. }),
            "a future format is Internal, not transient: {err:?}"
        );
        let message = err.to_string().to_lowercase();
        assert!(
            message.contains("redeploy") || message.contains("newer format"),
            "must ask the operator to redeploy an updated build: {err}"
        );
        assert!(
            !message.contains("re-run config push")
                && !message.contains("re-push to repair")
                && !message.contains("push to repair"),
            "must NOT instruct the operator to re-push to repair a future format: {err}"
        );
    }

    /// The store layer returns arbitrary DIRECT values VERBATIM (the shared
    /// `ConfigStore` contract): it must NOT parse or judge them -- a direct
    /// envelope from a newer writer carries no `edgezero_kind`, so the resolver
    /// passes it through as an `Ok`. Judging its version is the typed app-config
    /// extractor's job, which gives the redeploy remediation. Reverts an earlier
    /// store-layer inspection that broke the "return verbatim" contract.
    #[test]
    fn direct_future_envelope_is_returned_verbatim() {
        use futures::executor::block_on;
        // A v2 direct envelope: envelope-shaped, no `edgezero_kind`, version 2.
        let raw = r#"{"data":{"x":1},"sha256":"0000000000000000000000000000000000000000000000000000000000000000","generated_at":"2026-01-01T00:00:00Z","version":2}"#;
        let store = FastlyConfigStore::from_entries([("app_config".to_owned(), raw.to_owned())]);
        let got = block_on(store.get("app_config"))
            .expect("the store must return a direct value verbatim, not judge it");
        assert_eq!(
            got.as_deref(),
            Some(raw),
            "a direct value (even a future envelope) must be returned VERBATIM by the store layer"
        );
    }
}
