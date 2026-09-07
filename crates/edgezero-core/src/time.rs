use std::time::Duration;
use web_time::Instant;

/// Max adapter overhead tolerated before a fan-out slot fails closed.
pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);
/// Hard clamp on any caller-supplied duration, so construction cannot panic.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_hours(168);
/// Budget applied when a request sets neither a timeout nor a deadline.
pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);

/// An absolute, copyable monotonic deadline. A deadline at or before now is expired.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    /// Returns a deadline `now + min(duration, DEADLINE_FAR_FUTURE)`; never panics.
    #[inline]
    #[must_use]
    pub fn after(duration: Duration) -> Self {
        let now = Instant::now();
        let clamped = duration.min(DEADLINE_FAR_FUTURE);
        Deadline(now.checked_add(clamped).unwrap_or(now))
    }

    /// Constructs a deadline from an absolute instant.
    #[inline]
    #[must_use]
    pub fn at_instant(instant: Instant) -> Self {
        Deadline(instant)
    }

    /// Returns the absolute deadline instant.
    #[inline]
    #[must_use]
    pub fn instant(&self) -> Instant {
        self.0
    }

    /// Returns `true` once the deadline instant is at or before now.
    #[inline]
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    fn is_expired_at(&self, now: Instant) -> bool {
        self.remaining_at(now).is_none()
    }

    /// Returns the remaining time, or `None` once the deadline is reached or passed.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.0
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_time::Instant;

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
        let base = Instant::now();
        let past = Deadline::at_instant(base);
        let now = base
            .checked_add(Duration::from_secs(1))
            .expect("no overflow");
        assert!(past.is_expired_at(now));
        assert_eq!(past.remaining_at(now), None);
    }

    #[test]
    fn deadline_exactly_now_is_expired() {
        let base = Instant::now();
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
        let base = Instant::now();
        let future = Deadline::at_instant(
            base.checked_add(Duration::from_mins(1))
                .expect("no overflow"),
        );
        assert!(!future.is_expired_at(base));
        assert_eq!(future.remaining_at(base), Some(Duration::from_mins(1)));
    }

    #[test]
    fn after_clamps_duration_max_to_far_future() {
        let before = Instant::now();
        let deadline = Deadline::after(Duration::MAX);
        let after = Instant::now();
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
        let before = Instant::now();
        let far = Deadline::after(Duration::from_hours(1));
        let after = Instant::now();
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
        let base = Instant::now()
            .checked_add(Duration::from_secs(10))
            .expect("no overflow");
        assert_eq!(Deadline::at_instant(base).instant(), base);
    }
}
