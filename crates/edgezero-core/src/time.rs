use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{BudgetSource, EdgeError};
use crate::outbound::OutboundRequest;

/// Max adapter overhead tolerated before a fan-out slot fails closed.
pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);
/// Hard clamp on any caller-supplied duration, so construction cannot panic.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_hours(168);
/// Budget applied when a request sets neither a timeout nor a deadline.
pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);

/// An absolute, copyable monotonic deadline. A deadline at or before now is expired.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(MonotonicInstant);

/// Effective timeout selected for one outbound dispatch.
#[derive(Clone, Copy, Debug)]
pub struct DispatchBudget {
    pub cause: BudgetSource,
    pub deadline: Deadline,
    pub duration: Duration,
}

/// Portable monotonic clock instant used by `EdgeZero` timing APIs.
pub type MonotonicInstant = web_time::Instant;

/// Cloneable monotonic clock source shared by one application and its admitted requests.
#[derive(Clone)]
pub struct MonotonicClock {
    now: Arc<dyn Fn() -> MonotonicInstant + Send + Sync>,
}

impl MonotonicClock {
    #[must_use]
    #[inline]
    pub fn new<Now>(now: Now) -> Self
    where
        Now: Fn() -> MonotonicInstant + Send + Sync + 'static,
    {
        Self { now: Arc::new(now) }
    }

    #[must_use]
    #[inline]
    pub fn now(&self) -> MonotonicInstant {
        (self.now)()
    }
}

impl Default for MonotonicClock {
    #[inline]
    fn default() -> Self {
        Self::new(MonotonicInstant::now)
    }
}

impl fmt::Debug for MonotonicClock {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonotonicClock").finish_non_exhaustive()
    }
}

impl Deadline {
    /// Returns a deadline `now + min(duration, DEADLINE_FAR_FUTURE)`; never panics.
    #[inline]
    #[must_use]
    pub fn after(duration: Duration) -> Self {
        let now = MonotonicInstant::now();
        let clamped = duration.min(DEADLINE_FAR_FUTURE);
        Deadline(now.checked_add(clamped).unwrap_or(now))
    }

    /// Constructs a deadline from an absolute instant.
    #[inline]
    #[must_use]
    pub fn at_instant(instant: MonotonicInstant) -> Self {
        Deadline(instant)
    }

    /// Returns the absolute deadline instant.
    #[inline]
    #[must_use]
    pub fn instant(&self) -> MonotonicInstant {
        self.0
    }

    /// Returns `true` once the deadline instant is at or before now.
    #[inline]
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(MonotonicInstant::now())
    }

    /// Returns `true` when this deadline is at or before the supplied clock snapshot.
    #[inline]
    #[must_use]
    pub fn is_expired_at(&self, now: MonotonicInstant) -> bool {
        self.remaining_at(now).is_none()
    }

    /// Returns the remaining time, or `None` once the deadline is reached or passed.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(MonotonicInstant::now())
    }

    /// Returns time remaining at an explicit clock snapshot, or `None` at/past expiry.
    #[inline]
    #[must_use]
    pub fn remaining_at(&self, now: MonotonicInstant) -> Option<Duration> {
        self.0
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
    }
}

/// Computes one effective outbound deadline from a shared monotonic snapshot.
///
/// # Errors
/// Returns [`EdgeError::GatewayTimeout`] when the selected budget is already exhausted.
#[inline]
pub fn dispatch_budget(
    request: &OutboundRequest,
    now: MonotonicInstant,
) -> Result<DispatchBudget, EdgeError> {
    let inputs = request.budget_inputs();
    let deadline_from_duration = |duration: Duration| {
        let bounded_duration = duration.min(DEADLINE_FAR_FUTURE);
        Deadline::at_instant(now.checked_add(bounded_duration).unwrap_or(now))
    };

    let from_timeout = inputs.timeout.map(deadline_from_duration);
    let from_caller = inputs.deadline.map(|deadline| {
        let far = now.checked_add(DEADLINE_FAR_FUTURE).unwrap_or(now);
        Deadline::at_instant(deadline.instant().min(far))
    });
    let from_default = (inputs.timeout.is_none() && inputs.deadline.is_none())
        .then(|| deadline_from_duration(DEFAULT_NO_DEADLINE_BUDGET));

    let (cause, deadline) = [
        from_timeout.map(|deadline| (BudgetSource::PerCallTimeout, deadline)),
        from_caller.map(|deadline| (BudgetSource::BatchDeadline, deadline)),
        from_default.map(|deadline| (BudgetSource::Default, deadline)),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(_, deadline)| deadline.instant())
    .ok_or_else(|| {
        EdgeError::internal(anyhow::anyhow!(
            "dispatch_budget: no deadline candidate; invariant violated"
        ))
    })?;

    let duration = deadline.instant().saturating_duration_since(now);
    if duration.is_zero() {
        return Err(EdgeError::gateway_timeout_caused(
            "effective budget is zero",
            cause,
        ));
    }
    Ok(DispatchBudget {
        cause,
        deadline,
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn monotonic_clock_uses_the_injected_source() {
        let start = MonotonicInstant::now();
        let now = Arc::new(Mutex::new(start));
        let observed_now = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *observed_now.lock().expect("clock lock"));

        assert_eq!(clock.now(), start);
        let advanced = start
            .checked_add(Duration::from_secs(2))
            .expect("advanced instant");
        *now.lock().expect("clock lock") = advanced;
        assert_eq!(clock.now(), advanced);
        assert_eq!(clock.clone().now(), advanced);
    }

    #[test]
    fn monotonic_instant_is_public_clock_type() {
        let start = MonotonicInstant::now();
        let deadline = Deadline::at_instant(start);
        assert_eq!(deadline.instant(), start);
    }

    #[test]
    fn deadline_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Deadline>();
    }

    #[test]
    fn constants_have_exact_values() {
        assert_eq!(DEFAULT_NO_DEADLINE_BUDGET, Duration::from_secs(30));
        assert_eq!(DEADLINE_FAR_FUTURE, Duration::from_hours(168));
        assert_eq!(BATCH_DISPATCH_SLACK_MAX, Duration::from_millis(25));
    }

    #[test]
    fn deadline_before_now_is_expired() {
        let base = MonotonicInstant::now();
        let past = Deadline::at_instant(base);
        let now = base
            .checked_add(Duration::from_secs(1))
            .expect("no overflow");
        assert!(past.is_expired_at(now));
        assert_eq!(past.remaining_at(now), None);
    }

    #[test]
    fn deadline_exactly_now_is_expired() {
        let base = MonotonicInstant::now();
        let at_now = Deadline::at_instant(base);
        assert_eq!(
            at_now.remaining_at(base),
            None,
            "zero remaining is expired, not Some(0)"
        );
        assert!(
            at_now.is_expired_at(base),
            "a deadline exactly at now is expired"
        );
    }

    #[test]
    fn deadline_in_future_has_exact_remaining() {
        let base = MonotonicInstant::now();
        let future = Deadline::at_instant(
            base.checked_add(Duration::from_mins(1))
                .expect("no overflow"),
        );
        assert!(!future.is_expired_at(base));
        assert_eq!(future.remaining_at(base), Some(Duration::from_mins(1)));
    }

    #[test]
    fn after_clamps_duration_max_to_far_future() {
        let before = MonotonicInstant::now();
        let deadline = Deadline::after(Duration::MAX);
        let after = MonotonicInstant::now();
        let lower = before
            .checked_add(DEADLINE_FAR_FUTURE)
            .expect("no overflow");
        let upper = after.checked_add(DEADLINE_FAR_FUTURE).expect("no overflow");
        assert!(deadline.instant() >= lower, "clamped below the 7-day bound");
        assert!(
            deadline.instant() <= upper,
            "Duration::MAX was not clamped to 7 days"
        );
    }

    #[test]
    fn public_remaining_and_is_expired_smoke() {
        let before = MonotonicInstant::now();
        let far = Deadline::after(Duration::from_hours(1));
        let after = MonotonicInstant::now();
        assert!(!far.is_expired());
        let lower = before
            .checked_add(Duration::from_hours(1))
            .expect("no overflow");
        let upper = after
            .checked_add(Duration::from_hours(1))
            .expect("no overflow");
        assert!(
            far.instant() >= lower && far.instant() <= upper,
            "after() must land exactly at now plus duration"
        );
        assert!(far.remaining().is_some());

        let now_deadline = Deadline::after(Duration::ZERO);
        assert!(now_deadline.is_expired());
        assert_eq!(now_deadline.remaining(), None);
    }

    #[test]
    fn instant_round_trips() {
        let base = MonotonicInstant::now()
            .checked_add(Duration::from_secs(10))
            .expect("no overflow");
        assert_eq!(Deadline::at_instant(base).instant(), base);
    }
}
