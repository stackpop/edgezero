use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::ptr;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};

use axum::http::Response;
use bytes::Bytes;
use edgezero_core::body::{Body, BodyStream};
use edgezero_core::http::{Method, Response as CoreResponse, StatusCode};
use edgezero_core::response_egress::{
    RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET, ResponseEgressAttempt, ResponseEgressEnvelope,
    ResponseEgressFallbackDisposition, ResponseEgressOutcome,
};
use edgezero_core::response_egress_framing::{PreparedResponseEgress, prepare_response_egress};
use edgezero_core::time::{Deadline, MonotonicClock};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::Notify;
use tokio::time::Instant as TokioInstant;

const INTERNAL_FALLBACK_BODY: &[u8] = b"internal server error";
const TIMEOUT_FALLBACK_BODY: &[u8] = b"response write deadline exceeded";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EgressKind {
    Application,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AxumBodyError(ResponseEgressOutcome);

impl fmt::Display for AxumBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            ResponseEgressOutcome::DeadlineExceeded => "response write deadline exceeded",
            ResponseEgressOutcome::SourceError => "response body source failed",
            ResponseEgressOutcome::ClientDisconnected
            | ResponseEgressOutcome::Completed
            | ResponseEgressOutcome::ConversionError
            | ResponseEgressOutcome::HostHandoff
            | ResponseEgressOutcome::RequestCancelled
            | ResponseEgressOutcome::TransportError
            | ResponseEgressOutcome::Unspecified
            | _ => "response transport failed",
        })
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "the category-only body error has no nested diagnostic source"
)]
impl StdError for AxumBodyError {}

impl AxumBodyError {
    pub(crate) const fn is_deadline(self) -> bool {
        matches!(self.0, ResponseEgressOutcome::DeadlineExceeded)
    }
}

#[derive(Default)]
struct EgressConnectionInner {
    changed: Notify,
    responses: RefCell<VecDeque<Rc<ConnectionResponseState>>>,
}

struct ConnectionResponseState {
    active: Cell<bool>,
    connection: Weak<EgressConnectionInner>,
    host_deadline: TokioInstant,
    requested_terminal: Cell<Option<ResponseEgressOutcome>>,
}

impl ConnectionResponseState {
    fn request_terminal(&self, outcome: ResponseEgressOutcome) {
        if self.active.get() && self.requested_terminal.get().is_none() {
            self.requested_terminal.set(Some(outcome));
        }
    }

    fn unregister(&self) {
        if !self.active.replace(false) {
            return;
        }
        let Some(connection) = self.connection.upgrade() else {
            return;
        };
        connection
            .responses
            .borrow_mut()
            .retain(|candidate| !ptr::eq(Rc::as_ptr(candidate), self));
        connection.changed.notify_one();
    }
}

/// Per-HTTP/1-connection response registry used by the connection supervisor.
///
/// The registry stores only deadline events. The non-clone response attempt remains owned by the
/// body coordinator and observes the event when Hyper next polls or drops that body.
#[derive(Clone, Default)]
pub(crate) struct EgressConnection {
    inner: Rc<EgressConnectionInner>,
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "registry operations are grouped by registration, deadline, transport, and notification lifecycle"
)]
impl EgressConnection {
    fn register(
        &self,
        deadline: Deadline,
        clock: &MonotonicClock,
    ) -> Option<Rc<ConnectionResponseState>> {
        let remaining = deadline.remaining_at(clock.now())?;
        let host_deadline = TokioInstant::now().checked_add(remaining)?;
        let state = Rc::new(ConnectionResponseState {
            active: Cell::new(true),
            connection: Rc::downgrade(&self.inner),
            host_deadline,
            requested_terminal: Cell::new(None),
        });
        self.inner
            .responses
            .borrow_mut()
            .push_back(Rc::clone(&state));
        self.inner.changed.notify_one();
        Some(state)
    }

    pub(crate) fn next_deadline(&self) -> Option<TokioInstant> {
        self.inner
            .responses
            .borrow()
            .iter()
            .filter(|state| state.active.get())
            .map(|state| state.host_deadline)
            .min()
    }

    /// Marks the first ordered response at the earliest elapsed deadline as the winner and every
    /// other unsettled response as HTTP/1 connection collateral.
    pub(crate) fn signal_elapsed_deadline(&self, observed_at: TokioInstant) -> bool {
        let responses = self.inner.responses.borrow();
        let Some(winner) = responses
            .iter()
            .filter(|state| state.active.get() && state.host_deadline <= observed_at)
            .min_by_key(|state| state.host_deadline)
            .cloned()
        else {
            return false;
        };
        for state in responses.iter().filter(|state| state.active.get()) {
            if Rc::ptr_eq(state, &winner) {
                state.request_terminal(ResponseEgressOutcome::DeadlineExceeded);
            } else {
                state.request_terminal(ResponseEgressOutcome::TransportError);
            }
        }
        true
    }

    pub(crate) fn signal_transport_error(&self) {
        for state in self
            .inner
            .responses
            .borrow()
            .iter()
            .filter(|state| state.active.get())
        {
            state.request_terminal(ResponseEgressOutcome::TransportError);
        }
    }

    pub(crate) async fn changed(&self) {
        self.inner.changed.notified().await;
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.inner.responses.borrow().is_empty()
    }
}

enum ResponseSource {
    Done,
    Once(Option<Bytes>),
    Stream(BodyStream),
}

struct ActiveEgress {
    attempt: ResponseEgressAttempt,
    clock: MonotonicClock,
    deadline: Deadline,
    kind: EgressKind,
    state: Rc<ConnectionResponseState>,
}

/// Private Hyper body that owns one response-egress attempt through source completion or drop.
pub(crate) struct AxumEgressBody {
    active: Option<ActiveEgress>,
    remaining_hint: Option<u64>,
    source: ResponseSource,
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "constructors precede coordinator transitions and query helpers"
)]
impl AxumEgressBody {
    #[cfg(test)]
    pub(crate) fn application(
        body: Body,
        deadline: Deadline,
        clock: MonotonicClock,
        attempt: ResponseEgressAttempt,
        connection: &EgressConnection,
    ) -> Result<Self, Box<ResponseEgressAttempt>> {
        Self::owned(
            body,
            None,
            true,
            deadline,
            clock,
            attempt,
            EgressKind::Application,
            connection,
        )
    }

    fn detached(bytes: Bytes) -> Self {
        Self {
            active: None,
            remaining_hint: u64::try_from(bytes.len()).ok(),
            source: if bytes.is_empty() {
                ResponseSource::Done
            } else {
                ResponseSource::Once(Some(bytes))
            },
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the private constructor keeps the complete response-lifetime contract explicit"
    )]
    fn owned(
        body: Body,
        declared_length: Option<u64>,
        transmits_body: bool,
        deadline: Deadline,
        clock: MonotonicClock,
        mut attempt: ResponseEgressAttempt,
        kind: EgressKind,
        connection: &EgressConnection,
    ) -> Result<Self, Box<ResponseEgressAttempt>> {
        let Some(state) = connection.register(deadline, &clock) else {
            return Err(Box::new(attempt));
        };
        if !attempt.begin_writing() {
            state.unregister();
            return Err(Box::new(attempt));
        }
        let (source, inferred_length) = if transmits_body {
            match body {
                Body::Once(bytes) if bytes.is_empty() => (ResponseSource::Done, Some(0)),
                Body::Once(bytes) => {
                    let length = u64::try_from(bytes.len()).ok();
                    (ResponseSource::Once(Some(bytes)), length)
                }
                Body::Stream(stream) => (ResponseSource::Stream(stream), None),
            }
        } else {
            drop(body);
            (ResponseSource::Done, Some(0))
        };
        Ok(Self {
            active: Some(ActiveEgress {
                attempt,
                clock,
                deadline,
                kind,
                state,
            }),
            remaining_hint: declared_length.or(inferred_length),
            source,
        })
    }

    fn account(&mut self, length: usize) -> Result<(), AxumBodyError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let Ok(accepted_length) = u64::try_from(length) else {
            self.fail(ResponseEgressOutcome::TransportError);
            return Err(AxumBodyError(ResponseEgressOutcome::TransportError));
        };
        if !active
            .attempt
            .account_bytes(accepted_length, active.clock.now())
        {
            self.fail(ResponseEgressOutcome::TransportError);
            return Err(AxumBodyError(ResponseEgressOutcome::TransportError));
        }
        if let Some(remaining) = self.remaining_hint.as_mut() {
            *remaining = remaining.saturating_sub(accepted_length);
        }
        Ok(())
    }

    fn fail(&mut self, outcome: ResponseEgressOutcome) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let observed_at = if outcome == ResponseEgressOutcome::DeadlineExceeded {
            active.clock.now().max(active.deadline.instant())
        } else {
            active.clock.now()
        };
        match active.kind {
            EgressKind::Application => {
                active.attempt.terminate(outcome, observed_at);
            }
            EgressKind::Fallback => {
                active
                    .attempt
                    .finish_fallback(ResponseEgressFallbackDisposition::Aborted, observed_at);
            }
        }
        active.state.unregister();
    }

    fn finish(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let observed_at = active.clock.now();
        match active.kind {
            EgressKind::Application => {
                active
                    .attempt
                    .terminate(ResponseEgressOutcome::HostHandoff, observed_at);
            }
            EgressKind::Fallback => {
                active
                    .attempt
                    .finish_fallback(ResponseEgressFallbackDisposition::Completed, observed_at);
            }
        }
        active.state.unregister();
    }

    fn requested_failure(&self) -> Option<ResponseEgressOutcome> {
        let active = self.active.as_ref()?;
        active.state.requested_terminal.get()
    }

    fn deadline_expired(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.deadline.is_expired_at(active.clock.now()))
    }
}

impl Drop for AxumEgressBody {
    fn drop(&mut self) {
        if self.active.is_none() {
            return;
        }
        if matches!(
            self.source,
            ResponseSource::Done | ResponseSource::Once(None)
        ) {
            self.finish();
            return;
        }
        let outcome = self
            .requested_failure()
            .unwrap_or(ResponseEgressOutcome::TransportError);
        self.fail(outcome);
    }
}

impl HttpBody for AxumEgressBody {
    type Data = Bytes;
    type Error = AxumBodyError;

    fn is_end_stream(&self) -> bool {
        matches!(self.source, ResponseSource::Done) || self.active.is_none()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.active.is_none() && matches!(this.source, ResponseSource::Done) {
            return Poll::Ready(None);
        }
        if let Some(outcome) = this.requested_failure() {
            this.fail(outcome);
            return Poll::Ready(Some(Err(AxumBodyError(outcome))));
        }
        if this.deadline_expired() {
            this.fail(ResponseEgressOutcome::DeadlineExceeded);
            return Poll::Ready(Some(Err(AxumBodyError(
                ResponseEgressOutcome::DeadlineExceeded,
            ))));
        }

        let next = match &mut this.source {
            ResponseSource::Done | ResponseSource::Once(None) => {
                this.finish();
                return Poll::Ready(None);
            }
            ResponseSource::Once(bytes) => Poll::Ready(bytes.take().map(Ok)),
            ResponseSource::Stream(stream) => stream.as_mut().poll_next(cx),
        };
        if !matches!(next, Poll::Pending) {
            if let Some(outcome) = this.requested_failure() {
                this.fail(outcome);
                return Poll::Ready(Some(Err(AxumBodyError(outcome))));
            }
            if this.deadline_expired() {
                this.source = ResponseSource::Done;
                this.fail(ResponseEgressOutcome::DeadlineExceeded);
                return Poll::Ready(Some(Err(AxumBodyError(
                    ResponseEgressOutcome::DeadlineExceeded,
                ))));
            }
        }
        match next {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                if let Err(error) = this.account(bytes.len()) {
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Some(Err(_error))) => {
                this.source = ResponseSource::Done;
                this.fail(ResponseEgressOutcome::SourceError);
                Poll::Ready(Some(Err(AxumBodyError(ResponseEgressOutcome::SourceError))))
            }
            Poll::Ready(None) => {
                this.source = ResponseSource::Done;
                this.finish();
                Poll::Ready(None)
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(remaining) = self.remaining_hint {
            hint.set_exact(remaining);
        }
        hint
    }
}

/// Converts one owned core envelope into the private body used by `EdgeZero`'s Hyper server.
pub(crate) fn prepare_egress_response(
    egress: ResponseEgressEnvelope,
    connection: &EgressConnection,
) -> Response<AxumEgressBody> {
    let request_method = egress.request_method().clone();
    match egress.begin() {
        Ok((prepared, policy, attempt, clock)) => {
            if policy.write_deadline.is_expired_at(clock.now()) {
                return prepare_fallback(
                    &request_method,
                    ResponseEgressOutcome::DeadlineExceeded,
                    attempt,
                    &clock,
                    connection,
                );
            }
            match start_prepared(
                prepared,
                policy.write_deadline,
                attempt,
                clock.clone(),
                EgressKind::Application,
                connection,
            ) {
                Ok(response) => response,
                Err(returned_attempt) => prepare_fallback(
                    &request_method,
                    ResponseEgressOutcome::ConversionError,
                    *returned_attempt,
                    &clock,
                    connection,
                ),
            }
        }
        Err(failure) => {
            let (attempt, clock, cause) = failure.into_parts();
            prepare_fallback(&request_method, cause, attempt, &clock, connection)
        }
    }
}

fn start_prepared(
    prepared: PreparedResponseEgress,
    deadline: Deadline,
    attempt: ResponseEgressAttempt,
    clock: MonotonicClock,
    kind: EgressKind,
    connection: &EgressConnection,
) -> Result<Response<AxumEgressBody>, Box<ResponseEgressAttempt>> {
    let declared_length = prepared.declared_length();
    let transmits_body = prepared.transmits_body();
    let (parts, core_body) = prepared.into_response().into_parts();
    match AxumEgressBody::owned(
        core_body,
        declared_length,
        transmits_body,
        deadline,
        clock,
        attempt,
        kind,
        connection,
    ) {
        Ok(egress_body) => Ok(Response::from_parts(parts, egress_body)),
        Err(returned_attempt) => Err(returned_attempt),
    }
}

fn prepare_fallback(
    request_method: &Method,
    cause: ResponseEgressOutcome,
    mut attempt: ResponseEgressAttempt,
    clock: &MonotonicClock,
    connection: &EgressConnection,
) -> Response<AxumEgressBody> {
    if !attempt.begin_fallback(cause) {
        attempt.terminate(ResponseEgressOutcome::ConversionError, clock.now());
        return detached_fallback(ResponseEgressOutcome::ConversionError);
    }
    let started_at = clock.now();
    let Some(fallback_deadline) = started_at.checked_add(RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET)
    else {
        attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
        return detached_fallback(cause);
    };
    let fallback = fallback_response(cause);
    let Ok(prepared) = prepare_response_egress(request_method, fallback) else {
        attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
        return detached_fallback(cause);
    };
    match start_prepared(
        prepared,
        Deadline::at_instant(fallback_deadline),
        attempt,
        clock.clone(),
        EgressKind::Fallback,
        connection,
    ) {
        Ok(response) => response,
        Err(mut returned_attempt) => {
            returned_attempt
                .finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
            detached_fallback(cause)
        }
    }
}

fn fallback_response(cause: ResponseEgressOutcome) -> CoreResponse {
    let (status, body) = fallback_parts(cause);
    let mut response = CoreResponse::new(Body::from(body));
    *response.status_mut() = status;
    response
}

fn detached_fallback(cause: ResponseEgressOutcome) -> Response<AxumEgressBody> {
    let (status, body) = fallback_parts(cause);
    let mut response = Response::new(AxumEgressBody::detached(Bytes::from_static(body)));
    *response.status_mut() = status;
    response
}

const fn fallback_parts(cause: ResponseEgressOutcome) -> (StatusCode, &'static [u8]) {
    if matches!(cause, ResponseEgressOutcome::DeadlineExceeded) {
        (StatusCode::GATEWAY_TIMEOUT, TIMEOUT_FALLBACK_BODY)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_FALLBACK_BODY)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::body::Body;
    use edgezero_core::http::{HeaderMap, Method, StatusCode, Version};
    use edgezero_core::response_egress::{
        ResponseEgressAttempt, ResponseEgressHead, ResponseEgressObserver,
        ResponseEgressObserverHandle, ResponseEgressOutcome, ResponseEgressReport,
    };
    use edgezero_core::router::RouteMetadata;
    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures_util::future::poll_fn as poll_future;
    use futures_util::stream::{pending, poll_fn as poll_stream};
    use http_body::Body as _;

    use super::{AxumEgressBody, EgressConnection};

    #[derive(Clone, Default)]
    struct RecordingObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    impl ResponseEgressObserver for RecordingObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
    }

    fn scripted_clock(script: Vec<MonotonicInstant>) -> MonotonicClock {
        let observations = Arc::new(Mutex::new(VecDeque::from(script)));
        MonotonicClock::new(move || {
            observations
                .lock()
                .expect("clock observations")
                .pop_front()
                .expect("clock observation")
        })
    }

    fn attempt(observer: &RecordingObserver, now: MonotonicInstant) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let route = RouteMetadata::new(Method::GET, "/test");
        let head = ResponseEgressHead::new(
            StatusCode::OK,
            Version::HTTP_11,
            &headers,
            now,
            Some(&route),
        );
        ResponseEgressAttempt::new(
            &head,
            now,
            ResponseEgressObserverHandle::new(observer.clone()),
            MonotonicClock::default(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn body_is_lazy_and_accounts_each_frame_before_clean_handoff() {
        let observer = RecordingObserver::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let source_polls = Arc::clone(&polls);
        let source = poll_stream(move |_cx| {
            let poll_index = source_polls.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(match poll_index {
                0 => Some(Ok(Bytes::from_static(b"abc"))),
                1 => Some(Ok(Bytes::from_static(b"de"))),
                _ => None,
            })
        });
        let now = MonotonicInstant::now();
        let clock = MonotonicClock::new(move || now);
        let deadline =
            Deadline::at_instant(now.checked_add(Duration::from_secs(1)).expect("deadline"));
        let connection = EgressConnection::default();
        let body_result = AxumEgressBody::application(
            Body::from_stream(source),
            deadline,
            clock,
            attempt(&observer, now),
            &connection,
        );
        let Ok(registered_body) = body_result else {
            panic!("register body");
        };
        let mut pinned_body = Box::pin(registered_body);

        assert_eq!(polls.load(Ordering::SeqCst), 0);
        let first = poll_future(|cx| pinned_body.as_mut().poll_frame(cx))
            .await
            .expect("first frame")
            .expect("first frame result")
            .into_data()
            .expect("first data");
        assert_eq!(first, Bytes::from_static(b"abc"));
        assert!(observer.0.lock().expect("reports lock").is_empty());

        let second = poll_future(|cx| pinned_body.as_mut().poll_frame(cx))
            .await
            .expect("second frame")
            .expect("second frame result")
            .into_data()
            .expect("second data");
        assert_eq!(second, Bytes::from_static(b"de"));
        assert!(
            poll_future(|cx| pinned_body.as_mut().poll_frame(cx))
                .await
                .is_none()
        );

        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 5);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::HostHandoff);
        assert!(connection.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_unfinished_body_reports_transport_error_once() {
        let observer = RecordingObserver::default();
        let now = MonotonicInstant::now();
        let clock = MonotonicClock::new(move || now);
        let deadline =
            Deadline::at_instant(now.checked_add(Duration::from_secs(1)).expect("deadline"));
        let connection = EgressConnection::default();
        let body_result = AxumEgressBody::application(
            Body::from_stream(pending()),
            deadline,
            clock,
            attempt(&observer, now),
            &connection,
        );
        let Ok(registered_body) = body_result else {
            panic!("register body");
        };

        drop(registered_body);

        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 0);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::TransportError);
        assert!(connection.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_egress_post_ready_deadline_precedence() {
        let observer = RecordingObserver::default();
        let now = MonotonicInstant::now();
        let deadline = now.checked_add(Duration::from_secs(1)).expect("deadline");
        let clock = scripted_clock(vec![now, now, deadline, deadline]);
        let connection = EgressConnection::default();
        let body_result = AxumEgressBody::application(
            Body::from_stream(poll_stream(|_context| {
                Poll::Ready(Some(Ok(Bytes::from_static(b"late"))))
            })),
            Deadline::at_instant(deadline),
            clock,
            attempt(&observer, now),
            &connection,
        );
        let Ok(body) = body_result else {
            panic!("register body");
        };
        let mut pinned_body = Box::pin(body);

        let error = poll_future(|context| pinned_body.as_mut().poll_frame(context))
            .await
            .expect("terminal frame")
            .expect_err("deadline must win over ready source data");

        assert!(error.is_deadline());
        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].bytes_written, 0);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert!(connection.is_empty());
    }
}
