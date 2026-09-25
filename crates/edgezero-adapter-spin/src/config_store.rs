//! Spin adapter config store: wraps `SpinSdkKvStore`.
//!
//! KV-backed (was variables-backed up through 2026-Q2). Handlers query
//! the store with the canonical dotted key (`service.timeout_ms`); the
//! Spin KV API accepts arbitrary key bytes, so no `.→__` translation
//! is needed. The per-id platform store name is supplied at construction
//! by [`crate::request::build_config_registry`], which resolves it
//! through `EDGEZERO__STORES__CONFIG__<ID>__NAME`.

use std::future::Future;
#[cfg(test)]
use std::future::{pending, ready};
use std::time::Duration;

use async_trait::async_trait;
use edgezero_core::config_store::{
    BoundedStoreRead, ConfigStore, ConfigStoreError, finish_bounded_config_read,
};
use edgezero_core::time::{Deadline, MonotonicClock};
use futures_util::future::{Either, select};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::key_value::Store as SpinSdkKvStore;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::time::sleep;
#[cfg(test)]
use std::collections::BTreeMap;

/// Config store backed by a Spin KV store.
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(test)]
    InMemory(BTreeMap<String, bytes::Bytes>),
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    Spin {
        label: String,
        store: SpinSdkKvStore,
    },
}

impl SpinConfigStore {
    /// Build an in-memory fixture from `(key, bytes)` pairs.
    ///
    /// Bytes are stored verbatim — `get` strictly decodes UTF-8, mirroring
    /// the wasm backend's behaviour (the contract `non_utf8_value_returns_unavailable`
    /// test exercises the error path explicitly).
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, bytes::Bytes)>) -> Self {
        Self {
            inner: SpinConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }

    /// Open the platform store once. Called from
    /// [`crate::request::build_config_registry`] during dispatch setup so
    /// missing `key_value_stores = [...]` declarations surface as a clean
    /// dispatch error instead of on first config read.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::internal`] when the underlying
    /// `SpinSdkKvStore::open` fails — typically because the label isn't
    /// declared in the component's `key_value_stores = [...]` AND
    /// registered with a backend in `runtime-config.toml`. This is a
    /// structural / permanent failure (operator config drift), not a
    /// transient backend hiccup, so we report `Internal` rather than
    /// `Unavailable` so observability alerts on it and callers don't
    /// retry pointlessly.
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    #[inline]
    pub async fn open(label: String) -> Result<Self, ConfigStoreError> {
        let store = SpinSdkKvStore::open(&label).await.map_err(|err| {
            ConfigStoreError::internal(anyhow::anyhow!(
                "open `{label}`: {err} (is the label declared in spin.toml's `key_value_stores` AND registered in runtime-config.toml?)"
            ))
        })?;
        Ok(Self {
            inner: SpinConfigBackend::Spin { label, store },
        })
    }
}

#[async_trait(?Send)]
impl ConfigStore for SpinConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(test)]
            SpinConfigBackend::InMemory(map) => match map.get(key) {
                Some(bytes) => String::from_utf8(bytes.to_vec()).map(Some).map_err(|err| {
                    // Strict UTF-8 to match the wasm backend's error path.
                    // `from_utf8_lossy` would silently hide a divergence
                    // between test and prod.
                    ConfigStoreError::unavailable(format!("non-utf8 value for `{key}`: {err}"))
                }),
                None => Ok(None),
            },
            #[cfg(all(feature = "spin", target_arch = "wasm32"))]
            SpinConfigBackend::Spin { label, store } => match store.get(key).await {
                Ok(Some(bytes)) => String::from_utf8(bytes).map(Some).map_err(|err| {
                    ConfigStoreError::unavailable(format!(
                        "store `{label}`: non-utf8 value for `{key}`: {err}"
                    ))
                }),
                Ok(None) => Ok(None),
                Err(err) => Err(ConfigStoreError::unavailable(format!(
                    "store `{label}`: {err}"
                ))),
            },
        }
    }

    #[inline]
    async fn get_bounded(
        &self,
        key: &str,
        clock: &MonotonicClock,
        deadline: Deadline,
        max_backend_bytes: u64,
        max_value_bytes: u64,
    ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
        bounded_config_read(
            self.get(key),
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
        )
        .await
    }
}

// Spin KV returns a complete value, so the byte bounds apply after host
// materialization. The timer race bounds guest return but does not prove host cancellation.
async fn bounded_config_read<F>(
    read: F,
    clock: &MonotonicClock,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
) -> Result<BoundedStoreRead<String>, ConfigStoreError>
where
    F: Future<Output = Result<Option<String>, ConfigStoreError>>,
{
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    {
        return bounded_config_read_with_timer(
            read,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            sleep,
        )
        .await;
    }
    #[cfg(not(all(feature = "spin", target_arch = "wasm32")))]
    {
        bounded_config_read_with_timer(
            read,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            |_| pending(),
        )
        .await
    }
}

async fn bounded_config_read_with_timer<F, MakeTimer, Timer>(
    read: F,
    clock: &MonotonicClock,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
    mut make_timer: MakeTimer,
) -> Result<BoundedStoreRead<String>, ConfigStoreError>
where
    F: Future<Output = Result<Option<String>, ConfigStoreError>>,
    MakeTimer: FnMut(Duration) -> Timer,
    Timer: Future<Output = ()>,
{
    futures_util::pin_mut!(read);
    loop {
        let Some(remaining) = deadline.remaining_at(clock.now()) else {
            return Err(ConfigStoreError::DeadlineExceeded);
        };
        let timer = make_timer(remaining);
        futures_util::pin_mut!(timer);
        match select(timer, read.as_mut()).await {
            Either::Left(((), _read)) => {
                if deadline.is_expired_at(clock.now()) {
                    return Err(ConfigStoreError::DeadlineExceeded);
                }
            }
            Either::Right((result, _timer)) => {
                return finish_bounded_config_read(
                    result,
                    clock,
                    deadline,
                    max_backend_bytes,
                    max_value_bytes,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures::executor::block_on;

    // Contract tests exercise the InMemory backend with bytes-backed values.
    // KV accepts arbitrary key bytes so the dotted-key form is preserved
    // verbatim end-to-end (no `.→__` translation any more — see module docs).
    edgezero_core::config_store_contract_tests!(spin_config_store_contract, {
        SpinConfigStore::from_entries([
            (
                "contract.key.a".to_owned(),
                bytes::Bytes::from_static(b"value_a"),
            ),
            (
                "contract.key.b".to_owned(),
                bytes::Bytes::from_static(b"value_b"),
            ),
        ])
    });

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn clock_reaching_deadline() -> (MonotonicClock, Deadline) {
        let start = edgezero_core::MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let samples = Arc::new(AtomicUsize::new(0));
        let observed_samples = Arc::clone(&samples);
        let clock = MonotonicClock::new(move || {
            if observed_samples.fetch_add(1, Ordering::SeqCst) == 0 {
                start
            } else {
                terminal
            }
        });
        (clock, Deadline::at_instant(terminal))
    }

    #[test]
    fn bounded_read_timer_drops_never_ready_provider() {
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropProbe(Arc::clone(&drops));
        let read = async move {
            let _guard = guard;
            pending::<Result<Option<String>, ConfigStoreError>>().await
        };
        let (clock, deadline) = clock_reaching_deadline();

        let error = block_on(bounded_config_read_with_timer(
            read,
            &clock,
            deadline,
            1,
            1,
            |_| ready(()),
        ))
        .expect_err("timer must terminate a pending config read");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bounded_read_rearms_after_an_early_timer_wake() {
        let timer_calls = Arc::new(AtomicUsize::new(0));
        let observed_timer_calls = Arc::clone(&timer_calls);
        let clock = MonotonicClock::default();
        let error = block_on(bounded_config_read_with_timer(
            ready(Err(ConfigStoreError::unavailable("provider error"))),
            &clock,
            Deadline::after(Duration::from_secs(1)),
            1,
            1,
            move |_| {
                let early_wake = observed_timer_calls.fetch_add(1, Ordering::SeqCst) == 0;
                async move {
                    if !early_wake {
                        pending::<()>().await;
                    }
                }
            },
        ))
        .expect_err("provider error must win after an early timer wake");

        assert!(matches!(error, ConfigStoreError::Unavailable { .. }));
        assert_eq!(timer_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bounded_read_timer_wins_simultaneous_ready_error_at_deadline() {
        let (clock, deadline) = clock_reaching_deadline();
        let error = block_on(bounded_config_read_with_timer(
            ready(Err(ConfigStoreError::unavailable("provider error"))),
            &clock,
            deadline,
            1,
            1,
            |_| ready(()),
        ))
        .expect_err("timer readiness at the deadline must win equality");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
    }

    #[test]
    fn dotted_get_resolves_verbatim_under_kv() {
        // The KV backend stores keys verbatim — `feature.new_checkout`
        // round-trips without the legacy `.→__` translation.
        let store = SpinConfigStore::from_entries([
            (
                "feature.new_checkout".to_owned(),
                bytes::Bytes::from_static(b"false"),
            ),
            (
                "service.timeout_ms".to_owned(),
                bytes::Bytes::from_static(b"1500"),
            ),
        ]);

        assert_eq!(
            block_on(store.get("feature.new_checkout")).expect("dotted lookup"),
            Some("false".to_owned()),
        );
        assert_eq!(
            block_on(store.get("service.timeout_ms")).expect("dotted lookup"),
            Some("1500".to_owned()),
        );
        // Negative: the legacy flat form is NOT a fallback any more.
        assert_eq!(
            block_on(store.get("feature__new_checkout")).expect("flat lookup"),
            None,
            "KV accepts arbitrary keys; the dotted and flat forms are distinct"
        );
    }

    #[test]
    fn non_utf8_value_returns_unavailable() {
        // Mirrors the wasm backend's strict-UTF-8 path. Documents the
        // contract that binary KV values are NOT silently lossily decoded.
        let store = SpinConfigStore::from_entries([(
            "binary".to_owned(),
            // `0xFF` is not a valid UTF-8 lead byte.
            bytes::Bytes::from_static(&[0xFF_u8, 0xFE_u8]),
        )]);
        let err = block_on(store.get("binary")).expect_err("non-utf8 -> error");
        let msg = err.to_string();
        assert!(
            msg.contains("non-utf8 value for `binary`"),
            "expected non-utf8 message, got: {msg}"
        );
    }

    #[test]
    fn missing_key_returns_none() {
        let store = SpinConfigStore::from_entries([]);
        assert_eq!(block_on(store.get("absent")).expect("ok"), None);
    }

    #[test]
    fn bounded_read_reports_exact_bytes_and_accepts_exact_caps() {
        let clock = MonotonicClock::default();
        let result = block_on(bounded_config_read(
            async { Ok(Some("value".to_owned())) },
            &clock,
            Deadline::after(Duration::from_secs(1)),
            5,
            5,
        ))
        .expect("exact caps must succeed");

        assert_eq!(result.backend_bytes, 5);
        assert_eq!(result.value.as_deref(), Some("value"));
    }

    #[test]
    fn bounded_read_rejects_either_exceeded_cap() {
        for (max_backend_bytes, max_value_bytes) in [(4, 5), (5, 4)] {
            let clock = MonotonicClock::default();
            let error = block_on(bounded_config_read(
                async { Ok(Some("value".to_owned())) },
                &clock,
                Deadline::after(Duration::from_secs(1)),
                max_backend_bytes,
                max_value_bytes,
            ))
            .expect_err("an exceeded cap must fail");

            assert!(matches!(error, ConfigStoreError::ValueTooLarge));
        }
    }

    #[test]
    fn bounded_read_checks_deadline_before_polling_host_call() {
        let polled = Cell::new(false);
        let clock = MonotonicClock::default();
        let error = block_on(bounded_config_read(
            async {
                polled.set(true);
                Ok(None)
            },
            &clock,
            Deadline::after(Duration::ZERO),
            1,
            1,
        ))
        .expect_err("expired deadline must fail");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
        assert!(!polled.get(), "expired reads must not poll the host call");
    }

    #[test]
    fn bounded_read_checks_deadline_after_host_call() {
        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = block_on(bounded_config_read(
            async {
                *now.lock().expect("clock lock") = terminal;
                Ok(None)
            },
            &clock,
            Deadline::at_instant(terminal),
            1,
            1,
        ))
        .expect_err("a host call completing after the deadline must fail");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
    }

    #[test]
    fn bounded_read_deadline_wins_over_late_host_error() {
        let start = MonotonicInstant::now();
        let terminal = start
            .checked_add(Duration::from_secs(1))
            .expect("terminal instant");
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *clock_now.lock().expect("clock lock"));
        let error = block_on(bounded_config_read(
            async {
                *now.lock().expect("clock lock") = terminal;
                Err(ConfigStoreError::unavailable("late host error"))
            },
            &clock,
            Deadline::at_instant(terminal),
            1,
            1,
        ))
        .expect_err("the post-call deadline check must run after host errors");

        assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
    }

    #[test]
    fn bounded_config_read_uses_injected_clock() {
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

        let result = block_on(bounded_config_read(
            async { Ok(Some("value".to_owned())) },
            &clock,
            deadline,
            5,
            5,
        ))
        .expect("the application clock is still before its deadline");

        assert_eq!(result.value.as_deref(), Some("value"));
    }
}
