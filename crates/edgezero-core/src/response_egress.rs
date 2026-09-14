//! Portable response-egress policy and exactly-once lifecycle reporting.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use crate::http::{HeaderMap, Method, Response, StatusCode, Version};
use crate::response_egress_framing::{PreparedResponseEgress, prepare_response_egress};
use crate::router::RouteMetadata;
use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicClock, MonotonicInstant};

/// Portable response-write budget used when an application installs no policy.
pub const DEFAULT_RESPONSE_WRITE_BUDGET: Duration = Duration::from_secs(30);

/// Internal safety budget used only when transmitting a bounded precommit fallback.
pub const RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET: Duration = Duration::from_secs(1);

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
    RequestCancelled,
    SourceError,
    TransportError,
    Unspecified,
}

/// Identifies which bounded response body contributed the reported byte count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressBodyKind {
    Application,
    Fallback,
}

/// Records whether a selected fallback reached its finish boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressFallbackDisposition {
    Aborted,
    Completed,
}

/// Bounded terminal report for one response-conversion attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseEgressReport {
    pub body_kind: ResponseEgressBodyKind,
    pub bytes_written: u64,
    pub elapsed: Duration,
    pub fallback_disposition: Option<ResponseEgressFallbackDisposition>,
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
    request_method: Method,
    request_start: MonotonicInstant,
    response: Response,
    route: Option<RouteMetadata>,
}

/// Live response-egress state returned when application response preparation fails precommit.
#[doc(hidden)]
pub struct ResponseEgressBeginFailure {
    attempt: ResponseEgressAttempt,
    cause: ResponseEgressOutcome,
    clock: MonotonicClock,
}

impl ResponseEgressBeginFailure {
    /// Consumes the failure into the sole live attempt, its clock, and original cause.
    #[must_use]
    #[inline]
    pub fn into_parts(self) -> (ResponseEgressAttempt, MonotonicClock, ResponseEgressOutcome) {
        (self.attempt, self.clock, self.cause)
    }
}

impl ResponseEgressEnvelope {
    /// Starts an owned response-egress attempt without pre-terminalizing fallback-eligible errors.
    ///
    /// # Errors
    /// Returns the sole live attempt, its injected clock, and a bounded precommit cause when the
    /// policy panics on an unwind-capable target or its deadline cannot be normalized. A target
    /// compiled with panic-abort cannot contain application callback panics.
    #[inline]
    pub fn begin(
        self,
    ) -> Result<
        (
            PreparedResponseEgress,
            ResponseEgressPolicy,
            ResponseEgressAttempt,
            MonotonicClock,
        ),
        Box<ResponseEgressBeginFailure>,
    > {
        let egress_started_at = self.clock.now();
        let head = ResponseEgressHead::new(
            self.response.status(),
            self.response.version(),
            self.response.headers(),
            self.request_start,
            self.route.as_ref(),
        );
        let attempt =
            ResponseEgressAttempt::new(&head, egress_started_at, self.observer, self.clock.clone());
        let Ok(selected_policy) =
            catch_unwind(AssertUnwindSafe(|| (self.policy)(&head, egress_started_at)))
        else {
            log::error!("response-egress policy panicked before conversion");
            return Err(Box::new(ResponseEgressBeginFailure {
                attempt,
                cause: ResponseEgressOutcome::ConversionError,
                clock: self.clock,
            }));
        };
        let normalized_policy = match selected_policy.normalize_at(egress_started_at) {
            Ok(normalized) => normalized,
            Err(outcome) => {
                return Err(Box::new(ResponseEgressBeginFailure {
                    attempt,
                    cause: outcome,
                    clock: self.clock,
                }));
            }
        };
        let Ok(prepared) = prepare_response_egress(&self.request_method, self.response) else {
            log::error!("response-egress framing validation failed before commit");
            return Err(Box::new(ResponseEgressBeginFailure {
                attempt,
                cause: ResponseEgressOutcome::ConversionError,
                clock: self.clock,
            }));
        };
        Ok((prepared, normalized_policy, attempt, self.clock))
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
        request_method: Method,
        policy: ResponseEgressPolicyCallback,
        observer: ResponseEgressObserverHandle,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            clock,
            observer,
            policy,
            request_method,
            request_start,
            response,
            route,
        }
    }

    /// Returns the original request method used for response framing.
    #[must_use]
    #[inline]
    pub fn request_method(&self) -> &Method {
        &self.request_method
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
    body_kind: ResponseEgressBodyKind,
    bytes_written: u64,
    clock: MonotonicClock,
    egress_started_at: MonotonicInstant,
    fallback_cause: Option<ResponseEgressOutcome>,
    fallback_disposition: Option<ResponseEgressFallbackDisposition>,
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
            self.fail_current_body(ResponseEgressOutcome::TransportError, observed_at);
            return false;
        };
        self.bytes_written = total;
        true
    }

    /// Selects the bounded adapter fallback while retaining the original precommit cause.
    #[inline]
    pub fn begin_fallback(&mut self, cause: ResponseEgressOutcome) -> bool {
        if !matches!(self.state, AttemptState::Initial)
            || self.fallback_cause.is_some()
            || !matches!(
                cause,
                ResponseEgressOutcome::ConversionError | ResponseEgressOutcome::DeadlineExceeded
            )
        {
            return false;
        }
        self.body_kind = ResponseEgressBodyKind::Fallback;
        self.fallback_cause = Some(cause);
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
        if self.body_kind == ResponseEgressBodyKind::Fallback {
            return false;
        }
        match self.state {
            AttemptState::Initial => {
                self.transition(ResponseEgressOutcome::ConversionError, observed_at)
            }
            AttemptState::Writing => self.transition(ResponseEgressOutcome::Completed, observed_at),
            AttemptState::Terminal(_) => false,
        }
    }

    fn fail_current_body(
        &mut self,
        application_outcome: ResponseEgressOutcome,
        observed_at: MonotonicInstant,
    ) -> bool {
        if let Some(cause) = self.fallback_cause {
            self.fallback_disposition = Some(ResponseEgressFallbackDisposition::Aborted);
            self.transition(cause, observed_at)
        } else {
            self.transition(application_outcome, observed_at)
        }
    }

    /// Settles a selected fallback while preserving its original precommit cause.
    #[inline]
    pub fn finish_fallback(
        &mut self,
        disposition: ResponseEgressFallbackDisposition,
        observed_at: MonotonicInstant,
    ) -> bool {
        let Some(cause) = self.fallback_cause else {
            return false;
        };
        let valid_state = match disposition {
            ResponseEgressFallbackDisposition::Aborted => {
                matches!(self.state, AttemptState::Initial | AttemptState::Writing)
            }
            ResponseEgressFallbackDisposition::Completed => {
                matches!(self.state, AttemptState::Writing)
            }
        };
        if !valid_state {
            return false;
        }
        self.fallback_disposition = Some(disposition);
        self.transition(cause, observed_at)
    }

    #[must_use]
    #[inline]
    pub fn new(
        head: &ResponseEgressHead<'_>,
        egress_started_at: MonotonicInstant,
        observer: ResponseEgressObserverHandle,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            body_kind: ResponseEgressBodyKind::Application,
            bytes_written: 0,
            clock,
            egress_started_at,
            fallback_cause: None,
            fallback_disposition: None,
            observer,
            request_start: head.request_start(),
            route: head.route().cloned(),
            state: AttemptState::Initial,
        }
    }

    /// Terminalizes an attempt with the supplied outcome.
    ///
    /// `Completed` follows [`Self::complete`] state validation.
    #[inline]
    pub fn terminate(
        &mut self,
        outcome: ResponseEgressOutcome,
        observed_at: MonotonicInstant,
    ) -> bool {
        if self.body_kind == ResponseEgressBodyKind::Fallback {
            return false;
        }
        if matches!(
            outcome,
            ResponseEgressOutcome::Completed | ResponseEgressOutcome::HostHandoff
        ) {
            if outcome == ResponseEgressOutcome::HostHandoff
                && matches!(self.state, AttemptState::Writing)
            {
                self.transition(outcome, observed_at)
            } else {
                self.complete(observed_at)
            }
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

        let elapsed =
            if let Some(elapsed) = observed_at.checked_duration_since(self.egress_started_at) {
                elapsed
            } else {
                log::error!("response-egress monotonic clock moved backwards");
                outcome = ResponseEgressOutcome::Unspecified;
                Duration::ZERO
            };
        let report = ResponseEgressReport {
            bytes_written: self.bytes_written,
            body_kind: self.body_kind,
            elapsed,
            fallback_disposition: self.fallback_disposition,
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
        self.fail_current_body(outcome, self.clock.now());
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
    use crate::body::Body;
    use crate::http::{HeaderMap, HeaderValue, Method, StatusCode, Version, response_builder};
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
            MonotonicClock::default(),
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
            ResponseEgressOutcome::RequestCancelled,
            ResponseEgressOutcome::SourceError,
            ResponseEgressOutcome::TransportError,
            ResponseEgressOutcome::Unspecified,
        ];

        for outcome in outcomes {
            let states = if matches!(
                outcome,
                ResponseEgressOutcome::Completed | ResponseEgressOutcome::HostHandoff
            ) {
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
    fn host_handoff_cannot_skip_the_writing_state() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut attempt = attempt(&observer, started_at);

        assert!(attempt.terminate(ResponseEgressOutcome::HostHandoff, started_at));
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
            MonotonicClock::default(),
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
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Application);
        assert_eq!(reports[0].elapsed, Duration::from_millis(25));
        assert_eq!(reports[0].fallback_disposition, None);
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

        drop(ResponseEgressAttempt::new(
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
    fn zero_byte_completion_has_zero_accounting() {
        let completed_observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut completed = attempt(&completed_observer, started_at);
        assert!(completed.begin_writing());
        assert!(completed.complete(started_at));
        assert_eq!(completed_observer.reports()[0].bytes_written, 0);
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
    fn observer_panics_do_not_escape_terminal_or_drop_paths() {
        let direct = catch_unwind(AssertUnwindSafe(|| {
            let started_at = MonotonicInstant::now();
            let headers = HeaderMap::new();
            let head = head(&headers, started_at, None);
            let mut attempt = ResponseEgressAttempt::new(
                &head,
                started_at,
                ResponseEgressObserverHandle::new(PanickingObserver),
                MonotonicClock::default(),
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
                MonotonicClock::default(),
            ));
        }));
        dropped.unwrap_or_else(|_| panic!("observer panic escaped guard drop"));
    }

    #[test]
    fn fallback_reports_its_bytes_and_disposition_with_original_cause() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut fallback = attempt(&observer, started_at);

        assert!(fallback.begin_fallback(ResponseEgressOutcome::ConversionError));
        assert!(fallback.begin_writing());
        assert!(fallback.account_bytes(7, started_at));
        assert!(
            fallback.finish_fallback(ResponseEgressFallbackDisposition::Completed, started_at,)
        );

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 7);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
    }

    #[test]
    fn fallback_selection_is_one_shot_and_preserves_its_first_cause() {
        let observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut fallback = attempt(&observer, started_at);

        assert!(fallback.begin_fallback(ResponseEgressOutcome::ConversionError));
        assert!(!fallback.begin_fallback(ResponseEgressOutcome::DeadlineExceeded));
        assert!(fallback.begin_writing());
        assert!(fallback.account_bytes(3, started_at));
        assert!(fallback.finish_fallback(ResponseEgressFallbackDisposition::Aborted, started_at,));

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 3);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Aborted)
        );
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
    }

    #[test]
    fn fallback_drop_and_accounting_overflow_abort_with_original_cause() {
        let drop_observer = RecordingObserver::default();
        let started_at = MonotonicInstant::now();
        let mut dropped = attempt(&drop_observer, started_at);
        assert!(!dropped.begin_fallback(ResponseEgressOutcome::SourceError));
        assert!(dropped.begin_fallback(ResponseEgressOutcome::DeadlineExceeded));
        drop(dropped);
        let dropped_report = &drop_observer.reports()[0];
        assert_eq!(
            dropped_report.outcome,
            ResponseEgressOutcome::DeadlineExceeded
        );
        assert_eq!(
            dropped_report.fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Aborted)
        );

        let overflow_observer = RecordingObserver::default();
        let mut overflow = attempt(&overflow_observer, started_at);
        assert!(overflow.begin_fallback(ResponseEgressOutcome::ConversionError));
        assert!(overflow.begin_writing());
        assert!(overflow.account_bytes(u64::MAX, started_at));
        assert!(!overflow.account_bytes(1, started_at));
        let overflow_report = &overflow_observer.reports()[0];
        assert_eq!(overflow_report.bytes_written, u64::MAX);
        assert_eq!(
            overflow_report.outcome,
            ResponseEgressOutcome::ConversionError
        );
        assert_eq!(
            overflow_report.fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Aborted)
        );
    }

    #[test]
    fn policy_panic_returns_live_attempt_for_one_fallback_lifecycle() {
        let observer = RecordingObserver::default();
        let request_start = MonotonicInstant::now();
        let started_at = request_start
            .checked_add(Duration::from_millis(1))
            .expect("egress start");
        let clock = MonotonicClock::new(move || started_at);
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::from("private token=secret"))
            .expect("response");
        let envelope = ResponseEgressEnvelope::new(
            response,
            request_start,
            None,
            Method::GET,
            Arc::new(|_, _| panic!("private token=secret")),
            ResponseEgressObserverHandle::new(observer.clone()),
            clock,
        );
        assert_eq!(envelope.request_method(), &Method::GET);

        let Err(failure) = envelope.begin() else {
            panic!("policy panic must select fallback");
        };
        assert!(observer.reports().is_empty());
        let (mut attempt, failure_clock, cause) = failure.into_parts();
        assert_eq!(cause, ResponseEgressOutcome::ConversionError);
        assert_eq!(failure_clock.now(), started_at);
        assert!(attempt.begin_fallback(cause));
        assert!(attempt.begin_writing());
        assert!(attempt.finish_fallback(ResponseEgressFallbackDisposition::Completed, started_at,));

        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(reports[0].bytes_written, 0);
    }

    #[test]
    fn framing_failure_drops_source_unpolled_and_returns_one_live_attempt() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::task::Poll;

        use futures_util::stream::poll_fn;

        struct DropSignal(Rc<Cell<usize>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let observer = RecordingObserver::default();
        let request_start = MonotonicInstant::now();
        let clock = MonotonicClock::new(move || request_start);
        let polls = Rc::new(Cell::new(0_usize));
        let drops = Rc::new(Cell::new(0_usize));
        let observed_polls = Rc::clone(&polls);
        let drop_signal = DropSignal(Rc::clone(&drops));
        let body = Body::from_stream(poll_fn(move |_| {
            let _keep_signal_alive = &drop_signal;
            observed_polls.set(observed_polls.get() + 1);
            Poll::Ready(Some(Ok(bytes::Bytes::from_static(b"private"))))
        }));
        let response = response_builder()
            .status(StatusCode::OK)
            .header("trailer", "x-checksum")
            .body(body)
            .expect("response");
        let envelope = ResponseEgressEnvelope::new(
            response,
            request_start,
            None,
            Method::GET,
            Arc::new(default_response_egress_policy),
            ResponseEgressObserverHandle::new(observer.clone()),
            clock,
        );

        let Err(failure) = envelope.begin() else {
            panic!("framing failure must retain the attempt");
        };
        assert_eq!(polls.get(), 0);
        assert_eq!(drops.get(), 1);
        assert!(observer.reports().is_empty());
        let (mut attempt, _, cause) = failure.into_parts();
        assert_eq!(cause, ResponseEgressOutcome::ConversionError);
        assert!(attempt.begin_fallback(cause));
        assert!(
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, request_start,)
        );
        assert_eq!(observer.reports().len(), 1);
    }
}
