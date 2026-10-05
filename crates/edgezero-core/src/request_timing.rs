//! Bounded, best-effort per-request timing with application-owned data.
//!
//! All clones share one immutable origin and one state mutex. Elapsed reads are
//! lock-free; contended state operations never block or substitute zero for
//! unavailable facts. Callbacks run under
//! the lock and must be short, synchronous and non-reentrant. Panics propagate,
//! without rollback: callbacks must leave their data valid even on unwind.
//! Poisoned locks recover best-effort, not with validation of application state.
//! Collection does not render headers or implicitly mark request completion.
//!
//! On deployed Cloudflare Workers, timers advance only after I/O, so CPU-only
//! phases may record zero. Local Wrangler/workerd and browser tests do not
//! reproduce this restriction. See the [Workers timer documentation](https://developers.cloudflare.com/workers/runtime-apis/performance/).

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use web_time::Instant;

/// Why a timing operation could not be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TimingError {
    /// The slot is outside the collector's fixed array; nothing was changed.
    #[error("timing slot is out of bounds")]
    InvalidSlot,
    /// Another operation holds the lock; no callback was invoked or state changed.
    #[error("request timings are currently unavailable")]
    Unavailable,
}

struct Shared<const N: usize, D> {
    state: Mutex<Inner<N, D>>,
    t0: Instant,
}

struct Inner<const N: usize, D> {
    data: D,
    headers_ready: Option<Duration>,
    phases: [Option<Duration>; N],
    request_complete: Option<Duration>,
    response_bytes: Option<u64>,
}

/// Consistent generic fields and borrowed application data, projected under lock.
///
/// The callback may return owned values, but cannot retain `data` beyond the call.
/// Durations are unrounded; output conversion and exposure are application policy.
///
/// ```compile_fail
/// use edgezero_core::request_timing::RequestTimings;
/// let timings = RequestTimings::<1, String>::new();
/// let escaped = timings.snapshot(|view| view.data).unwrap();
/// ```
#[non_exhaustive]
pub struct TimingSnapshot<'data, const N: usize, D = ()> {
    /// Application facts protected by the same lock as the generic fields.
    pub data: &'data D,
    /// Elapsed from the shared origin when this snapshot was taken.
    pub elapsed: Duration,
    /// Elapsed at the first successful explicit headers-ready mark.
    pub headers_ready: Option<Duration>,
    /// Accumulated durations; absent differs from a recorded zero.
    pub phases: [Option<Duration>; N],
    /// Elapsed at the first successful explicit request-completion mark.
    pub request_complete: Option<Duration>,
    /// Last successfully recorded response byte count.
    pub response_bytes: Option<u64>,
}

/// A cheap shared handle; neither cloning nor projection requires `D: Clone`.
///
/// Create one per request at the chosen measurement boundary, not as router state.
/// Installed extension handles require `D: Send + 'static`, not `D: Sync`.
/// T0 is collector creation, not platform ingress or browser navigation start.
pub struct RequestTimings<const N: usize, D = ()>(Arc<Shared<N, D>>);

impl<const N: usize, D> fmt::Debug for RequestTimings<N, D> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestTimings").finish_non_exhaustive()
    }
}

impl<const N: usize, D> Clone for RequestTimings<N, D> {
    #[inline]
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

impl<const N: usize, D: Default> Default for RequestTimings<N, D> {
    #[inline]
    fn default() -> Self {
        Self::with_data(D::default())
    }
}

impl<const N: usize, D: Default> RequestTimings<N, D> {
    /// Starts a request clock with default application data.
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<const N: usize, D> RequestTimings<N, D> {
    /// Returns elapsed time from the immutable shared origin without locking.
    /// This remains available during state contention and inside callbacks.
    #[must_use]
    #[inline]
    pub fn elapsed(&self) -> Duration {
        self.0.t0.elapsed()
    }

    /// Marks headers ready, first successful write wins (later calls succeed unchanged).
    /// # Errors
    /// Returns [`TimingError::Unavailable`] on contention.
    #[inline]
    pub fn mark_headers_ready(&self) -> Result<(), TimingError> {
        let mut inner = self.try_inner()?;
        if inner.headers_ready.is_none() {
            inner.headers_ready = Some(self.elapsed());
        }
        Ok(())
    }

    /// Marks request completion explicitly, first successful write wins.
    /// Neither span drop nor middleware return calls this automatically.
    /// # Errors
    /// Returns [`TimingError::Unavailable`] on contention.
    #[inline]
    pub fn mark_request_complete(&self) -> Result<(), TimingError> {
        let mut inner = self.try_inner()?;
        if inner.request_complete.is_none() {
            inner.request_complete = Some(self.elapsed());
        }
        Ok(())
    }

    /// Saturating-adds a duration to a slot, preserving recorded zero.
    /// Overlapping spans on one slot sum and can exceed wall-clock time.
    /// To measure concurrent latency, use one span around the join.
    /// # Errors
    /// Returns [`TimingError::InvalidSlot`] before any mutation for invalid slots,
    /// or [`TimingError::Unavailable`] on contention.
    #[inline]
    pub fn record(&self, slot: usize, duration: Duration) -> Result<(), TimingError> {
        self.record_with(slot, duration, |_, _| ())
    }

    /// Accumulates a phase and updates application facts under one lock.
    ///
    /// The callback receives elapsed time from T0. It must be short, synchronous,
    /// non-reentrant and leave data valid on unwind. A panic propagates and can
    /// leave both the phase and application data partially changed; no rollback
    /// is attempted. Poison recovery follows the module's best-effort contract.
    /// # Errors
    /// Invalid slots return [`TimingError::InvalidSlot`] before the callback or
    /// any mutation. Contention returns [`TimingError::Unavailable`] likewise.
    #[inline]
    pub fn record_with<R, F: FnOnce(&mut D, Duration) -> R>(
        &self,
        slot: usize,
        duration: Duration,
        update: F,
    ) -> Result<R, TimingError> {
        Self::validate_slot(slot)?;
        let mut inner = self.try_inner()?;
        // Bounds were checked before acquiring the lock or invoking user code.
        let phase = inner.phases.get_mut(slot).ok_or(TimingError::InvalidSlot)?;
        *phase = Some(phase.unwrap_or(Duration::ZERO).saturating_add(duration));
        let elapsed = self.elapsed();
        Ok(update(&mut inner.data, elapsed))
    }

    /// Records the response byte count, last successful write wins.
    /// # Errors
    /// Returns [`TimingError::Unavailable`] on contention.
    #[inline]
    pub fn set_response_bytes(&self, bytes: u64) -> Result<(), TimingError> {
        self.try_inner()?.response_bytes = Some(bytes);
        Ok(())
    }

    /// Projects generic and application facts together, without cloning the payload.
    /// The callback runs under lock and must be short, synchronous and non-reentrant.
    /// Callback panics propagate; subsequent operations recover poison best-effort.
    /// # Errors
    /// Returns [`TimingError::Unavailable`] without invoking the callback on contention.
    #[inline]
    pub fn snapshot<R, F: FnOnce(TimingSnapshot<'_, N, D>) -> R>(
        &self,
        project: F,
    ) -> Result<R, TimingError> {
        let inner = self.try_inner()?;
        Ok(project(TimingSnapshot {
            phases: inner.phases,
            elapsed: self.elapsed(),
            headers_ready: inner.headers_ready,
            request_complete: inner.request_complete,
            response_bytes: inner.response_bytes,
            data: &inner.data,
        }))
    }

    /// Starts a phase span. Dropping it attempts to record work so far, including
    /// cancellation, but never marks completion. Panic-abort may skip destructors.
    /// # Errors
    /// Returns [`TimingError::InvalidSlot`] for an out-of-bounds slot.
    #[inline]
    pub fn span(&self, slot: usize) -> Result<PhaseSpan<N, D>, TimingError> {
        Self::validate_slot(slot)?;
        Ok(PhaseSpan {
            timings: self.clone(),
            slot,
            started: Instant::now(),
        })
    }

    fn try_inner(&self) -> Result<MutexGuard<'_, Inner<N, D>>, TimingError> {
        match self.0.state.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => Err(TimingError::Unavailable),
        }
    }

    /// Updates application facts using the shared origin, under the collector lock.
    /// The callback has the same panic and reentrancy contract as [`Self::record_with`].
    /// # Errors
    /// Returns [`TimingError::Unavailable`] without invoking the callback on contention.
    #[inline]
    pub fn update_data<R, F: FnOnce(&mut D, Duration) -> R>(
        &self,
        update: F,
    ) -> Result<R, TimingError> {
        let mut inner = self.try_inner()?;
        let elapsed = self.elapsed();
        Ok(update(&mut inner.data, elapsed))
    }

    fn validate_slot(slot: usize) -> Result<(), TimingError> {
        if slot < N {
            Ok(())
        } else {
            Err(TimingError::InvalidSlot)
        }
    }

    /// Starts a request clock with application data, without a `Default` bound.
    #[must_use]
    #[inline]
    pub fn with_data(data: D) -> Self {
        let t0 = Instant::now();
        Self(Arc::new(Shared {
            state: Mutex::new(Inner {
                phases: [None; N],
                headers_ready: None,
                request_complete: None,
                response_bytes: None,
                data,
            }),
            t0,
        }))
    }
}

/// Records elapsed phase work on drop, discarding a contended sample.
#[must_use = "a span records when dropped; bind it to a named variable (not `_`) to time the enclosing scope"]
pub struct PhaseSpan<const N: usize, D = ()> {
    slot: usize,
    started: Instant,
    timings: RequestTimings<N, D>,
}

impl<const N: usize, D> Drop for PhaseSpan<N, D> {
    #[inline]
    fn drop(&mut self) {
        match self.timings.record(self.slot, self.started.elapsed()) {
            Ok(()) | Err(TimingError::Unavailable | TimingError::InvalidSlot) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aged<const N: usize, D>(data: D) -> RequestTimings<N, D> {
        let t0 = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        RequestTimings(Arc::new(Shared {
            state: Mutex::new(Inner {
                phases: [None; N],
                headers_ready: None,
                request_complete: None,
                response_bytes: None,
                data,
            }),
            t0,
        }))
    }

    #[test]
    fn clone_shares_aged_origin_and_facts_but_requests_are_independent() {
        let timings = aged::<2, _>(());
        let clone = timings.clone();
        assert!(Arc::ptr_eq(&timings.0, &clone.0));
        assert!(clone.elapsed() >= Duration::from_secs(10));
        clone.record(0, Duration::from_millis(3)).unwrap();
        assert_eq!(
            timings.snapshot(|view| view.phases).unwrap(),
            [Some(Duration::from_millis(3)), None]
        );
        let separate = RequestTimings::<2>::new();
        assert!(!Arc::ptr_eq(&timings.0, &separate.0));
        assert_eq!(separate.snapshot(|view| view.phases).unwrap(), [None; 2]);
    }

    #[test]
    fn middleware_and_handler_preserve_preaged_origin() {
        use crate::body::Body;
        use crate::context::RequestContext;
        use crate::error::EdgeError;
        use crate::http::{Response, StatusCode, request_builder};
        use crate::middleware::RequestTimingMiddleware;
        use crate::response::response_with_body;
        use crate::router::RouterService;
        use futures::executor::block_on;

        #[crate::action]
        async fn timed(ctx: RequestContext) -> Result<Response, EdgeError> {
            let handle = ctx
                .request()
                .extensions()
                .get::<RequestTimings<1>>()
                .unwrap();
            assert!(handle.elapsed() >= Duration::from_secs(10));
            handle.record(0, Duration::from_millis(3)).unwrap();
            handle.mark_headers_ready().unwrap();
            response_with_body(StatusCode::OK, Body::empty())
        }

        let timings = aged::<1, _>(());
        timings.record(0, Duration::from_millis(8)).unwrap();
        let mut request = request_builder().uri("/timed").body(Body::empty()).unwrap();
        request.extensions_mut().insert(timings.clone());
        let router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .middleware(RequestTimingMiddleware::<1>::default())
            .get("/timed", timed)
            .build();
        assert_eq!(
            block_on(router.oneshot(request)).unwrap().status(),
            StatusCode::OK
        );
        timings
            .snapshot(|view| {
                assert_eq!(view.phases, [Some(Duration::from_millis(11))]);
                assert!(view.headers_ready.unwrap() >= Duration::from_secs(10));
                assert_eq!(view.request_complete, None);
            })
            .unwrap();
    }

    #[test]
    fn data_does_not_need_default_or_clone() {
        struct Data(u8);
        let timings = RequestTimings::<0, _>::with_data(Data(7));
        let clone = timings.clone();
        assert_eq!(clone.snapshot(|view| view.data.0).unwrap(), 7);
        let _guard = timings.0.state.lock().unwrap();
        assert_eq!(format!("{clone:?}"), "RequestTimings { .. }");
    }

    #[test]
    fn phases_preserve_zero_accumulate_saturate_and_reject_invalid_slots() {
        let timings = RequestTimings::<2>::new();
        timings.record(0, Duration::ZERO).unwrap();
        assert_eq!(
            timings
                .snapshot(|view: TimingSnapshot<'_, 2>| view.phases)
                .unwrap(),
            [Some(Duration::ZERO), None]
        );
        timings.record(0, Duration::from_millis(2)).unwrap();
        timings.record(0, Duration::from_millis(3)).unwrap();
        assert_eq!(
            timings.snapshot(|view| view.phases[0]).unwrap(),
            Some(Duration::from_millis(5))
        );
        timings.record(0, Duration::MAX).unwrap();
        assert_eq!(
            timings.snapshot(|view| view.phases[0]).unwrap(),
            Some(Duration::MAX)
        );
        assert_eq!(
            timings.record(2, Duration::MAX),
            Err(TimingError::InvalidSlot)
        );
        assert_eq!(
            timings.record(usize::MAX, Duration::ZERO),
            Err(TimingError::InvalidSlot)
        );
        assert!(matches!(timings.span(2), Err(TimingError::InvalidSlot)));
        assert_eq!(
            timings.snapshot(|view| view.phases).unwrap(),
            [Some(Duration::MAX), None]
        );
        assert_eq!(
            RequestTimings::<0>::new().record(0, Duration::ZERO),
            Err(TimingError::InvalidSlot)
        );
    }

    #[test]
    fn lifecycle_marks_are_explicit_first_write_and_bytes_are_last_write() {
        let timings = aged::<1, _>(());
        timings
            .snapshot(|view| {
                assert_eq!(view.headers_ready, None);
                assert_eq!(view.request_complete, None);
                assert_eq!(view.response_bytes, None);
            })
            .unwrap();
        timings.mark_headers_ready().unwrap();
        timings.mark_request_complete().unwrap();
        let first = timings
            .snapshot(|view| (view.headers_ready, view.request_complete))
            .unwrap();
        assert!(first.0.unwrap() >= Duration::from_secs(10));
        assert!(first.1.unwrap() >= first.0.unwrap());
        timings.mark_headers_ready().unwrap();
        timings.mark_request_complete().unwrap();
        assert_eq!(
            timings
                .snapshot(|view| (view.headers_ready, view.request_complete))
                .unwrap(),
            first
        );
        timings.set_response_bytes(42).unwrap();
        timings.set_response_bytes(0).unwrap();
        assert_eq!(
            timings.snapshot(|view| view.response_bytes).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn compound_updates_and_projection_share_one_lock_and_origin() {
        let timings = aged::<1, _>((None, None));
        let returned = timings
            .record_with(0, Duration::from_millis(9), |data, elapsed| {
                data.0 = Some("before headers");
                data.1 = Some(elapsed);
                42_u32
            })
            .unwrap();
        assert_eq!(returned, 42);
        timings
            .snapshot(|view| {
                assert_eq!(view.phases, [Some(Duration::from_millis(9))]);
                assert_eq!(view.data.0, Some("before headers"));
                assert!(view.data.1.unwrap() >= Duration::from_secs(10));
                assert!(view.elapsed >= view.data.1.unwrap());
                assert!(timings.elapsed() >= view.elapsed);
                assert_eq!(
                    timings.record(0, Duration::MAX),
                    Err(TimingError::Unavailable)
                );
            })
            .unwrap();
        timings
            .update_data(|data, elapsed| {
                assert!(timings.elapsed() >= elapsed);
                data.0 = Some("streaming");
            })
            .unwrap();
        assert_eq!(
            timings.snapshot(|view| view.data.0).unwrap(),
            Some("streaming")
        );
        assert_eq!(
            timings.record_with(1, Duration::MAX, |_, _| panic!("invalid callback")),
            Err(TimingError::InvalidSlot)
        );
        assert_eq!(
            timings.snapshot(|view| (view.phases, view.data.0)).unwrap(),
            ([Some(Duration::from_millis(9))], Some("streaming"))
        );
    }

    #[test]
    fn contention_drops_whole_operations_without_invoking_callbacks() {
        let timings = RequestTimings::<1, u32>::new();
        let elapsed = timings.elapsed();
        let guard = timings.0.state.lock().unwrap();
        assert_eq!(
            timings.record_with(0, Duration::MAX, |_, _| panic!("contended update")),
            Err(TimingError::Unavailable)
        );
        assert_eq!(
            timings.update_data(|_, _| panic!("contended payload")),
            Err(TimingError::Unavailable)
        );
        assert_eq!(
            timings.snapshot(|_| panic!("contended snapshot")),
            Err(TimingError::Unavailable)
        );
        assert!(timings.elapsed() >= elapsed);
        assert_eq!(timings.mark_headers_ready(), Err(TimingError::Unavailable));
        assert_eq!(
            timings.mark_request_complete(),
            Err(TimingError::Unavailable)
        );
        assert_eq!(timings.set_response_bytes(8), Err(TimingError::Unavailable));
        assert_eq!(
            timings.record_with(1, Duration::MAX, |_, _| panic!("invalid update")),
            Err(TimingError::InvalidSlot)
        );
        drop(timings.span(0).unwrap());
        drop(guard);
        timings
            .snapshot(|view| {
                assert_eq!(view.phases, [None]);
                assert_eq!(*view.data, 0);
                assert_eq!(view.headers_ready, None);
                assert_eq!(view.request_complete, None);
                assert_eq!(view.response_bytes, None);
            })
            .unwrap();
        timings.mark_headers_ready().unwrap();
        assert!(
            timings
                .snapshot(|view| view.headers_ready)
                .unwrap()
                .is_some()
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn elapsed_is_available_while_another_thread_updates_data() {
        use std::{sync::Barrier, thread};

        let timings = aged::<1, _>(0_u32);
        let entered = Barrier::new(2);
        let finished = Barrier::new(2);
        thread::scope(|scope| {
            scope.spawn(|| {
                timings
                    .update_data(|_, _| {
                        entered.wait();
                        finished.wait();
                    })
                    .unwrap();
            });
            entered.wait();
            let elapsed = timings.elapsed();
            finished.wait();
            assert!(elapsed >= Duration::from_secs(10));
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn callback_panic_propagates_leaves_partial_state_and_recovers_poison() {
        use std::panic::catch_unwind;
        let timings = RequestTimings::<1, u32>::new();
        let outcome = catch_unwind(|| {
            timings
                .record_with(0, Duration::from_millis(4), |data, _| {
                    *data = 7;
                    panic!("callback interrupted");
                })
                .unwrap();
        });
        assert!(outcome.is_err());
        assert!(timings.0.state.is_poisoned());
        assert_eq!(
            timings.snapshot(|view| (view.phases, *view.data)).unwrap(),
            ([Some(Duration::from_millis(4))], 7)
        );
        timings
            .record_with(0, Duration::from_millis(2), |data, _| {
                *data = data.saturating_add(1);
            })
            .unwrap();
        assert_eq!(
            timings.snapshot(|view| (view.phases, *view.data)).unwrap(),
            ([Some(Duration::from_millis(6))], 8)
        );
        timings.mark_request_complete().unwrap();
        assert!(
            timings
                .snapshot(|view| view.request_complete)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn spans_record_drop_and_cancellation_without_completing_request() {
        use futures::{FutureExt as _, future::pending};
        let timings = RequestTimings::<1>::new();
        let mut span = timings.span(0).unwrap();
        span.started = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        drop(span);
        let before = timings.snapshot(|view| view.phases[0]).unwrap().unwrap();
        assert!(before >= Duration::from_secs(2));
        let fresh = RequestTimings::<1>::new();
        let fresh_work = async {
            let _span = fresh.span(0).unwrap();
            pending::<()>().await;
        };
        // Poll once, then drop the pending future, as cancellation would.
        assert!(fresh_work.now_or_never().is_none());
        fresh
            .snapshot(|view| {
                assert!(view.phases[0].is_some());
                assert_eq!(view.headers_ready, None);
                assert_eq!(view.request_complete, None);
            })
            .unwrap();
    }
}
