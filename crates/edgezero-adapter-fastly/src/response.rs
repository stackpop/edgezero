#![cfg_attr(
    feature = "fastly",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "portable protocol traits precede the gated Fastly ABI lifecycle implementation"
    )
)]

#[cfg(feature = "fastly")]
#[path = "fastly_response_abi.rs"]
mod fastly_abi;

use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderMap, Method, Response, StatusCode, Uri};
use edgezero_core::response_egress::{
    RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET, ResponseEgressAttempt, ResponseEgressEnvelope,
    ResponseEgressFallbackDisposition, ResponseEgressOutcome, ResponseEgressPolicy,
};
use edgezero_core::response_egress_framing::{
    PreparedResponseEgress, prepare_response_egress, response_egress_source_error_category,
};
use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
use futures::executor;
use futures_util::StreamExt as _;

const INTERNAL_FALLBACK_BODY: &[u8] = b"internal server error";
const TIMEOUT_FALLBACK_BODY: &[u8] = b"response write deadline exceeded";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryError {
    Conversion,
    Deadline,
    Source,
    Transport,
}

impl DeliveryError {
    const fn from_outcome(outcome: ResponseEgressOutcome) -> Self {
        match outcome {
            ResponseEgressOutcome::ConversionError => Self::Conversion,
            ResponseEgressOutcome::DeadlineExceeded => Self::Deadline,
            ResponseEgressOutcome::SourceError => Self::Source,
            ResponseEgressOutcome::ClientDisconnected
            | ResponseEgressOutcome::Completed
            | ResponseEgressOutcome::HostHandoff
            | ResponseEgressOutcome::RequestCancelled
            | ResponseEgressOutcome::TransportError
            | ResponseEgressOutcome::Unspecified
            | _ => Self::Transport,
        }
    }

    const fn outcome(self) -> ResponseEgressOutcome {
        match self {
            Self::Conversion => ResponseEgressOutcome::ConversionError,
            Self::Deadline => ResponseEgressOutcome::DeadlineExceeded,
            Self::Source => ResponseEgressOutcome::SourceError,
            Self::Transport => ResponseEgressOutcome::TransportError,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransmitFailure {
    Postcommit(DeliveryError),
    Precommit {
        cause: ResponseEgressOutcome,
        error: DeliveryError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransmitKind {
    Application,
    Fallback,
}

trait EgressSink {
    type Error;

    fn abandon(&mut self) -> Result<(), Self::Error>;
    fn finish(&mut self) -> Result<(), Self::Error>;
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error>;
}

trait EgressCommitter {
    type Error;
    type Prepared;
    type Sink: EgressSink;

    fn commit(&mut self, prepared: Self::Prepared) -> Result<Self::Sink, Self::Error>;
    fn prepare(
        &mut self,
        status: StatusCode,
        headers: &HeaderMap,
        transmits_body: bool,
    ) -> Result<Self::Prepared, Self::Error>;
}

#[cfg(feature = "fastly")]
mod platform {
    use super::{
        DeliveryError, EgressCommitter, EgressSink, HeaderMap, ResponseEgressEnvelope, StatusCode,
        send_egress_with,
    };
    use std::mem;

    use edgezero_core::http::header::CONTENT_LENGTH;
    use fastly_shared::{INVALID_BODY_HANDLE, INVALID_RESPONSE_HANDLE};
    use fastly_sys::{BodyHandle as FastlyBodyHandle, ResponseHandle as FastlyResponseHandle};

    use super::fastly_abi::{self, FastlyAbiError};

    const fn public_message(error: DeliveryError) -> &'static str {
        match error {
            DeliveryError::Conversion => "response conversion failed",
            DeliveryError::Deadline => "response write deadline exceeded",
            DeliveryError::Source => "response body source failed",
            DeliveryError::Transport => "response transport failed",
        }
    }

    pub(super) struct FastlyPrepared {
        body: FastlyBodyHandle,
        response: FastlyResponseHandle,
    }

    impl FastlyPrepared {
        fn commit(mut self) -> Result<FastlyBodySink, FastlyAbiError> {
            let response = mem::replace(&mut self.response, INVALID_RESPONSE_HANDLE);
            let body = mem::replace(&mut self.body, INVALID_BODY_HANDLE);
            if fastly_abi::send_streaming(response, body).is_err() {
                let _result = fastly_abi::abandon_body(body);
                return Err(FastlyAbiError);
            }
            Ok(FastlyBodySink(body))
        }

        fn new(
            status: StatusCode,
            headers: &HeaderMap,
            transmits_body: bool,
        ) -> Result<Self, FastlyAbiError> {
            let response = fastly_abi::new_response()?;
            let mut prepared = Self {
                body: INVALID_BODY_HANDLE,
                response,
            };
            prepared.body = fastly_abi::new_body()?;
            fastly_abi::set_status(prepared.response, status.as_u16())?;
            for (name, value) in headers {
                fastly_abi::append_header(
                    prepared.response,
                    name.as_str().as_bytes(),
                    value.as_bytes(),
                )?;
            }
            if transmits_body && headers.contains_key(CONTENT_LENGTH) {
                fastly_abi::set_manual_framing(prepared.response)?;
            }
            Ok(prepared)
        }
    }

    impl Drop for FastlyPrepared {
        fn drop(&mut self) {
            if self.body != INVALID_BODY_HANDLE {
                let _result = fastly_abi::close_body(self.body);
                self.body = INVALID_BODY_HANDLE;
            }
            if self.response != INVALID_RESPONSE_HANDLE {
                let _result = fastly_abi::close_response(self.response);
                self.response = INVALID_RESPONSE_HANDLE;
            }
        }
    }

    pub(super) struct FastlyBodySink(FastlyBodyHandle);

    impl Drop for FastlyBodySink {
        fn drop(&mut self) {
            if self.0 != INVALID_BODY_HANDLE {
                let _result = fastly_abi::abandon_body(self.0);
                self.0 = INVALID_BODY_HANDLE;
            }
        }
    }

    impl EgressSink for FastlyBodySink {
        type Error = FastlyAbiError;

        fn abandon(&mut self) -> Result<(), Self::Error> {
            if self.0 == INVALID_BODY_HANDLE {
                return Ok(());
            }
            let handle = mem::replace(&mut self.0, INVALID_BODY_HANDLE);
            fastly_abi::abandon_body(handle)
        }

        fn finish(&mut self) -> Result<(), Self::Error> {
            if self.0 == INVALID_BODY_HANDLE {
                return Err(FastlyAbiError);
            }
            let handle = mem::replace(&mut self.0, INVALID_BODY_HANDLE);
            fastly_abi::close_body(handle)
        }

        fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            if self.0 == INVALID_BODY_HANDLE {
                return Err(FastlyAbiError);
            }
            fastly_abi::write_body(self.0, bytes)
        }
    }

    pub(super) struct FastlyCommitter;

    impl EgressCommitter for FastlyCommitter {
        type Error = FastlyAbiError;
        type Prepared = FastlyPrepared;
        type Sink = FastlyBodySink;

        fn commit(&mut self, prepared: Self::Prepared) -> Result<Self::Sink, Self::Error> {
            prepared.commit()
        }

        fn prepare(
            &mut self,
            status: StatusCode,
            headers: &HeaderMap,
            transmits_body: bool,
        ) -> Result<Self::Prepared, Self::Error> {
            FastlyPrepared::new(status, headers, transmits_body)
        }
    }

    /// Sends one core response through Fastly's unbuffered downstream body handle.
    ///
    /// # Errors
    /// Returns a category-only Fastly error when the committed response cannot reach its terminal
    /// body-handle boundary. Precommit conversion and deadline failures first attempt the bounded
    /// adapter fallback on the same response-egress attempt.
    pub(crate) fn send_egress_response(
        egress: ResponseEgressEnvelope,
    ) -> Result<(), fastly::Error> {
        send_egress_with(egress, &mut FastlyCommitter)
            .map_err(|error| fastly::Error::msg(public_message(error)))
    }
}

#[cfg(feature = "fastly")]
pub(crate) use platform::send_egress_response;

fn send_egress_with<Committer>(
    egress: ResponseEgressEnvelope,
    committer: &mut Committer,
) -> Result<(), DeliveryError>
where
    Committer: EgressCommitter,
{
    let request_method = egress.request_method().clone();
    match egress.begin() {
        Ok((prepared, policy, mut attempt, clock)) => {
            match transmit_prepared(
                prepared,
                policy,
                &mut attempt,
                &clock,
                TransmitKind::Application,
                committer,
            ) {
                Ok(()) => Ok(()),
                Err(TransmitFailure::Precommit { cause, .. }) => {
                    send_fallback(&request_method, cause, attempt, &clock, committer)
                }
                Err(TransmitFailure::Postcommit(error)) => Err(error),
            }
        }
        Err(failure) => {
            let (attempt, clock, cause) = failure.into_parts();
            send_fallback(&request_method, cause, attempt, &clock, committer)
        }
    }
}

fn send_fallback<Committer>(
    request_method: &Method,
    original_cause: ResponseEgressOutcome,
    mut attempt: ResponseEgressAttempt,
    clock: &MonotonicClock,
    committer: &mut Committer,
) -> Result<(), DeliveryError>
where
    Committer: EgressCommitter,
{
    if !attempt.begin_fallback(original_cause) {
        log::error!("response-egress fallback state transition failed");
        attempt.terminate(ResponseEgressOutcome::ConversionError, clock.now());
        return Err(DeliveryError::Conversion);
    }

    let fallback_started_at = clock.now();
    let fallback_deadline = fallback_started_at
        .checked_add(RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET)
        .unwrap_or(fallback_started_at);
    let fallback_response = fallback_response(original_cause);
    let prepared = match prepare_response_egress(request_method, fallback_response) {
        Ok(prepared) => prepared,
        Err(_error) => {
            log::error!("response-egress fallback framing failed");
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
            return Err(DeliveryError::Conversion);
        }
    };
    let policy = ResponseEgressPolicy {
        write_deadline: Deadline::at_instant(fallback_deadline),
    };
    match transmit_prepared(
        prepared,
        policy,
        &mut attempt,
        clock,
        TransmitKind::Fallback,
        committer,
    ) {
        Ok(()) => Ok(()),
        Err(TransmitFailure::Postcommit(error) | TransmitFailure::Precommit { error, .. }) => {
            Err(error)
        }
    }
}

fn fallback_response(cause: ResponseEgressOutcome) -> Response {
    let (status, body) = if cause == ResponseEgressOutcome::DeadlineExceeded {
        (StatusCode::GATEWAY_TIMEOUT, TIMEOUT_FALLBACK_BODY)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_FALLBACK_BODY)
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
}

fn transmit_prepared<Committer>(
    prepared: PreparedResponseEgress,
    policy: ResponseEgressPolicy,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
    kind: TransmitKind,
    committer: &mut Committer,
) -> Result<(), TransmitFailure>
where
    Committer: EgressCommitter,
{
    if policy.write_deadline.is_expired_at(clock.now()) {
        return precommit_failure(
            kind,
            attempt,
            clock,
            ResponseEgressOutcome::DeadlineExceeded,
        );
    }

    let transmits_body = prepared.transmits_body();
    let (parts, body) = prepared.into_response().into_parts();
    let prepared_head = match committer.prepare(parts.status, &parts.headers, transmits_body) {
        Ok(prepared_head) => prepared_head,
        Err(_error) => {
            log::error!("response-egress Fastly head preparation failed");
            let cause = if policy.write_deadline.is_expired_at(clock.now()) {
                ResponseEgressOutcome::DeadlineExceeded
            } else {
                ResponseEgressOutcome::ConversionError
            };
            return precommit_failure(kind, attempt, clock, cause);
        }
    };

    let commit_started_at = clock.now();
    if policy.write_deadline.is_expired_at(commit_started_at) {
        return precommit_failure(
            kind,
            attempt,
            clock,
            ResponseEgressOutcome::DeadlineExceeded,
        );
    }
    if !attempt.begin_writing() {
        log::error!("response-egress writing state transition failed");
        settle_failure(
            kind,
            attempt,
            ResponseEgressOutcome::TransportError,
            clock.now(),
        );
        return Err(TransmitFailure::Postcommit(DeliveryError::Transport));
    }

    let mut sink = match committer.commit(prepared_head) {
        Ok(sink) => sink,
        Err(_error) => {
            log::error!("response-egress Fastly downstream commit failed");
            settle_failure(
                kind,
                attempt,
                ResponseEgressOutcome::TransportError,
                clock.now(),
            );
            return Err(TransmitFailure::Postcommit(DeliveryError::Transport));
        }
    };

    let committed_at = clock.now();
    if policy.write_deadline.is_expired_at(committed_at) {
        abandon(&mut sink);
        settle_failure(
            kind,
            attempt,
            ResponseEgressOutcome::DeadlineExceeded,
            committed_at,
        );
        return Err(TransmitFailure::Postcommit(DeliveryError::Deadline));
    }

    let pump_result = pump_body(body, &mut sink, policy.write_deadline, attempt, clock);
    if let Err(error) = pump_result {
        abandon(&mut sink);
        settle_failure(kind, attempt, error.outcome(), clock.now());
        return Err(TransmitFailure::Postcommit(error));
    }

    if policy.write_deadline.is_expired_at(clock.now()) {
        abandon(&mut sink);
        settle_failure(
            kind,
            attempt,
            ResponseEgressOutcome::DeadlineExceeded,
            clock.now(),
        );
        return Err(TransmitFailure::Postcommit(DeliveryError::Deadline));
    }
    // Fastly consumes the native handle before reporting a close failure, so no
    // second abandon operation is available after this boundary.
    let finish_result = sink.finish();
    let observed_at = clock.now();
    let finish_failed = finish_result.is_err();
    let finish_error = if policy.write_deadline.is_expired_at(observed_at) {
        Some(DeliveryError::Deadline)
    } else if finish_failed {
        Some(DeliveryError::Transport)
    } else {
        None
    };
    if let Some(error) = finish_error {
        if finish_failed {
            log::error!("response-egress Fastly body finish failed");
        }
        settle_failure(kind, attempt, error.outcome(), observed_at);
        return Err(TransmitFailure::Postcommit(error));
    }

    settle_success(kind, attempt, observed_at);
    Ok(())
}

fn pump_body<Sink>(
    body: Body,
    sink: &mut Sink,
    deadline: Deadline,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
) -> Result<(), DeliveryError>
where
    Sink: EgressSink,
{
    match body {
        Body::Once(bytes) => write_chunk(bytes.as_ref(), sink, deadline, attempt, clock),
        Body::Stream(mut source) => loop {
            ensure_deadline(deadline, clock)?;
            let next = executor::block_on(source.next());
            ensure_deadline(deadline, clock)?;
            match next {
                Some(Ok(bytes)) => {
                    write_chunk(bytes.as_ref(), sink, deadline, attempt, clock)?;
                }
                Some(Err(error)) => {
                    log::warn!(
                        "response-egress Fastly body source failed after commit: {}",
                        response_egress_source_error_category(&error)
                    );
                    return Err(DeliveryError::Source);
                }
                None => return Ok(()),
            }
        },
    }
}

fn write_chunk<Sink>(
    bytes: &[u8],
    sink: &mut Sink,
    deadline: Deadline,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
) -> Result<(), DeliveryError>
where
    Sink: EgressSink,
{
    let mut offset = 0;
    while offset < bytes.len() {
        ensure_deadline(deadline, clock)?;
        let remaining = bytes.get(offset..).ok_or(DeliveryError::Transport)?;
        let write_result = sink.write(remaining);
        let observed_at = clock.now();
        if deadline.is_expired_at(observed_at) {
            if let Ok(accepted_size) = write_result
                && accepted_size <= remaining.len()
            {
                let accepted_u64 =
                    u64::try_from(accepted_size).map_err(|_error| DeliveryError::Transport)?;
                if !attempt.account_bytes(accepted_u64, observed_at) {
                    return Err(DeliveryError::Transport);
                }
            }
            return Err(DeliveryError::Deadline);
        }
        let accepted_size = match write_result {
            Ok(0) => return Err(DeliveryError::Transport),
            Ok(accepted) if accepted <= remaining.len() => accepted,
            Ok(_) | Err(_) => return Err(DeliveryError::Transport),
        };
        let accepted_u64 =
            u64::try_from(accepted_size).map_err(|_error| DeliveryError::Transport)?;
        if !attempt.account_bytes(accepted_u64, observed_at) {
            return Err(DeliveryError::Transport);
        }
        offset = offset
            .checked_add(accepted_size)
            .ok_or(DeliveryError::Transport)?;
    }
    Ok(())
}

fn ensure_deadline(deadline: Deadline, clock: &MonotonicClock) -> Result<(), DeliveryError> {
    if deadline.is_expired_at(clock.now()) {
        Err(DeliveryError::Deadline)
    } else {
        Ok(())
    }
}

fn abandon<Sink>(sink: &mut Sink)
where
    Sink: EgressSink,
{
    if sink.abandon().is_err() {
        log::error!("response-egress Fastly body abandon failed");
    }
}

fn precommit_failure(
    kind: TransmitKind,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
    cause: ResponseEgressOutcome,
) -> Result<(), TransmitFailure> {
    let error = DeliveryError::from_outcome(cause);
    if kind == TransmitKind::Fallback {
        attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
    }
    Err(TransmitFailure::Precommit { cause, error })
}

fn settle_failure(
    kind: TransmitKind,
    attempt: &mut ResponseEgressAttempt,
    outcome: ResponseEgressOutcome,
    observed_at: MonotonicInstant,
) {
    match kind {
        TransmitKind::Application => {
            attempt.terminate(outcome, observed_at);
        }
        TransmitKind::Fallback => {
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, observed_at);
        }
    }
}

fn settle_success(
    kind: TransmitKind,
    attempt: &mut ResponseEgressAttempt,
    observed_at: MonotonicInstant,
) {
    match kind {
        TransmitKind::Application => {
            attempt.complete(observed_at);
        }
        TransmitKind::Fallback => {
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Completed, observed_at);
        }
    }
}

pub(crate) fn parse_uri(uri: &str) -> Result<Uri, EdgeError> {
    uri.parse::<Uri>()
        .map_err(|err| EdgeError::bad_request(format!("invalid request URI: {err}")))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::app::App;
    use edgezero_core::context::RequestContext;
    use edgezero_core::http::{Version, request_builder, response_builder};
    use edgezero_core::ingress::{IngressDispatchOutcome, IngressFraming, IngressHeadAccounting};
    use edgezero_core::response_egress::{
        ResponseEgressBodyKind, ResponseEgressCompletion, ResponseEgressHead,
        ResponseEgressObserver, ResponseEgressObserverHandle, ResponseEgressReport,
    };
    use edgezero_core::router::RouterService;
    use edgezero_core::time::MonotonicInstant;
    use futures_util::stream::{self, poll_fn};

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    impl RecordingObserver {
        fn reports(&self) -> Vec<ResponseEgressReport> {
            self.0.lock().expect("reports lock").clone()
        }
    }

    impl ResponseEgressObserver for RecordingObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
    }

    fn response_egress(outcome: IngressDispatchOutcome) -> ResponseEgressEnvelope {
        let IngressDispatchOutcome::Response(envelope) = outcome else {
            panic!("expected response egress");
        };
        envelope
    }

    #[derive(Default)]
    struct SinkState {
        abandon: MockTerminal,
        accepted: Vec<u8>,
        commits: Vec<(StatusCode, HeaderMap, bool)>,
        events: Vec<&'static str>,
        finish: MockTerminal,
        write_plan: VecDeque<Result<usize, ()>>,
    }

    struct MockSink(Rc<RefCell<SinkState>>);

    #[derive(Clone, Copy, Default)]
    enum MockTerminal {
        Error,
        #[default]
        Success,
    }

    impl EgressSink for MockSink {
        type Error = ();

        fn abandon(&mut self) -> Result<(), Self::Error> {
            let mut state = self.0.borrow_mut();
            state.events.push("abandon");
            match state.abandon {
                MockTerminal::Error => Err(()),
                MockTerminal::Success => Ok(()),
            }
        }

        fn finish(&mut self) -> Result<(), Self::Error> {
            let mut state = self.0.borrow_mut();
            state.events.push("finish");
            match state.finish {
                MockTerminal::Error => Err(()),
                MockTerminal::Success => Ok(()),
            }
        }

        fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            let mut state = self.0.borrow_mut();
            state.events.push("write");
            let result = state.write_plan.pop_front().unwrap_or(Ok(bytes.len()));
            if let Ok(accepted) = result
                && accepted <= bytes.len()
            {
                state.accepted.extend_from_slice(&bytes[..accepted]);
            }
            result
        }
    }

    struct MockCommitter {
        commit_plan: VecDeque<MockCommit>,
        prepare_plan: VecDeque<Result<(), ()>>,
        state: Rc<RefCell<SinkState>>,
    }

    enum MockCommit {
        Accept,
        Error,
    }

    impl MockCommitter {
        fn new(state: Rc<RefCell<SinkState>>) -> Self {
            Self {
                commit_plan: VecDeque::new(),
                prepare_plan: VecDeque::new(),
                state,
            }
        }
    }

    impl EgressCommitter for MockCommitter {
        type Error = ();
        type Prepared = (StatusCode, HeaderMap, bool);
        type Sink = MockSink;

        fn commit(&mut self, prepared: Self::Prepared) -> Result<Self::Sink, Self::Error> {
            self.state.borrow_mut().commits.push(prepared);
            match self.commit_plan.pop_front().unwrap_or(MockCommit::Accept) {
                MockCommit::Accept => Ok(MockSink(Rc::clone(&self.state))),
                MockCommit::Error => Err(()),
            }
        }

        fn prepare(
            &mut self,
            status: StatusCode,
            headers: &HeaderMap,
            transmits_body: bool,
        ) -> Result<Self::Prepared, Self::Error> {
            self.prepare_plan.pop_front().unwrap_or(Ok(()))?;
            Ok((status, headers.clone(), transmits_body))
        }
    }

    fn stable_clock(now: MonotonicInstant) -> MonotonicClock {
        MonotonicClock::new(move || now)
    }

    fn attempt(observer: &RecordingObserver, now: MonotonicInstant) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let head =
            ResponseEgressHead::new(StatusCode::OK, Version::HTTP_11, &headers, None, now, None);
        ResponseEgressAttempt::new(
            &head,
            now,
            ResponseEgressCompletion::empty(),
            ResponseEgressObserverHandle::new(observer.clone()),
            MonotonicClock::default(),
        )
    }

    fn future_policy(now: MonotonicInstant) -> ResponseEgressPolicy {
        ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(
                now.checked_add(Duration::from_secs(10)).expect("deadline"),
            ),
        }
    }

    fn prepared(body: Body) -> PreparedResponseEgress {
        prepare_response_egress(
            &Method::GET,
            response_builder()
                .status(StatusCode::OK)
                .body(body)
                .expect("response"),
        )
        .expect("prepared response")
    }

    fn transmit(
        body: Body,
        state: Rc<RefCell<SinkState>>,
    ) -> (Result<(), TransmitFailure>, Vec<ResponseEgressReport>) {
        let now = MonotonicInstant::now();
        let clock = stable_clock(now);
        let observer = RecordingObserver::default();
        let mut attempt = attempt(&observer, now);
        let mut committer = MockCommitter::new(state);
        let result = transmit_prepared(
            prepared(body),
            future_policy(now),
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );
        drop(attempt);
        (result, observer.reports())
    }

    async fn invalid_length_response(_context: RequestContext) -> Result<Response, EdgeError> {
        response_builder()
            .status(StatusCode::OK)
            .header("content-length", "2")
            .body(Body::from("x"))
            .map_err(EdgeError::internal)
    }

    async fn ordinary_response(_context: RequestContext) -> Result<Response, EdgeError> {
        response_builder()
            .status(StatusCode::OK)
            .body(Body::from("application"))
            .map_err(EdgeError::internal)
    }

    #[test]
    fn parse_valid_uri() {
        let uri = parse_uri("https://example.com/foo").expect("uri");
        assert_eq!(uri.to_string(), "https://example.com/foo");
    }

    #[test]
    fn parse_invalid_uri() {
        let err = parse_uri("::invalid uri::").expect_err("should fail");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    #[cfg(all(feature = "fastly", target_arch = "wasm32"))]
    fn raw_abi_commits_repeated_set_cookie_and_streams_body() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "a=1".parse().expect("header"));
        headers.append("set-cookie", "b=2".parse().expect("header"));
        let mut committer = platform::FastlyCommitter;
        let prepared = committer
            .prepare(StatusCode::OK, &headers, true)
            .expect("prepare raw response");
        let mut sink = committer.commit(prepared).expect("commit raw response");
        assert_eq!(sink.write(b"raw-egress"), Ok(10));
        assert_eq!(sink.finish(), Ok(()));
    }

    #[test]
    fn streams_empty_once_and_multiple_chunks_with_short_writes() {
        for (body, plan, expected) in [
            (Body::empty(), VecDeque::new(), b"".as_slice()),
            (
                Body::from("hello"),
                VecDeque::from([Ok(2), Ok(3)]),
                b"hello".as_slice(),
            ),
            (
                Body::stream(stream::iter([
                    Bytes::from_static(b"hello "),
                    Bytes::from_static(b"world"),
                ])),
                VecDeque::new(),
                b"hello world".as_slice(),
            ),
        ] {
            let state = Rc::new(RefCell::new(SinkState {
                write_plan: plan,
                ..SinkState::default()
            }));
            let (result, reports) = transmit(body, Rc::clone(&state));
            assert_eq!(result, Ok(()));
            assert_eq!(state.borrow().accepted, expected);
            assert_eq!(state.borrow().events.last(), Some(&"finish"));
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::Completed);
            assert_eq!(
                reports[0].bytes_written,
                u64::try_from(expected.len()).expect("body length")
            );
        }
    }

    #[test]
    fn zero_write_write_error_source_error_and_finish_error_abort_once() {
        let cases = [
            (
                Body::from("x"),
                VecDeque::from([Ok(0)]),
                false,
                ResponseEgressOutcome::TransportError,
            ),
            (
                Body::from("x"),
                VecDeque::from([Err(())]),
                false,
                ResponseEgressOutcome::TransportError,
            ),
            (
                Body::from_stream(stream::iter([Err(EdgeError::internal(anyhow::anyhow!(
                    "private source detail"
                )))])),
                VecDeque::new(),
                false,
                ResponseEgressOutcome::SourceError,
            ),
            (
                Body::empty(),
                VecDeque::new(),
                true,
                ResponseEgressOutcome::TransportError,
            ),
        ];
        for (body, write_plan, finish_error, expected) in cases {
            let state = Rc::new(RefCell::new(SinkState {
                abandon: MockTerminal::Error,
                finish: if finish_error {
                    MockTerminal::Error
                } else {
                    MockTerminal::Success
                },
                write_plan,
                ..SinkState::default()
            }));
            let (result, reports) = transmit(body, Rc::clone(&state));
            assert!(matches!(result, Err(TransmitFailure::Postcommit(_))));
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, expected);
            let expected_terminal = if finish_error { "finish" } else { "abandon" };
            assert_eq!(state.borrow().events.last(), Some(&expected_terminal));
        }
    }

    #[test]
    fn accepted_prefix_at_deadline_is_accounted_before_deadline_wins() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        let clock = MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) < 4 {
                start
            } else {
                deadline
            }
        });
        let report_observer = RecordingObserver::default();
        let mut attempt = attempt(&report_observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        let result = transmit_prepared(
            prepared(Body::from("abc")),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            },
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );
        assert_eq!(
            result,
            Err(TransmitFailure::Postcommit(DeliveryError::Deadline))
        );
        let reports = report_observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].bytes_written, 3);
    }

    #[test]
    fn deadline_at_commit_abandons_without_polling_or_writing() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        let clock = MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) < 2 {
                start
            } else {
                deadline
            }
        });
        let polls = Rc::new(Cell::new(0_usize));
        let observed_polls = Rc::clone(&polls);
        let body = Body::from_stream(poll_fn(move |_| {
            observed_polls.set(observed_polls.get() + 1);
            Poll::Pending
        }));
        let report_observer = RecordingObserver::default();
        let mut attempt = attempt(&report_observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        let result = transmit_prepared(
            prepared(body),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            },
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(
            result,
            Err(TransmitFailure::Postcommit(DeliveryError::Deadline))
        );
        assert_eq!(polls.get(), 0);
        assert!(state.borrow().accepted.is_empty());
        assert_eq!(state.borrow().events, ["abandon"]);
        assert_eq!(
            report_observer.reports()[0].outcome,
            ResponseEgressOutcome::DeadlineExceeded
        );
    }

    #[test]
    fn deadline_after_head_preparation_prevents_irreversible_commit() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        let clock = MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) == 0 {
                start
            } else {
                deadline
            }
        });
        let report_observer = RecordingObserver::default();
        let mut attempt = attempt(&report_observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let result = transmit_prepared(
            prepared(Body::from("uncommitted")),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            },
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(
            result,
            Err(TransmitFailure::Precommit {
                cause: ResponseEgressOutcome::DeadlineExceeded,
                error: DeliveryError::Deadline,
            })
        );
        assert!(state.borrow().commits.is_empty());
        assert!(report_observer.reports().is_empty());
    }

    #[test]
    fn deadline_at_source_poll_abandons_before_writing_the_chunk() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        let clock = MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) < 4 {
                start
            } else {
                deadline
            }
        });
        let polls = Rc::new(Cell::new(0_usize));
        let observed_polls = Rc::clone(&polls);
        let body = Body::from_stream(poll_fn(move |_| {
            observed_polls.set(observed_polls.get() + 1);
            Poll::Ready(Some(Ok(Bytes::from_static(b"late"))))
        }));
        let report_observer = RecordingObserver::default();
        let mut attempt = attempt(&report_observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let result = transmit_prepared(
            prepared(body),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            },
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(
            result,
            Err(TransmitFailure::Postcommit(DeliveryError::Deadline))
        );
        assert_eq!(polls.get(), 1);
        assert!(state.borrow().accepted.is_empty());
        assert_eq!(state.borrow().events, ["abandon"]);
        let reports = report_observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].bytes_written, 0);
    }

    #[test]
    fn deadline_at_finish_wins_after_the_handle_is_consumed() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        let clock = MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) < 4 {
                start
            } else {
                deadline
            }
        });
        let report_observer = RecordingObserver::default();
        let mut attempt = attempt(&report_observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let result = transmit_prepared(
            prepared(Body::empty()),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            },
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(
            result,
            Err(TransmitFailure::Postcommit(DeliveryError::Deadline))
        );
        assert_eq!(state.borrow().events, ["finish"]);
        let reports = report_observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
    }

    #[test]
    fn head_suppression_releases_source_without_polling() {
        struct DropSignal(Rc<Cell<usize>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let polls = Rc::new(Cell::new(0_usize));
        let drops = Rc::new(Cell::new(0_usize));
        let observed_polls = Rc::clone(&polls);
        let signal = DropSignal(Rc::clone(&drops));
        let body = Body::from_stream(poll_fn(move |_| {
            let _keep_alive = &signal;
            observed_polls.set(observed_polls.get() + 1);
            Poll::Pending
        }));
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "99")
            .body(body)
            .expect("response");
        let prepared = prepare_response_egress(&Method::HEAD, response).expect("prepared");
        let start = MonotonicInstant::now();
        let clock = stable_clock(start);
        let observer = RecordingObserver::default();
        let mut attempt = attempt(&observer, start);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        let result = transmit_prepared(
            prepared,
            future_policy(start),
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(polls.get(), 0);
        assert_eq!(drops.get(), 1);
        assert!(state.borrow().accepted.is_empty());
        assert_eq!(observer.reports()[0].bytes_written, 0);
    }

    #[test]
    fn body_suppressed_response_disables_manual_framing() {
        for (method, status) in [
            (Method::HEAD, StatusCode::OK),
            (Method::GET, StatusCode::NOT_MODIFIED),
        ] {
            let response = response_builder()
                .status(status)
                .header("content-length", "99")
                .body(Body::from("suppressed"))
                .expect("response");
            let prepared = prepare_response_egress(&method, response).expect("prepared");
            let now = MonotonicInstant::now();
            let clock = stable_clock(now);
            let observer = RecordingObserver::default();
            let mut attempt = attempt(&observer, now);
            let state = Rc::new(RefCell::new(SinkState::default()));
            let mut committer = MockCommitter::new(Rc::clone(&state));

            let result = transmit_prepared(
                prepared,
                future_policy(now),
                &mut attempt,
                &clock,
                TransmitKind::Application,
                &mut committer,
            );

            assert_eq!(result, Ok(()));
            let observed_state = state.borrow();
            let (committed_status, headers, transmits_body) = &observed_state.commits[0];
            assert_eq!(*committed_status, status);
            assert_eq!(headers.get("content-length").expect("length"), "99");
            assert!(!transmits_body);
            assert!(observed_state.accepted.is_empty());
        }
    }

    #[test]
    fn payload_response_allows_manual_framing() {
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "3")
            .body(Body::from("abc"))
            .expect("response");
        let prepared = prepare_response_egress(&Method::GET, response).expect("prepared");
        let now = MonotonicInstant::now();
        let clock = stable_clock(now);
        let observer = RecordingObserver::default();
        let mut attempt = attempt(&observer, now);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let result = transmit_prepared(
            prepared,
            future_policy(now),
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(result, Ok(()));
        let observed_state = state.borrow();
        let (_, headers, transmits_body) = &observed_state.commits[0];
        assert_eq!(headers.get("content-length").expect("length"), "3");
        assert!(*transmits_body);
        assert_eq!(observed_state.accepted, b"abc");
    }

    #[test]
    fn framing_failure_sends_bounded_fallback_with_original_cause() {
        let mut app = App::new(
            RouterService::builder()
                .get("/", invalid_length_response)
                .build(),
        );
        let observer = RecordingObserver::default();
        app.set_response_egress_observer(observer.clone());
        let request_start = app.monotonic_now();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let egress = response_egress(
            executor::block_on(app.dispatch_ingress(
                request,
                request_start,
                IngressHeadAccounting::HostManaged,
                IngressFraming::HostManaged,
            ))
            .expect("egress"),
        );
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        assert_eq!(send_egress_with(egress, &mut committer), Ok(()));

        let snapshot = state.borrow();
        assert_eq!(snapshot.commits.len(), 1);
        assert_eq!(snapshot.commits[0].0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(snapshot.accepted, INTERNAL_FALLBACK_BODY);
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
    }

    #[test]
    fn expired_application_deadline_uses_independent_fallback_budget() {
        let mut app = App::new(RouterService::builder().get("/", ordinary_response).build());
        let now = MonotonicInstant::now();
        app.set_monotonic_clock(stable_clock(now));
        app.set_response_egress_policy(move |_, _| ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(now),
        });
        let observer = RecordingObserver::default();
        app.set_response_egress_observer(observer.clone());
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let egress = response_egress(
            executor::block_on(app.dispatch_ingress(
                request,
                now,
                IngressHeadAccounting::HostManaged,
                IngressFraming::HostManaged,
            ))
            .expect("egress"),
        );
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        assert_eq!(send_egress_with(egress, &mut committer), Ok(()));
        assert_eq!(state.borrow().commits[0].0, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(state.borrow().accepted, TIMEOUT_FALLBACK_BODY);
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn head_fallback_suppresses_payload_and_policy_panic_stays_one_attempt() {
        let mut app = App::new(
            RouterService::builder()
                .route("/", Method::HEAD, ordinary_response)
                .build(),
        );
        app.set_response_egress_policy(|_, _| panic!("private policy payload"));
        let observer = RecordingObserver::default();
        app.set_response_egress_observer(observer.clone());
        let request_start = app.monotonic_now();
        let request = request_builder()
            .method(Method::HEAD)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let egress = response_egress(
            executor::block_on(app.dispatch_ingress(
                request,
                request_start,
                IngressHeadAccounting::HostManaged,
                IngressFraming::HostManaged,
            ))
            .expect("egress"),
        );
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        assert_eq!(send_egress_with(egress, &mut committer), Ok(()));
        assert!(state.borrow().accepted.is_empty());
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(reports[0].bytes_written, 0);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
    }

    #[test]
    fn fallback_safety_deadline_aborts_and_preserves_original_cause() {
        let mut app = App::new(
            RouterService::builder()
                .get("/", invalid_length_response)
                .build(),
        );
        let start = MonotonicInstant::now();
        let safety_deadline = start
            .checked_add(RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET)
            .expect("safety deadline");
        let reads = Arc::new(AtomicUsize::new(0));
        let clock_reads = Arc::clone(&reads);
        app.set_monotonic_clock(MonotonicClock::new(move || {
            if clock_reads.fetch_add(1, Ordering::SeqCst) < 4 {
                start
            } else {
                safety_deadline
            }
        }));
        let observer = RecordingObserver::default();
        app.set_response_egress_observer(observer.clone());
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let egress = response_egress(
            executor::block_on(app.dispatch_ingress(
                request,
                start,
                IngressHeadAccounting::HostManaged,
                IngressFraming::HostManaged,
            ))
            .expect("egress"),
        );
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));
        assert_eq!(
            send_egress_with(egress, &mut committer),
            Err(DeliveryError::Deadline)
        );
        assert!(state.borrow().accepted.is_empty());
        assert_eq!(state.borrow().events.last(), Some(&"abandon"));
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Aborted)
        );
    }

    #[test]
    fn fallback_partial_write_and_finish_failure_preserve_conversion_cause() {
        for finish_error in [false, true] {
            let mut app = App::new(
                RouterService::builder()
                    .get("/", invalid_length_response)
                    .build(),
            );
            let observer = RecordingObserver::default();
            app.set_response_egress_observer(observer.clone());
            let request_start = app.monotonic_now();
            let request = request_builder()
                .method(Method::GET)
                .uri("/")
                .body(Body::empty())
                .expect("request");
            let egress = response_egress(
                executor::block_on(app.dispatch_ingress(
                    request,
                    request_start,
                    IngressHeadAccounting::HostManaged,
                    IngressFraming::HostManaged,
                ))
                .expect("egress"),
            );
            let state = Rc::new(RefCell::new(SinkState {
                finish: if finish_error {
                    MockTerminal::Error
                } else {
                    MockTerminal::Success
                },
                write_plan: if finish_error {
                    VecDeque::new()
                } else {
                    VecDeque::from([Ok(3), Err(())])
                },
                ..SinkState::default()
            }));
            let mut committer = MockCommitter::new(Rc::clone(&state));
            assert!(send_egress_with(egress, &mut committer).is_err());
            let reports = observer.reports();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
            assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
            assert_eq!(
                reports[0].fallback_disposition,
                Some(ResponseEgressFallbackDisposition::Aborted)
            );
            if !finish_error {
                assert_eq!(reports[0].bytes_written, 3);
            }
        }
    }

    #[test]
    fn head_preparation_error_uses_one_successful_fallback_attempt() {
        let mut app = App::new(RouterService::builder().get("/", ordinary_response).build());
        let observer = RecordingObserver::default();
        app.set_response_egress_observer(observer.clone());
        let request_start = app.monotonic_now();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let egress = response_egress(
            executor::block_on(app.dispatch_ingress(
                request,
                request_start,
                IngressHeadAccounting::HostManaged,
                IngressFraming::HostManaged,
            ))
            .expect("egress"),
        );
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter {
            commit_plan: VecDeque::new(),
            prepare_plan: VecDeque::from([Err(()), Ok(())]),
            state: Rc::clone(&state),
        };
        assert_eq!(send_egress_with(egress, &mut committer), Ok(()));
        assert_eq!(state.borrow().commits.len(), 1);
        assert_eq!(
            state.borrow().commits[0].0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
    }

    #[test]
    fn downstream_commit_error_is_postcommit_and_does_not_send_fallback() {
        let now = MonotonicInstant::now();
        let clock = stable_clock(now);
        let observer = RecordingObserver::default();
        let mut attempt = attempt(&observer, now);
        let state = Rc::new(RefCell::new(SinkState::default()));
        let mut committer = MockCommitter {
            commit_plan: VecDeque::from([MockCommit::Error]),
            prepare_plan: VecDeque::new(),
            state: Rc::clone(&state),
        };

        let result = transmit_prepared(
            prepared(Body::from("application")),
            future_policy(now),
            &mut attempt,
            &clock,
            TransmitKind::Application,
            &mut committer,
        );

        assert_eq!(
            result,
            Err(TransmitFailure::Postcommit(DeliveryError::Transport))
        );
        assert_eq!(state.borrow().commits.len(), 1);
        assert_eq!(observer.reports().len(), 1);
        assert_eq!(
            observer.reports()[0].outcome,
            ResponseEgressOutcome::TransportError
        );
    }
}
