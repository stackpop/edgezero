//! Cloudflare Workers adapter config store: reads from a KV namespace.
//!
//! Each declared config id maps to its own Cloudflare KV namespace binding,
//! resolved at request time from `EDGEZERO__STORES__CONFIG__<ID>__NAME`.
//! Reads are async (`worker::kv::KvStore::get(key).text().await`).
//!
//! ```toml
//! # wrangler.toml
//! [[kv_namespaces]]
//! binding = "app_config"
//! id      = "abc123…"
//! ```
//!
//! This replaces the pre-rewrite `[vars]`-backed JSON-string config store.
//! `[vars]` bindings are restricted to JavaScript identifier syntax, so
//! arbitrary dotted keys had to be JSON-packed inside one variable. The KV
//! backing has no such restriction.

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
#[cfg(test)]
use std::collections::HashMap;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::kv::KvStore as WorkerKvStore;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::{Delay, Env};

/// Config store backed by a Cloudflare KV namespace.
///
/// The namespace binding is opened at construction; individual reads are
/// async KV lookups against that namespace.
pub struct CloudflareConfigStore {
    inner: CloudflareConfigBackend,
}

enum CloudflareConfigBackend {
    #[cfg(test)]
    InMemory(HashMap<String, String>),
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    Kv(WorkerKvStore),
}

impl CloudflareConfigStore {
    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: CloudflareConfigBackend::InMemory(entries.into_iter().collect()),
        }
    }

    /// Open the KV namespace bound as `binding_name`.
    ///
    /// # Errors
    /// Returns [`ConfigStoreError::Unavailable`] when the binding is missing
    /// or cannot be opened.
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    #[inline]
    pub fn from_env(env: &Env, binding_name: &str) -> Result<Self, ConfigStoreError> {
        let store = env.kv(binding_name).map_err(|err| {
            ConfigStoreError::unavailable(format!(
                "failed to open config KV binding '{binding_name}': {err}"
            ))
        })?;
        Ok(Self {
            inner: CloudflareConfigBackend::Kv(store),
        })
    }
}

#[async_trait(?Send)]
impl ConfigStore for CloudflareConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
            CloudflareConfigBackend::Kv(store) => store.get(key).text().await.map_err(|err| {
                ConfigStoreError::internal(anyhow::anyhow!("kv config get failed: {err}"))
            }),
            #[cfg(test)]
            CloudflareConfigBackend::InMemory(data) => Ok(data.get(key).cloned()),
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

// Workers KV returns a complete string, so the byte bounds apply after host
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
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    {
        return bounded_config_read_with_timer(
            read,
            clock,
            deadline,
            max_backend_bytes,
            max_value_bytes,
            Delay::from,
        )
        .await;
    }
    #[cfg(not(all(feature = "cloudflare", target_arch = "wasm32")))]
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

    edgezero_core::config_store_contract_tests!(cloudflare_config_store_contract, {
        CloudflareConfigStore::from_entries([
            ("contract.key.a".to_owned(), "value_a".to_owned()),
            ("contract.key.b".to_owned(), "value_b".to_owned()),
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
