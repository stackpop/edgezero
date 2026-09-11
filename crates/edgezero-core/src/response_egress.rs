//! Portable response-egress policy and exactly-once lifecycle reporting.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use crate::http::{HeaderMap, Response, StatusCode, Version};
use crate::router::RouteMetadata;
use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicClock, MonotonicInstant};

/// Portable response-write budget used when an application installs no policy.
pub const DEFAULT_RESPONSE_WRITE_BUDGET: Duration = Duration::from_secs(30);

/// One finite absolute deadline selected for a response-conversion attempt.
#[derive(Clone, Copy, Debug)]
pub struct ResponseEgressPolicy {
    pub write_deadline: Deadline,
}

impl ResponseEgressPolicy {
    /// Clamps this policy to the portable far-future bound relative to egress start.
    ///
    /// An already-expired deadline remains valid and is not moved forward.
    ///
    /// # Errors
    /// Returns [`ResponseEgressOutcome::ConversionError`] if the clamp instant cannot be
    /// represented by the monotonic clock.
    #[inline]
    pub fn normalize_at(
        mut self,
        egress_started_at: MonotonicInstant,
    ) -> Result<Self, ResponseEgressOutcome> {
        let maximum = egress_started_at
            .checked_add(DEADLINE_FAR_FUTURE)
            .ok_or(ResponseEgressOutcome::ConversionError)?;
        if self.write_deadline.instant() > maximum {
            self.write_deadline = Deadline::at_instant(maximum);
        }
        Ok(self)
    }
}

/// Immutable response metadata visible to the synchronous egress policy callback.
///
/// This type deliberately has no response-body or mutation access.
#[derive(Clone, Copy, Debug)]
pub struct ResponseEgressHead<'head> {
    headers: &'head HeaderMap,
    request_start: MonotonicInstant,
    route: Option<&'head RouteMetadata>,
    status: StatusCode,
    version: Version,
}

impl<'head> ResponseEgressHead<'head> {
    #[must_use]
    #[inline]
    pub fn headers(&self) -> &'head HeaderMap {
        self.headers
    }

    /// Creates a body-blind response-head view for an adapter conversion attempt.
    #[must_use]
    #[inline]
    pub fn new(
        status: StatusCode,
        version: Version,
        headers: &'head HeaderMap,
        request_start: MonotonicInstant,
        route: Option<&'head RouteMetadata>,
    ) -> Self {
        Self {
            headers,
            request_start,
            route,
            status,
            version,
        }
    }

    #[must_use]
    #[inline]
    pub fn request_start(&self) -> MonotonicInstant {
        self.request_start
    }

    #[must_use]
    #[inline]
    pub fn route(&self) -> Option<&'head RouteMetadata> {
        self.route
    }

    #[must_use]
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    #[inline]
    pub fn version(&self) -> Version {
        self.version
    }
}

/// Synchronous, body-blind policy callback retained by an application.
pub type ResponseEgressPolicyCallback = Arc<
    dyn for<'head> Fn(&ResponseEgressHead<'head>, MonotonicInstant) -> ResponseEgressPolicy
        + Send
        + Sync
        + 'static,
>;

/// Terminal result of one response-conversion attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressOutcome {
    ClientDisconnected,
    Completed,
    ConversionError,
    DeadlineExceeded,
    HostHandoff,
    ResponseReturned,
    SourceError,
    TransportError,
    Unspecified,
}

/// Bounded terminal report for one response-conversion attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseEgressReport {
    pub bytes_written: u64,
    pub elapsed: Duration,
    pub outcome: ResponseEgressOutcome,
    pub request_start: MonotonicInstant,
    pub route: Option<RouteMetadata>,
}

/// Synchronous observer for terminal response-egress reports.
pub trait ResponseEgressObserver: Send + Sync + 'static {
    fn complete(&self, report: &ResponseEgressReport);
}

struct NoopResponseEgressObserver;

impl ResponseEgressObserver for NoopResponseEgressObserver {
    #[inline]
    fn complete(&self, _report: &ResponseEgressReport) {}
}

/// Cloneable handle to an application response-egress observer.
#[derive(Clone)]
pub struct ResponseEgressObserverHandle {
    observer: Arc<dyn ResponseEgressObserver>,
}

/// Adapter-facing response plus immutable ingress metadata and app-owned egress hooks.
#[doc(hidden)]
pub struct ResponseEgressEnvelope {
    clock: MonotonicClock,
    observer: ResponseEgressObserverHandle,
    policy: ResponseEgressPolicyCallback,
    request_start: MonotonicInstant,
    response: Response,
    route: Option<RouteMetadata>,
}

impl ResponseEgressEnvelope {
    /// Starts one response-conversion attempt and evaluates the app policy exactly once.
    ///
    /// # Errors
    /// Returns `ConversionError` after reporting it when policy evaluation panics or deadline
    /// normalization cannot represent the portable clamp.
    #[inline]
    pub fn begin(
        self,
    ) -> Result<
        (
            Response,
            ResponseEgressPolicy,
            ResponseEgressAttempt,
            MonotonicClock,
        ),
        ResponseEgressOutcome,
    > {
        let egress_started_at = self.clock.now();
        let head = ResponseEgressHead::new(
            self.response.status(),
            self.response.version(),
            self.response.headers(),
            self.request_start,
            self.route.as_ref(),
        );
        let mut attempt = ResponseEgressAttempt::new_with_clock(
            &head,
            egress_started_at,
            self.observer,
            self.clock.clone(),
        );
        let Ok(selected_policy) =
            catch_unwind(AssertUnwindSafe(|| (self.policy)(&head, egress_started_at)))
        else {
            log::error!("response-egress policy panicked before conversion");
            attempt.terminate(ResponseEgressOutcome::ConversionError, egress_started_at);
            return Err(ResponseEgressOutcome::ConversionError);
        };
        let normalized_policy = match selected_policy.normalize_at(egress_started_at) {
            Ok(normalized) => normalized,
            Err(outcome) => {
                attempt.terminate(outcome, egress_started_at);
                return Err(outcome);
            }
        };
        Ok((self.response, normalized_policy, attempt, self.clock))
    }

    /// Extracts the response for low-level callers that do not own a platform converter.
    #[must_use]
    #[inline]
    pub fn into_response(self) -> Response {
        self.response
    }

    pub(crate) fn new(
        response: Response,
        request_start: MonotonicInstant,
        route: Option<RouteMetadata>,
        policy: ResponseEgressPolicyCallback,
        observer: ResponseEgressObserverHandle,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            clock,
            observer,
            policy,
            request_start,
            response,
            route,
        }
    }
}

impl ResponseEgressObserverHandle {
    fn complete(&self, report: &ResponseEgressReport) {
        let result = catch_unwind(AssertUnwindSafe(|| self.observer.complete(report)));
        if result.is_err() {
            log::error!("response-egress observer panicked after terminal transition");
        }
    }

    #[must_use]
    #[inline]
    pub fn new<Observer>(observer: Observer) -> Self
    where
        Observer: ResponseEgressObserver,
    {
        Self {
            observer: Arc::new(observer),
        }
    }
}

impl Default for ResponseEgressObserverHandle {
    #[inline]
    fn default() -> Self {
        Self::new(NoopResponseEgressObserver)
    }
}

enum AttemptState {
    Initial,
    Terminal(ResponseEgressReport),
    Writing,
}

/// Adapter-facing exactly-once response-egress completion guard.
///
/// The guard is intentionally non-clone. Dropping it before a terminal signal reports a
/// conversion error from the initial state or a transport error from the writing state.
#[doc(hidden)]
pub struct ResponseEgressAttempt {
    bytes_written: u64,
    clock: MonotonicClock,
    egress_started_at: MonotonicInstant,
    observer: ResponseEgressObserverHandle,
    request_start: MonotonicInstant,
    route: Option<RouteMetadata>,
    state: AttemptState,
}

impl ResponseEgressAttempt {
    /// Accounts payload bytes accepted at the adapter's documented write boundary.
    ///
    /// Returns `false` outside the writing state. Overflow terminalizes the attempt as
    /// [`ResponseEgressOutcome::TransportError`] and also returns `false`.
    #[inline]
    pub fn account_bytes(&mut self, bytes: u64, observed_at: MonotonicInstant) -> bool {
        if !matches!(self.state, AttemptState::Writing) {
            return false;
        }
        let Some(total) = self.bytes_written.checked_add(bytes) else {
            self.transition(ResponseEgressOutcome::TransportError, observed_at);
            return false;
        };
        self.bytes_written = total;
        true
    }

    /// Moves an initial attempt into the writing state.
    ///
    /// Returns `false` after writing has already begun or the attempt is terminal.
    #[inline]
    pub fn begin_writing(&mut self) -> bool {
        if !matches!(self.state, AttemptState::Initial) {
            return false;
        }
        self.state = AttemptState::Writing;
        true
    }

    /// Successfully completes a writing attempt.
    ///
    /// Calling this in the initial state terminalizes as `ConversionError`; a converter must
    /// enter writing even for an empty response. Later terminal signals return `false`.
    #[inline]
    pub fn complete(&mut self, observed_at: MonotonicInstant) -> bool {
        match self.state {
            AttemptState::Initial => {
                self.transition(ResponseEgressOutcome::ConversionError, observed_at)
            }
            AttemptState::Writing => self.transition(ResponseEgressOutcome::Completed, observed_at),
            AttemptState::Terminal(_) => false,
        }
    }

    #[must_use]
    #[inline]
    pub fn new(
        head: &ResponseEgressHead<'_>,
        egress_started_at: MonotonicInstant,
        observer: ResponseEgressObserverHandle,
    ) -> Self {
        Self::new_with_clock(head, egress_started_at, observer, MonotonicClock::default())
    }

    fn new_with_clock(
        head: &ResponseEgressHead<'_>,
        egress_started_at: MonotonicInstant,
        observer: ResponseEgressObserverHandle,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            bytes_written: 0,
            clock,
            egress_started_at,
            observer,
            request_start: head.request_start(),
            route: head.route().cloned(),
            state: AttemptState::Initial,
        }
    }

    /// Terminalizes an attempt with the supplied outcome.
    ///
    /// `Completed` follows [`Self::complete`] state validation. `ResponseReturned` always
    /// reports zero bytes because response-object construction is not a guest-visible write.
    #[inline]
    pub fn terminate(
        &mut self,
        outcome: ResponseEgressOutcome,
        observed_at: MonotonicInstant,
    ) -> bool {
        if outcome == ResponseEgressOutcome::Completed {
            self.complete(observed_at)
        } else {
            self.transition(outcome, observed_at)
        }
    }

    fn transition(
        &mut self,
        mut outcome: ResponseEgressOutcome,
        observed_at: MonotonicInstant,
    ) -> bool {
        if matches!(self.state, AttemptState::Terminal(_)) {
            return false;
        }

        let force_zero_bytes = outcome == ResponseEgressOutcome::ResponseReturned;
        let elapsed =
            if let Some(elapsed) = observed_at.checked_duration_since(self.egress_started_at) {
                elapsed
            } else {
                log::error!("response-egress monotonic clock moved backwards");
                outcome = ResponseEgressOutcome::Unspecified;
                Duration::ZERO
            };
        let bytes_written = if force_zero_bytes {
            0
        } else {
            self.bytes_written
        };
        let report = ResponseEgressReport {
            bytes_written,
            elapsed,
            outcome,
            request_start: self.request_start,
            route: self.route.clone(),
        };
        self.state = AttemptState::Terminal(report);

        if let AttemptState::Terminal(terminal_report) = &self.state {
            self.observer.complete(terminal_report);
        }
        true
    }
}

impl Drop for ResponseEgressAttempt {
    #[inline]
    fn drop(&mut self) {
        let outcome = match self.state {
            AttemptState::Initial => ResponseEgressOutcome::ConversionError,
            AttemptState::Writing => ResponseEgressOutcome::TransportError,
            AttemptState::Terminal(_) => return,
        };
        self.transition(outcome, self.clock.now());
    }
}

/// Selects the portable default response-write policy.
#[must_use]
#[inline]
pub fn default_response_egress_policy(
    _head: &ResponseEgressHead<'_>,
    egress_started_at: MonotonicInstant,
) -> ResponseEgressPolicy {
    let deadline = egress_started_at
        .checked_add(DEFAULT_RESPONSE_WRITE_BUDGET)
        .unwrap_or(egress_started_at);
    ResponseEgressPolicy {
        write_deadline: Deadline::at_instant(deadline),
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::http::{HeaderMap, HeaderValue, Method, StatusCode, Version};
    use crate::router::RouteMetadata;
    use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicClock, MonotonicInstant};

    #[derive(Clone, Default)]
    struct RecordingObserver {
        reports: Arc<Mutex<Vec<ResponseEgressReport>>>,
    }

    impl RecordingObserver {
        fn reports(&self) -> Vec<ResponseEgressReport> {
            self.reports.lock().expect("reports lock").clone()
        }
    }

    impl ResponseEgressObserver for RecordingObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.reports
                .lock()
                .expect("reports lock")
                .push(report.clone());
        }
    }

    struct PanickingObserver;

    impl ResponseEgressObserver for PanickingObserver {
        fn complete(&self, _report: &ResponseEgressReport) {
            panic!("observer panic");
        }
    }

    fn head<'head>(
        headers: &'head HeaderMap,
        request_start: MonotonicInstant,
        route: Option<&'head RouteMetadata>,
    ) -> ResponseEgressHead<'head> {
        ResponseEgressHead::new(
            StatusCode::CREATED,
            Version::HTTP_2,
            headers,
            request_start,
            route,
        )
    }

    fn attempt(
        observer: &RecordingObserver,
        started_at: MonotonicInstant,
    ) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let head = head(&headers, started_at, None);
        ResponseEgressAttempt::new(
            &head,
            started_at,
            ResponseEgressObserverHandle::new(observer.clone()),
        )
    }

    #[test]
    fn default_policy_is_finite_and_exactly_thirty_seconds() {
        let headers = HeaderMap::new();
        let started_at = MonotonicInstant::now();
        let head = head(&headers, started_at, None);
        let policy = default_response_egress_policy(&head, started_at);

        assert_eq!(
            policy.write_deadline.instant(),
            started_at
                .checked_add(DEFAULT_RESPONSE_WRITE_BUDGET)
                .expect("default deadline")
        );
        assert_eq!(DEFAULT_RESPONSE_WRITE_BUDGET, Duration::from_secs(30));
    }

    #[test]
    fn policy_normalization_clamps_far_future_and_preserves_expiry() {
        let started_at = MonotonicInstant::now();
        let maximum = started_at
            .checked_add(DEADLINE_FAR_FUTURE)
            .expect("maximum deadline");
        let beyond = maximum
            .checked_add(Duration::from_secs(1))
            .expect("deadline beyond maximum");
        let clamped = ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(beyond),
        }
        .normalize_at(started_at)
        .expect("normalization");
        assert_eq!(clamped.write_deadline.instant(), maximum);

        let expired = ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(started_at),
        }
        .normalize_at(started_at)
        .expect("expired policy remains valid");
        assert_eq!(expired.write_deadline.instant(), started_at);
    }

    #[test]
    fn head_is_body_blind_and_exposes_only_immutable_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("visible"));
        let request_start = MonotonicInstant::now();
        let route = RouteMetadata::new(Method::GET, "/items/{id}");
        let head = head(&headers, request_start, Some(&route));

        assert_eq!(head.status(), StatusCode::CREATED);
        assert_eq!(head.version(), Version::HTTP_2);
        assert_eq!(head.headers(), &headers);
        assert_eq!(head.request_start(), request_start);
        assert_eq!(head.route(), Some(&route));
    }

    #[test]
    fn observer_trait_is_object_safe_and_handle_has_a_noop_default() {
        fn assert_object_safe(_observer: &dyn ResponseEgressObserver) {}

        let observer = RecordingObserver::default();
        assert_object_safe(&observer);
        let _: ResponseEgressObserverHandle = ResponseEgressObserverHandle::default();
    }

    #[test]
    fn every_outcome_is_reported_without_collapsing_variants_from_valid_states() {
        let outcomes = [
            ResponseEgressOutcome::Completed,
            ResponseEgressOutcome::ClientDisconnected,
            ResponseEgressOutcome::ConversionError,
            ResponseEgressOutcome::DeadlineExceeded,
            ResponseEgressOutcome::HostHandoff,
            ResponseEgressOutcome::ResponseReturned,
            ResponseEgressOutcome::SourceError,
            ResponseEgressOutcome::TransportError,
            ResponseEgressOutcome::Unspecified,
        ];

        for outcome in outcomes {
            let states = if outcome == ResponseEgressOutcome::Completed {
                &[true][..]
            } else {
                &[false, true][..]
            };
            for &writing in states {
                let observer = RecordingObserver::default();
                let started_at = MonotonicInstant::now();
                let mut attempt = attempt(&observer, started_at);
                if writing {
                    assert!(attempt.begin_writing());
                }
                if outcome == ResponseEgressOutcome::Completed {
                    assert!(attempt.complete(started_at));
                } else {
                    assert!(attempt.terminate(outcome, started_at));
                }
                assert_eq!(observer.reports()[0].outcome, outcome);
            }
        }
    }

    #[test]
    fn completed_cannot_skip_the_writing_state() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut attempt = attempt(&observer, started_at);

        assert!(attempt.complete(started_at));
        assert_eq!(observer.reports().len(), 1);
        assert_eq!(
            observer.reports()[0].outcome,
            ResponseEgressOutcome::ConversionError
        );
    }

    #[test]
    fn writing_completion_records_metadata_bytes_and_elapsed_once() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let terminal_at = started_at
            .checked_add(Duration::from_millis(25))
            .expect("terminal instant");
        let request_start = started_at
            .checked_sub(Duration::from_millis(10))
            .expect("request start");
        let route = RouteMetadata::new(Method::POST, "/submit");
        let headers = HeaderMap::new();
        let head = head(&headers, request_start, Some(&route));
        let mut attempt = ResponseEgressAttempt::new(
            &head,
            started_at,
            ResponseEgressObserverHandle::new(observer.clone()),
        );

        assert!(attempt.begin_writing());
        assert!(attempt.account_bytes(0, started_at));
        assert!(attempt.account_bytes(7, started_at));
        assert!(attempt.complete(terminal_at));
        assert!(!attempt.complete(terminal_at));
        assert!(!attempt.terminate(ResponseEgressOutcome::TransportError, terminal_at));
        drop(attempt);

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 7);
        assert_eq!(reports[0].elapsed, Duration::from_millis(25));
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::Completed);
        assert_eq!(reports[0].request_start, request_start);
        assert_eq!(reports[0].route, Some(route));
    }

    #[test]
    fn drop_and_duplicate_signals_preserve_exactly_once_terminal_state() {
        let initial_observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        drop(attempt(&initial_observer, started_at));
        assert_eq!(initial_observer.reports().len(), 1);
        assert_eq!(
            initial_observer.reports()[0].outcome,
            ResponseEgressOutcome::ConversionError
        );

        let writing_observer = RecordingObserver::default();
        let mut writing = attempt(&writing_observer, started_at);
        assert!(writing.begin_writing());
        assert!(!writing.begin_writing());
        drop(writing);
        assert_eq!(writing_observer.reports().len(), 1);
        assert_eq!(
            writing_observer.reports()[0].outcome,
            ResponseEgressOutcome::TransportError
        );

        let disconnected_observer = RecordingObserver::default();
        let mut disconnected = attempt(&disconnected_observer, started_at);
        assert!(disconnected.begin_writing());
        assert!(disconnected.terminate(ResponseEgressOutcome::ClientDisconnected, started_at));
        drop(disconnected);
        assert_eq!(disconnected_observer.reports().len(), 1);
    }

    #[test]
    fn dropped_attempt_uses_its_injected_clock() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let completed_at = started_at
            .checked_add(Duration::from_millis(17))
            .expect("completed instant");
        let clock = MonotonicClock::new(move || completed_at);
        let headers = HeaderMap::new();
        let head = head(&headers, started_at, None);

        drop(ResponseEgressAttempt::new_with_clock(
            &head,
            started_at,
            ResponseEgressObserverHandle::new(observer.clone()),
            clock,
        ));

        assert_eq!(observer.reports()[0].elapsed, Duration::from_millis(17));
    }

    #[test]
    fn byte_overflow_terminalizes_as_transport_error() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut attempt = attempt(&observer, started_at);
        assert!(attempt.begin_writing());
        assert!(attempt.account_bytes(u64::MAX, started_at));
        assert!(!attempt.account_bytes(1, started_at));
        assert!(!attempt.complete(started_at));

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, u64::MAX);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::TransportError);
    }

    #[test]
    fn zero_byte_completion_and_response_returned_have_zero_accounting() {
        let completed_observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut completed = attempt(&completed_observer, started_at);
        assert!(completed.begin_writing());
        assert!(completed.complete(started_at));
        assert_eq!(completed_observer.reports()[0].bytes_written, 0);

        let returned_observer = RecordingObserver::default();
        let mut returned = attempt(&returned_observer, started_at);
        assert!(returned.begin_writing());
        assert!(returned.account_bytes(99, started_at));
        assert!(returned.terminate(ResponseEgressOutcome::ResponseReturned, started_at));
        assert_eq!(returned_observer.reports()[0].bytes_written, 0);
    }

    #[test]
    fn backwards_clock_is_zero_elapsed_unspecified_and_still_once() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let earlier = started_at
            .checked_sub(Duration::from_millis(1))
            .expect("earlier instant");
        let mut attempt = attempt(&observer, started_at);
        assert!(attempt.begin_writing());
        assert!(attempt.complete(earlier));
        drop(attempt);

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].elapsed, Duration::ZERO);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::Unspecified);
    }

    #[test]
    fn backwards_clock_does_not_undo_response_returned_zero_accounting() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let earlier = started_at
            .checked_sub(Duration::from_millis(1))
            .expect("earlier instant");
        let mut attempt = attempt(&observer, started_at);
        assert!(attempt.begin_writing());
        assert!(attempt.account_bytes(99, started_at));
        assert!(attempt.terminate(ResponseEgressOutcome::ResponseReturned, earlier));

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 0);
        assert_eq!(reports[0].elapsed, Duration::ZERO);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::Unspecified);
    }

    #[test]
    fn observer_panics_do_not_escape_terminal_or_drop_paths() {
        let direct = catch_unwind(AssertUnwindSafe(|| {
            let started_at = MonotonicInstant::now();
            let headers = HeaderMap::new();
            let head = head(&headers, started_at, None);
            let mut attempt = ResponseEgressAttempt::new(
                &head,
                started_at,
                ResponseEgressObserverHandle::new(PanickingObserver),
            );
            assert!(attempt.terminate(ResponseEgressOutcome::ConversionError, started_at));
        }));
        direct.unwrap_or_else(|_| panic!("observer panic escaped direct completion"));

        let dropped = catch_unwind(AssertUnwindSafe(|| {
            let started_at = MonotonicInstant::now();
            let headers = HeaderMap::new();
            let head = head(&headers, started_at, None);
            drop(ResponseEgressAttempt::new(
                &head,
                started_at,
                ResponseEgressObserverHandle::new(PanickingObserver),
            ));
        }));
        dropped.unwrap_or_else(|_| panic!("observer panic escaped guard drop"));
    }
}
