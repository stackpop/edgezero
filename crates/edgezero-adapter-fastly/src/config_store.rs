//! Fastly config reads use one caller-sized host buffer, without SDK retries.

use std::cell::Cell;
#[cfg(test)]
use std::collections::HashMap;

use crate::chunked_config::{
    BoundedResolveFailure, ResolveFailure, SyncHostCallError, exact_fastly_read,
    resolve_fastly_config_value_typed, resolve_fastly_config_value_typed_bounded,
    run_sync_host_call,
};
use async_trait::async_trait;
use edgezero_core::config_store::{BoundedStoreRead, ConfigStore, ConfigStoreError};
use edgezero_core::{Deadline, MonotonicClock};
use fastly_shared::FastlyStatus;
use fastly_sys::fastly_config_store;

// A physical entry is at most 8,000 Unicode scalars. This adapter also imposes
// a finite byte ceiling independently of changes to the provider's quota.
const MAX_ENTRY_BYTES: u64 = 32_000;

/// Config store backed by a Fastly Config Store resource link.
pub struct FastlyConfigStore {
    inner: FastlyConfigStoreBackend,
}

enum FastlyConfigStoreBackend {
    Fastly(u32),
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

    #[expect(
        unsafe_code,
        reason = "the pinned ABI receives a live initialized slice and cannot resize it"
    )]
    fn read_entry(&self, key: &str, limit: u64) -> Result<Option<String>, ConfigStoreError> {
        let read_limit = limit.min(MAX_ENTRY_BYTES);
        match &self.inner {
            FastlyConfigStoreBackend::Fastly(handle) => {
                read_host_buffer(read_limit, |buffer| {
                    let mut written = 0;
                    // SAFETY: host output is confined to this initialized slice;
                    // reported lengths are validated before indexing or truncation.
                    let status = unsafe {
                        fastly_config_store::get(
                            *handle,
                            key.as_ptr(),
                            key.len(),
                            buffer.as_mut_ptr(),
                            buffer.len(),
                            &raw mut written,
                        )
                    };
                    (status, written)
                })
            }
            #[cfg(test)]
            FastlyConfigStoreBackend::InMemory(data) => match data.get(key) {
                Some(value)
                    if u64::try_from(value.len()).map_or(true, |length| length > read_limit) =>
                {
                    Err(ConfigStoreError::ValueTooLarge)
                }
                value => Ok(value.cloned()),
            },
        }
    }

    /// Open a Fastly Config Store by resource link name.
    ///
    /// # Errors
    /// Returns a redacted typed failure if the host cannot open the store.
    #[inline]
    #[expect(
        unsafe_code,
        reason = "the pinned host ABI writes only the supplied handle out-parameter"
    )]
    pub fn try_open(name: &str) -> Result<Self, ConfigStoreError> {
        let mut handle = fastly_shared::INVALID_CONFIG_STORE_HANDLE;
        // SAFETY: the UTF-8 name slice and initialized out-parameter remain live
        // for this synchronous call. Config Store exposes no close operation.
        let status =
            unsafe { fastly_config_store::open(name.as_ptr(), name.len(), &raw mut handle) };
        if status != FastlyStatus::OK || handle == fastly_shared::INVALID_CONFIG_STORE_HANDLE {
            return Err(ConfigStoreError::unavailable(
                "config store could not be opened",
            ));
        }
        Ok(Self {
            inner: FastlyConfigStoreBackend::Fastly(handle),
        })
    }
}

#[async_trait(?Send)]
impl ConfigStore for FastlyConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<edgezero_core::ConfigValue>, ConfigStoreError> {
        let root_value = self.read_entry(key, MAX_ENTRY_BYTES)?;
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
            let got = self
                .read_entry(chunk_key, MAX_ENTRY_BYTES)
                .map_err(|error| {
                    if !matches!(error, ConfigStoreError::InvalidKey { .. }) {
                        transient.set(true);
                    }
                    "config store chunk read failed".to_owned()
                })?;
            if got.is_none() {
                // Referenced chunk absent at this POP: treat as propagation lag
                // (transient) rather than corruption.
                transient.set(true);
            }
            Ok(got)
        });
        outcome
            .map(edgezero_core::ConfigValue::from)
            .map(Some)
            .map_err(|error| map_resolve_failure(key, transient.get(), error))
    }

    #[inline]
    async fn get_bounded(
        &self,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<edgezero_core::ConfigValue>, ConfigStoreError> {
        let root_limit = max_backend_bytes.min(max_value_bytes.max(8_000));
        let materialized_root =
            run_sync_host_call(clock, deadline, || self.read_entry(key, root_limit))
                .map_err(map_sync_config_error)?;
        let root_read = exact_fastly_read(materialized_root, max_backend_bytes)
            .map_err(|_size_error| ConfigStoreError::ValueTooLarge)?;
        let Some(root_value) = root_read.value else {
            return Ok(BoundedStoreRead {
                backend_bytes: root_read.backend_bytes,
                value: None,
            });
        };

        let transient = Cell::new(false);
        let outcome = resolve_fastly_config_value_typed_bounded(
            key,
            root_value,
            root_read.backend_bytes,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            |chunk_key, remaining_backend_bytes| {
                let chunk_value = run_sync_host_call(clock, deadline, || {
                    self.read_entry(chunk_key, remaining_backend_bytes)
                })
                .map_err(map_sync_config_error)?;
                if chunk_value.is_none() {
                    transient.set(true);
                }
                exact_fastly_read(chunk_value, remaining_backend_bytes)
                    .map_err(|_size_error| ConfigStoreError::ValueTooLarge)
            },
        );

        if deadline.is_expired_at(clock.now()) {
            return Err(ConfigStoreError::DeadlineExceeded);
        }
        match outcome {
            Ok(read) => {
                let value = read.value.map(Into::into);
                if deadline.is_expired_at(clock.now()) {
                    return Err(ConfigStoreError::DeadlineExceeded);
                }
                Ok(BoundedStoreRead {
                    backend_bytes: read.backend_bytes,
                    value,
                })
            }
            Err(BoundedResolveFailure::Backend(error)) => Err(error),
            Err(BoundedResolveFailure::DeadlineExceeded) => Err(ConfigStoreError::DeadlineExceeded),
            Err(BoundedResolveFailure::Resolve(error)) => {
                Err(map_resolve_failure(key, transient.get(), error))
            }
            Err(BoundedResolveFailure::ValueTooLarge) => Err(ConfigStoreError::ValueTooLarge),
        }
    }
}

fn read_host_buffer(
    limit: u64,
    read: impl FnOnce(&mut [u8]) -> (FastlyStatus, usize),
) -> Result<Option<String>, ConfigStoreError> {
    let length = usize::try_from(limit).map_err(|_error| ConfigStoreError::ValueTooLarge)?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_error| ConfigStoreError::ValueTooLarge)?;
    buffer.resize(length, 0);
    let (status, written) = read(&mut buffer);
    match status {
        FastlyStatus::OK if written <= buffer.len() => {
            buffer.truncate(written);
            String::from_utf8(buffer)
                .map(Some)
                .map_err(|_error| ConfigStoreError::unavailable("config value is not UTF-8"))
        }
        FastlyStatus::NONE => Ok(None),
        FastlyStatus::BUFLEN => Err(ConfigStoreError::ValueTooLarge),
        FastlyStatus::INVAL | FastlyStatus::UNSUPPORTED => {
            Err(ConfigStoreError::invalid_key("invalid config key"))
        }
        _ => Err(ConfigStoreError::unavailable("config store read failed")),
    }
}

fn map_sync_config_error(call_error: SyncHostCallError<ConfigStoreError>) -> ConfigStoreError {
    match call_error {
        SyncHostCallError::Backend(backend_error) => backend_error,
        SyncHostCallError::DeadlineExceeded => ConfigStoreError::DeadlineExceeded,
    }
}

fn map_resolve_failure(key: &str, transient: bool, error: ResolveFailure) -> ConfigStoreError {
    if error.is_future_format() {
        log::warn!(
            "Fastly config-store value for `{key}` uses a NEWER format than this build \
             understands: {}. Re-pushing the same config will not help -- redeploy this \
             service with an updated EdgeZero build.",
            error.into_message()
        );
        return ConfigStoreError::internal(anyhow::anyhow!(
            "config store value uses a newer format than this build understands; redeploy \
             this service with an updated build (re-pushing will not help)"
        ));
    }

    let message = error.into_message();
    if transient {
        log::warn!(
            "Fastly config-store chunk lookup for `{key}` was transiently unavailable: {message}"
        );
        ConfigStoreError::unavailable("config store temporarily unavailable")
    } else {
        log::warn!(
            "Fastly config-store chunk resolution failed for `{key}`: {message}. \
             Re-run `<app-cli> config push` to repair the store."
        );
        ConfigStoreError::internal(anyhow::anyhow!(
            "config store entry is corrupt or incomplete; re-run config push to repair: {message}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::{Deadline, MonotonicClock, MonotonicInstant};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    edgezero_core::config_store_contract_tests!(fastly_config_store_contract, {
        FastlyConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
        ])
    });

    #[test]
    fn bounded_chunked_read_counts_root_pointer_and_all_chunks() {
        use crate::chunked_config::prepare_fastly_config_entries;
        use edgezero_core::blob_envelope::BlobEnvelope;
        use futures::executor::block_on;
        use serde_json::json;

        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({ "pad": "x".repeat(9_000) }),
            "2026-01-01T00:00:00Z".to_owned(),
        ))
        .expect("envelope");
        let entries = prepare_fastly_config_entries("app_config", &envelope).expect("entries");
        let maybe_backend_bytes = entries.iter().try_fold(0_u64, |total, (_, value)| {
            total.checked_add(u64::try_from(value.len()).ok()?)
        });
        let expected_backend_bytes = maybe_backend_bytes.expect("backend byte sum");
        let store = FastlyConfigStore::from_entries(entries);
        let clock = MonotonicClock::default();

        let read = block_on(store.get_bounded(
            "app_config",
            &clock,
            Deadline::after(Duration::from_secs(1)),
            expected_backend_bytes,
            u64::try_from(envelope.len()).expect("envelope length"),
        ))
        .expect("exact aggregate cap must succeed");

        assert_eq!(read.backend_bytes, expected_backend_bytes);
        assert_eq!(read.value.as_deref(), Some(envelope.as_str()));
    }

    #[test]
    fn bounded_config_read_uses_injected_clock() {
        use futures::executor::block_on;

        let process_now = MonotonicInstant::now();
        let injected_now = process_now
            .checked_sub(Duration::from_mins(1))
            .expect("injected instant");
        let clock = MonotonicClock::new(move || injected_now);
        let deadline = Deadline::at_instant(
            injected_now
                .checked_add(Duration::from_secs(1))
                .expect("deadline instant"),
        );
        let store = FastlyConfigStore::from_entries([("key".to_owned(), "value".to_owned())]);
        let read = block_on(store.get_bounded("key", &clock, deadline, 5, 5))
            .expect("the application clock is still before its deadline");
        assert_eq!(read.value.as_deref(), Some("value"));

        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let advancing_clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = run_sync_host_call(&advancing_clock, Deadline::at_instant(terminal), || {
            *now.lock().expect("clock lock") = terminal;
            Err::<(), _>(ConfigStoreError::unavailable("provider failed at expiry"))
        })
        .expect_err("equality expiry must beat a ready provider error");
        assert!(matches!(error, SyncHostCallError::DeadlineExceeded));
    }

    #[test]
    fn fixture_matches_physical_entry_byte_ceiling() {
        let store = FastlyConfigStore::from_entries([("key".to_owned(), "x".repeat(32_001))]);
        let error = store.read_entry("key", u64::MAX).expect_err("physical cap");
        assert!(matches!(error, ConfigStoreError::ValueTooLarge));
    }

    #[test]
    fn host_buffer_accepts_exact_cap_without_copying_the_string() {
        let value = read_host_buffer(4, |buffer| {
            assert_eq!(buffer.len(), 4);
            buffer.copy_from_slice(b"test");
            (FastlyStatus::OK, 4)
        })
        .expect("exact cap");
        assert_eq!(value.as_deref(), Some("test"));
    }

    #[test]
    fn host_buffer_never_retries_or_allocates_the_reported_oversize_length() {
        for limit in [0, 1, 7_000] {
            let calls = Cell::new(0_u32);
            let error = read_host_buffer(limit, |buffer| {
                calls.set(calls.get() + 1);
                assert_eq!(u64::try_from(buffer.len()).expect("buffer length"), limit);
                (FastlyStatus::BUFLEN, usize::MAX)
            })
            .expect_err("oversize");
            assert!(matches!(error, ConfigStoreError::ValueTooLarge));
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn host_buffer_status_and_malformed_results_are_typed_and_redacted() {
        for status in [FastlyStatus::INVAL, FastlyStatus::UNSUPPORTED] {
            let error = read_host_buffer(1, |_buffer| (status, 0)).expect_err("key failure");
            assert!(matches!(error, ConfigStoreError::InvalidKey { .. }));
        }
        for status in [
            FastlyStatus::ERROR,
            FastlyStatus::BADF,
            FastlyStatus::LIMITEXCEEDED,
            FastlyStatus::OK,
        ] {
            let error = read_host_buffer(1, |_buffer| (status, 2)).expect_err("bad result");
            assert!(matches!(error, ConfigStoreError::Unavailable { .. }));
        }
        assert!(
            read_host_buffer(0, |_buffer| (FastlyStatus::NONE, 0))
                .expect("missing")
                .is_none()
        );
        let error = read_host_buffer(1, |buffer| {
            buffer.fill(255);
            (FastlyStatus::OK, 1)
        })
        .expect_err("invalid UTF-8");
        assert!(matches!(error, ConfigStoreError::Unavailable { .. }));
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
            serde_json::json!({"edgezero_kind": "fastly_config_chunks"}).to_string(),
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
            serde_json::json!({"edgezero_kind": "fastly_config_chunks", "version": 2_u64, "chunks": []}).to_string(),
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
