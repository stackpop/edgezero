//! Cloudflare Workers adapter config store: reads from a KV namespace.
//!
//! Each declared config id maps to its own Cloudflare KV namespace binding,
//! resolved at request time from `EDGEZERO__STORES__CONFIG__<ID>__NAME`.
//! Bounded reads drain a byte stream into caller-sized buffers; raw `get` is
//! explicitly unbounded. Neither path certifies opaque host allocations.
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

#![cfg_attr(
    all(feature = "cloudflare", target_arch = "wasm32"),
    expect(
        clippy::self_named_module_files,
        reason = "the workspace denies both contradictory module-file conventions; retain its self-named-file convention"
    )
)]

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod kv;

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
    Kv(kv::KvBinding),
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
        let store = kv::KvBinding::from_this(env.as_ref(), binding_name)?;
        Ok(Self {
            inner: CloudflareConfigBackend::Kv(store),
        })
    }

    #[cfg_attr(
        not(all(feature = "cloudflare", target_arch = "wasm32")),
        expect(
            clippy::unused_async,
            reason = "native fixture is synchronous; the production SDK read is asynchronous"
        )
    )]
    async fn materialized_get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
            CloudflareConfigBackend::Kv(store) => store.materialized_get(key).await,
            #[cfg(test)]
            CloudflareConfigBackend::InMemory(data) => Ok(data.get(key).cloned()),
        }
    }
}

#[async_trait(?Send)]
impl ConfigStore for CloudflareConfigStore {
    #[inline]
    async fn get(&self, key: &str) -> Result<Option<edgezero_core::ConfigValue>, ConfigStoreError> {
        self.materialized_get(key)
            .await
            .map(|value| value.map(Into::into))
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
        match &self.inner {
            #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
            CloudflareConfigBackend::Kv(store) => {
                bounded_config_read(
                    store.streaming_get(
                        key,
                        max_backend_bytes.min(max_value_bytes),
                        clock,
                        deadline,
                    ),
                    clock,
                    deadline,
                    max_backend_bytes,
                    max_value_bytes,
                )
                .await
            }
            #[cfg(test)]
            CloudflareConfigBackend::InMemory(_) => {
                bounded_config_read(
                    self.materialized_get(key),
                    clock,
                    deadline,
                    max_backend_bytes,
                    max_value_bytes,
                )
                .await
            }
        }
    }
}

// The timer covers acquisition, draining and conversion. Reader drop requests
// cancellation, but finite host teardown is a separate deployed evidence gate.
async fn bounded_config_read<F>(
    read: F,
    clock: &MonotonicClock,
    deadline: Deadline,
    max_backend_bytes: u64,
    max_value_bytes: u64,
) -> Result<BoundedStoreRead<edgezero_core::ConfigValue>, ConfigStoreError>
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
) -> Result<BoundedStoreRead<edgezero_core::ConfigValue>, ConfigStoreError>
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
    #[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
    mod streaming {
        use super::*;
        use wasm_bindgen_test::wasm_bindgen_test;
        use worker::js_sys::{Function, Number, Promise, Reflect, global};
        use worker::wasm_bindgen::JsCast as _;
        use worker::wasm_bindgen::JsValue;
        use worker::wasm_bindgen_futures::JsFuture;

        fn streaming_store(length: usize, stalled: bool) -> (CloudflareConfigStore, JsValue) {
            // An actual KV binding double: text reads fail and the byte source only
            // fills caller-owned BYOB views, without materializing a complete value.
            let factory = Function::new_with_args(
                "length, stalled",
                "
                // worker::Delay expects browser numeric handles, not Node objects.
                if (!globalThis.edgezeroNumericTimers) {
                    globalThis.edgezeroNumericTimers = true;
                    const set = globalThis.setTimeout;
                    const clear = globalThis.clearTimeout;
                    const timers = new Map();
                    let next = 0;
                    globalThis.setTimeout = (callback, delay, ...args) => {
                        const id = ++next;
                        timers.set(id, set(() => { timers.delete(id); callback(...args); }, delay));
                        return id;
                    };
                    globalThis.clearTimeout = id => {
                        clear(timers.get(id) ?? id);
                        timers.delete(id);
                    };
                }
                const stats = { cancels: 0, gets: 0, pulls: 0, maxView: 0 };
                const kv = {
                    get: async (key, options) => {
                        stats.gets++;
                        if (options.type !== 'stream') throw new Error('text read forbidden');
                        if (key === 'missing') return null;
                        if (key === 'bad-binding') return 'not-a-stream';
                        if (key === 'acquisition-error') throw new Error('TOKEN-DO-NOT-LEAK');
                        if (key === 'non-byte') return new ReadableStream({
                            cancel() { stats.cancels++; return Promise.reject(new Error('TOKEN-DO-NOT-LEAK')); }
                        });
                        let position = 0;
                        const stream = new ReadableStream({
                            type: 'bytes',
                            pull(controller) {
                                stats.pulls++;
                                if (key === 'source-error') throw new Error('TOKEN-DO-NOT-LEAK');
                                const view = controller.byobRequest.view;
                                stats.maxView = Math.max(stats.maxView, view.byteLength);
                                if (stalled) return new Promise(() => {});
                                const count = Math.min(view.byteLength, length - position);
                                if (count === 0) {
                                    controller.close();
                                    controller.byobRequest.respond(0);
                                } else {
                                    view.fill(key === 'invalid-utf8' ? 255 : 97, 0, count);
                                    position += count;
                                    controller.byobRequest.respond(count);
                                }
                            },
                            cancel() {
                                stats.cancels++;
                                if (key === 'reject-cancel') return Promise.reject(new Error('TOKEN-DO-NOT-LEAK'));
                            }
                        }, { highWaterMark: 0 });
                        if (key === 'late') return new Promise(resolve => { stats.resolve = () => resolve(stream); });
                        return stream;
                    },
                    getWithMetadata() {}, put() {}, list() {}, delete() {}
                };
                return { kv, stats };
            ",
            );
            let js_length = u32::try_from(length).expect("fixture length fits JS integer");
            let binding = factory
                .call2(
                    &JsValue::NULL,
                    &JsValue::from(js_length),
                    &JsValue::from(stalled),
                )
                .expect("KV fixture");
            let stats = Reflect::get(&binding, &JsValue::from("stats")).expect("stats");
            let kv = kv::KvBinding::from_this(&binding, "kv").expect("KV binding");
            (
                CloudflareConfigStore {
                    inner: CloudflareConfigBackend::Kv(kv),
                },
                stats,
            )
        }

        fn counter(stats: &JsValue, field: &str) -> usize {
            Number::from(Reflect::get(stats, &JsValue::from(field)).expect("counter"))
                .to_string_with_radix(10)
                .expect("decimal counter")
                .as_string()
                .expect("counter string")
                .parse()
                .expect("integer counter")
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_requires_gc_cleanup_support_before_provider_dispatch() {
            let (store, stats) = streaming_store(1, false);
            let global_object = global();
            let property = JsValue::from("FinalizationRegistry");
            let previous = Reflect::get(&global_object, &property).expect("runtime feature");
            Reflect::set(&global_object, &property, &JsValue::UNDEFINED).expect("disable feature");
            let result = store
                .get_bounded(
                    "value",
                    &MonotonicClock::default(),
                    Deadline::after(Duration::from_secs(1)),
                    1,
                    1,
                )
                .await;
            Reflect::set(&global_object, &property, &previous).expect("restore feature");
            assert!(matches!(result, Err(ConfigStoreError::Unavailable { .. })));
            assert_eq!(counter(&stats, "gets"), 0);
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_failed_binding_and_cancellation_never_publish_provider_errors() {
            for key in [
                "bad-binding",
                "acquisition-error",
                "non-byte",
                "source-error",
                "reject-cancel",
            ] {
                let (store, stats) = streaming_store(2, false);
                let error = store
                    .get_bounded(
                        key,
                        &MonotonicClock::default(),
                        Deadline::after(Duration::from_secs(1)),
                        1,
                        1,
                    )
                    .await
                    .expect_err("typed rejection");
                assert!(!error.to_string().contains("TOKEN-DO-NOT-LEAK"));
                assert!(matches!(
                    error,
                    ConfigStoreError::Unavailable { .. } | ConfigStoreError::ValueTooLarge
                ));
                if matches!(key, "non-byte" | "reject-cancel") {
                    assert_eq!(counter(&stats, "cancels"), 1);
                }
                // Node's runner fails on unhandled promise rejection.
                Delay::from(Duration::from_millis(10)).await;
            }
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_cancels_stream_arriving_after_acquisition_is_dropped() {
            let (store, stats) = streaming_store(1, false);
            let clock = MonotonicClock::default();
            let mut read = Box::pin(store.get_bounded(
                "late",
                &clock,
                Deadline::after(Duration::from_secs(1)),
                1,
                1,
            ));
            assert!(futures_util::poll!(read.as_mut()).is_pending());
            drop(read);
            let resolve = Reflect::get(&stats, &JsValue::from("resolve"))
                .expect("resolver")
                .dyn_into::<Function>()
                .expect("resolve function");
            resolve.call0(&JsValue::NULL).expect("resolve late stream");
            Delay::from(Duration::from_millis(10)).await;
            assert_eq!(
                counter(&stats, "cancels"),
                1,
                "late acquisition still owns cancellation"
            );
            assert_eq!(
                counter(&stats, "pulls"),
                0,
                "late stream must never be read"
            );
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_ready_reads_check_the_clock_between_pulls() {
            let (store, stats) = streaming_store(32 * 1024, false);
            let start = edgezero_core::MonotonicInstant::now();
            let end = start.checked_add(Duration::from_secs(1)).expect("end");
            let calls = Arc::new(AtomicUsize::new(0));
            let samples = Arc::clone(&calls);
            let clock = MonotonicClock::new(move || {
                if samples.fetch_add(1, Ordering::SeqCst) < 2 {
                    start
                } else {
                    end
                }
            });
            let error = store
                .get_bounded(
                    "value",
                    &clock,
                    Deadline::at_instant(end),
                    32 * 1024,
                    32 * 1024,
                )
                .await
                .expect_err("deadline");
            assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
            assert!(
                counter(&stats, "pulls") <= 1,
                "ready microtasks must not drain past expiry"
            );
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_uses_streaming_and_inclusive_caps() {
            for length in [0, 1, 0x4000, 0x8001] {
                let (store, stats) = streaming_store(length, false);
                let limit = u64::try_from(length).expect("limit");
                let read = store
                    .get_bounded(
                        "value",
                        &MonotonicClock::default(),
                        Deadline::after(Duration::from_secs(1)),
                        limit,
                        limit,
                    )
                    .await
                    .expect("bounded stream");
                assert_eq!(read.backend_bytes, limit);
                assert_eq!(read.value.as_deref().map(str::len), Some(length));
                assert_eq!(counter(&stats, "cancels"), 0);
                assert!(counter(&stats, "maxView") <= 0x4000);
            }
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_cancels_before_appending_first_excess_byte() {
            for (backend, value) in [(4, 5), (5, 4), (0, 0)] {
                let (store, stats) = streaming_store(5, false);
                let error = store
                    .get_bounded(
                        "value",
                        &MonotonicClock::default(),
                        Deadline::after(Duration::from_secs(1)),
                        backend,
                        value,
                    )
                    .await
                    .expect_err("overflow");
                assert!(matches!(error, ConfigStoreError::ValueTooLarge));
                assert_eq!(counter(&stats, "cancels"), 1);
            }
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_missing_and_invalid_utf8_are_distinct() {
            let (store, _) = streaming_store(1, false);
            let clock = MonotonicClock::default();
            let deadline = Deadline::after(Duration::from_secs(1));
            let read = store
                .get_bounded("missing", &clock, deadline, 1, 1)
                .await
                .expect("missing");
            assert!(read.value.is_none());
            assert_eq!(read.backend_bytes, 0);
            let error = store
                .get_bounded("invalid-utf8", &clock, deadline, 1, 1)
                .await
                .expect_err("UTF-8");
            assert!(matches!(error, ConfigStoreError::Unavailable { .. }));
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_dropped_read_cancels_pending_reader() {
            let (store, stats) = streaming_store(1, true);
            let clock = MonotonicClock::default();
            let mut read = Box::pin(store.get_bounded(
                "value",
                &clock,
                Deadline::after(Duration::from_mins(1)),
                1,
                1,
            ));
            // Allow promise acquisition and the first read to become pending.
            for _ in 0_u32..4 {
                assert!(futures_util::poll!(read.as_mut()).is_pending());
                JsFuture::from(Promise::resolve(&JsValue::NULL))
                    .await
                    .expect("microtask");
            }
            assert_eq!(counter(&stats, "pulls"), 1);
            drop(read);
            assert_eq!(counter(&stats, "cancels"), 1);
        }

        #[wasm_bindgen_test]
        async fn bounded_kv_timer_cancels_a_permanently_pending_read() {
            let (store, stats) = streaming_store(1, true);
            let error = store
                .get_bounded(
                    "value",
                    &MonotonicClock::default(),
                    Deadline::after(Duration::from_millis(20)),
                    1,
                    1,
                )
                .await
                .expect_err("deadline");
            assert!(matches!(error, ConfigStoreError::DeadlineExceeded));
            assert_eq!(counter(&stats, "cancels"), 1);
        }
    }

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
