#![cfg_attr(
    target_arch = "wasm32",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "the platform module follows response construction, cancellation, and entrypoint lifecycle order"
    )
)]

use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(test)]
use std::time::Duration;

use bytes::Bytes;
use edgezero_core::body::{Body, BodyStream};
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderMap, Method, Response, StatusCode};
use edgezero_core::response_egress::{
    RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET, ResponseEgressAttempt,
    ResponseEgressFallbackDisposition, ResponseEgressOutcome, ResponseEgressPolicy,
};
use edgezero_core::response_egress_framing::{
    PreparedResponseEgress, prepare_response_egress, response_egress_source_error_category,
};
use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
use futures_util::StreamExt as _;
use futures_util::future::{FutureExt as _, LocalBoxFuture};
use futures_util::stream;

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
enum EgressKind {
    Application,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IoFailureKind {
    Deadline,
    Transport,
}

impl IoFailureKind {
    const fn delivery_error(self) -> DeliveryError {
        match self {
            Self::Deadline => DeliveryError::Deadline,
            Self::Transport => DeliveryError::Transport,
        }
    }
}

#[derive(Debug)]
struct IoFailure {
    accepted: usize,
    kind: IoFailureKind,
}

#[derive(Debug)]
struct WriteProgress {
    accepted: usize,
    remaining: Vec<u8>,
}

enum StartOutcome<Response> {
    Precommit {
        attempt: Box<ResponseEgressAttempt>,
        cause: ResponseEgressOutcome,
    },
    Started(Response),
}

trait SpinEgressCommitter {
    type Io: SpinEgressIo + 'static;
    type Response;

    fn commit(
        &mut self,
        status: StatusCode,
        headers: &HeaderMap,
        transmits_body: bool,
    ) -> Result<(Self::Response, Self::Io), DeliveryError>;

    fn spawn(&mut self, task: LocalBoxFuture<'static, ()>);
}

#[async_trait::async_trait(?Send)]
trait SpinEgressIo {
    fn abort(&mut self);

    async fn finish(
        &mut self,
        deadline: Deadline,
        clock: &MonotonicClock,
    ) -> Result<(), IoFailureKind>;

    async fn next_body(
        &mut self,
        source: &mut BodyStream,
        deadline: Deadline,
        clock: &MonotonicClock,
    ) -> Result<Option<Result<Bytes, EdgeError>>, IoFailureKind>;

    async fn write(
        &mut self,
        bytes: Vec<u8>,
        deadline: Deadline,
        clock: &MonotonicClock,
    ) -> Result<WriteProgress, IoFailure>;
}

async fn transmit_committed<Io>(
    body: Body,
    policy: ResponseEgressPolicy,
    mut attempt: ResponseEgressAttempt,
    clock: MonotonicClock,
    kind: EgressKind,
    mut io: Io,
) -> Result<(), DeliveryError>
where
    Io: SpinEgressIo,
{
    let result = pump_body(body, policy.write_deadline, &mut attempt, &clock, &mut io).await;
    match result {
        Ok(()) => {
            settle_success(kind, &mut attempt, clock.now());
            Ok(())
        }
        Err(error) => {
            abort(&mut io);
            let observed_at = terminal_observed_at(error, policy.write_deadline, &clock);
            settle_failure(kind, &mut attempt, error.outcome(), observed_at);
            Err(error)
        }
    }
}

async fn pump_body<Io>(
    body: Body,
    deadline: Deadline,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
    io: &mut Io,
) -> Result<(), DeliveryError>
where
    Io: SpinEgressIo,
{
    ensure_deadline(deadline, clock)?;
    let mut source = body_source(body);
    loop {
        let next = io
            .next_body(&mut source, deadline, clock)
            .await
            .map_err(IoFailureKind::delivery_error)?;
        ensure_deadline(deadline, clock)?;
        let Some(item) = next else {
            break;
        };
        let chunk = item.map_err(|error| {
            log::warn!(
                "response-egress Spin body source failed after commit: {}",
                response_egress_source_error_category(&error)
            );
            DeliveryError::Source
        })?;
        write_chunk(chunk.to_vec(), deadline, attempt, clock, io).await?;
    }
    io.finish(deadline, clock)
        .await
        .map_err(IoFailureKind::delivery_error)?;
    ensure_deadline(deadline, clock)
}

async fn write_chunk<Io>(
    mut remaining: Vec<u8>,
    deadline: Deadline,
    attempt: &mut ResponseEgressAttempt,
    clock: &MonotonicClock,
    io: &mut Io,
) -> Result<(), DeliveryError>
where
    Io: SpinEgressIo,
{
    while !remaining.is_empty() {
        ensure_deadline(deadline, clock)?;
        let requested = remaining.len();
        match io.write(remaining, deadline, clock).await {
            Ok(progress) => {
                account_prefix(progress.accepted, requested, attempt, clock.now())?;
                if progress.remaining.len() != requested.saturating_sub(progress.accepted) {
                    return Err(DeliveryError::Transport);
                }
                if progress.accepted == 0 {
                    return Err(DeliveryError::Transport);
                }
                remaining = progress.remaining;
                ensure_deadline(deadline, clock)?;
            }
            Err(failure) => {
                account_prefix(failure.accepted, requested, attempt, clock.now())?;
                return Err(failure.kind.delivery_error());
            }
        }
    }
    Ok(())
}

fn account_prefix(
    accepted: usize,
    requested: usize,
    attempt: &mut ResponseEgressAttempt,
    observed_at: MonotonicInstant,
) -> Result<(), DeliveryError> {
    if accepted > requested {
        return Err(DeliveryError::Transport);
    }
    let accepted_u64 = u64::try_from(accepted).map_err(|_error| DeliveryError::Transport)?;
    if !attempt.account_bytes(accepted_u64, observed_at) {
        return Err(DeliveryError::Transport);
    }
    Ok(())
}

fn abort<Io>(io: &mut Io)
where
    Io: SpinEgressIo,
{
    if catch_unwind(AssertUnwindSafe(|| io.abort())).is_err() {
        log::error!("response-egress Spin abort panicked");
    }
}

fn body_source(body: Body) -> BodyStream {
    match body {
        Body::Once(bytes) => stream::once(async move { Ok(bytes) }).boxed_local(),
        Body::Stream(source) => source,
    }
}

fn ensure_deadline(deadline: Deadline, clock: &MonotonicClock) -> Result<(), DeliveryError> {
    if deadline.is_expired_at(clock.now()) {
        Err(DeliveryError::Deadline)
    } else {
        Ok(())
    }
}

fn terminal_observed_at(
    error: DeliveryError,
    deadline: Deadline,
    clock: &MonotonicClock,
) -> MonotonicInstant {
    let observed_at = clock.now();
    if error == DeliveryError::Deadline {
        observed_at.max(deadline.instant())
    } else {
        observed_at
    }
}

fn settle_failure(
    kind: EgressKind,
    attempt: &mut ResponseEgressAttempt,
    outcome: ResponseEgressOutcome,
    observed_at: MonotonicInstant,
) {
    match kind {
        EgressKind::Application => {
            attempt.terminate(outcome, observed_at);
        }
        EgressKind::Fallback => {
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, observed_at);
        }
    }
}

fn settle_success(
    kind: EgressKind,
    attempt: &mut ResponseEgressAttempt,
    observed_at: MonotonicInstant,
) {
    match kind {
        EgressKind::Application => {
            attempt.terminate(ResponseEgressOutcome::HostHandoff, observed_at);
        }
        EgressKind::Fallback => {
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Completed, observed_at);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod platform {
    use futures_util::StreamExt as _;

    use super::{
        BodyStream, Bytes, Deadline, DeliveryError, EdgeError, EgressKind, HeaderMap, IoFailure,
        IoFailureKind, LocalBoxFuture, MonotonicClock, SpinEgressCommitter, SpinEgressIo,
        StartOutcome, StatusCode, WriteProgress, ensure_deadline, start_fallback, start_prepared,
    };
    use std::future::{Future, IntoFuture as _, pending};
    use std::pin::Pin;
    use std::task::Poll;

    use edgezero_core::response_egress::ResponseEgressEnvelope;
    use futures_util::future::poll_fn;
    use spin_sdk::time::sleep;
    use spin_sdk::wasip3::http::types::{ErrorCode, Fields};
    use spin_sdk::wasip3::http_compat::{BodyResult, BodyWriter};
    use spin_sdk::wasip3::spawn;
    use spin_sdk::wit_bindgen::rt::async_support::{
        FutureRead, FutureWrite, FutureWriteCancel, FutureWriter, StreamResult, StreamWrite,
        StreamWriter,
    };

    use crate::SpinResponse;

    const fn public_message(error: DeliveryError) -> &'static str {
        match error {
            DeliveryError::Conversion => "response conversion failed",
            DeliveryError::Deadline => "response write deadline exceeded",
            DeliveryError::Source => "response body source failed",
            DeliveryError::Transport => "response transport failed",
        }
    }

    struct WasiCommitter;

    impl SpinEgressCommitter for WasiCommitter {
        type Io = WasiEgressIo;
        type Response = SpinResponse;

        fn commit(
            &mut self,
            status: StatusCode,
            headers: &HeaderMap,
            transmits_body: bool,
        ) -> Result<(Self::Response, Self::Io), DeliveryError> {
            let entries = headers
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
                .collect::<Vec<_>>();
            let fields = Fields::from_list(&entries).map_err(|_error| DeliveryError::Conversion)?;
            let (body_writer, contents, body_result) = BodyWriter::new();
            let BodyWriter {
                stream_writer,
                result_writer,
                ..
            } = body_writer;
            let response_contents = transmits_body.then_some(contents);
            let (response, response_result) =
                SpinResponse::new(fields, response_contents, body_result);
            response
                .set_status_code(status.as_u16())
                .map_err(|()| DeliveryError::Conversion)?;
            Ok((
                response,
                WasiEgressIo {
                    response_result: Box::pin(response_result.into_future()),
                    result_writer: Some(result_writer),
                    stream_writer: Some(stream_writer),
                },
            ))
        }

        fn spawn(&mut self, task: LocalBoxFuture<'static, ()>) {
            spawn(task);
        }
    }

    struct WasiEgressIo {
        response_result: Pin<Box<FutureRead<Result<(), ErrorCode>>>>,
        result_writer: Option<FutureWriter<BodyResult>>,
        stream_writer: Option<StreamWriter<u8>>,
    }

    enum RaceEvent<Output> {
        Deadline,
        Operation(Output),
        Response(Result<(), ErrorCode>),
    }

    async fn race_operation<Operation>(
        deadline: Deadline,
        clock: &MonotonicClock,
        mut response_result: Pin<&mut FutureRead<Result<(), ErrorCode>>>,
        mut operation: Pin<&mut Operation>,
    ) -> RaceEvent<Operation::Output>
    where
        Operation: Future,
    {
        let Some(remaining) = deadline.remaining_at(clock.now()) else {
            return RaceEvent::Deadline;
        };
        let timer = sleep(remaining);
        futures_util::pin_mut!(timer);
        poll_fn(|context| {
            if timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(RaceEvent::Deadline);
            }
            if let Poll::Ready(result) = response_result.as_mut().poll(context) {
                return Poll::Ready(RaceEvent::Response(result));
            }
            operation.as_mut().poll(context).map(RaceEvent::Operation)
        })
        .await
    }

    impl WasiEgressIo {
        fn cancel_response_result(&mut self) {
            drop(self.response_result.as_mut().cancel());
        }

        fn cancelled_write(
            write: Pin<&mut StreamWrite<'_, u8>>,
            requested: usize,
            kind: IoFailureKind,
        ) -> IoFailure {
            let (_status, buffer) = write.cancel();
            IoFailure {
                accepted: requested.saturating_sub(buffer.remaining()),
                kind,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl SpinEgressIo for WasiEgressIo {
        fn abort(&mut self) {
            drop(self.stream_writer.take());
            drop(self.result_writer.take());
        }

        async fn finish(
            &mut self,
            deadline: Deadline,
            clock: &MonotonicClock,
        ) -> Result<(), IoFailureKind> {
            ensure_deadline(deadline, clock).map_err(|_error| IoFailureKind::Deadline)?;
            drop(self.stream_writer.take());
            let result_writer = self.result_writer.take().ok_or(IoFailureKind::Transport)?;
            let result_write = result_writer.write(Ok(None));
            futures_util::pin_mut!(result_write);
            let event = race_operation(
                deadline,
                clock,
                self.response_result.as_mut(),
                result_write.as_mut(),
            )
            .await;
            match event {
                RaceEvent::Deadline => {
                    drop(cancel_result_write(result_write.as_mut()));
                    self.cancel_response_result();
                    return Err(IoFailureKind::Deadline);
                }
                RaceEvent::Response(_result) => {
                    drop(cancel_result_write(result_write.as_mut()));
                    return Err(IoFailureKind::Transport);
                }
                RaceEvent::Operation(Ok(())) => {}
                RaceEvent::Operation(Err(_error)) => return Err(IoFailureKind::Transport),
            }
            if deadline.is_expired_at(clock.now()) {
                self.cancel_response_result();
                return Err(IoFailureKind::Deadline);
            }

            let pending = pending::<()>();
            futures_util::pin_mut!(pending);
            match race_operation(
                deadline,
                clock,
                self.response_result.as_mut(),
                pending.as_mut(),
            )
            .await
            {
                RaceEvent::Deadline => {
                    self.cancel_response_result();
                    Err(IoFailureKind::Deadline)
                }
                RaceEvent::Response(Ok(())) if !deadline.is_expired_at(clock.now()) => Ok(()),
                RaceEvent::Response(Ok(()) | Err(_)) => {
                    if deadline.is_expired_at(clock.now()) {
                        Err(IoFailureKind::Deadline)
                    } else {
                        Err(IoFailureKind::Transport)
                    }
                }
                RaceEvent::Operation(()) => Err(IoFailureKind::Transport),
            }
        }

        async fn next_body(
            &mut self,
            source: &mut BodyStream,
            deadline: Deadline,
            clock: &MonotonicClock,
        ) -> Result<Option<Result<Bytes, EdgeError>>, IoFailureKind> {
            ensure_deadline(deadline, clock).map_err(|_error| IoFailureKind::Deadline)?;
            let next = source.next();
            futures_util::pin_mut!(next);
            match race_operation(
                deadline,
                clock,
                self.response_result.as_mut(),
                next.as_mut(),
            )
            .await
            {
                RaceEvent::Deadline => {
                    self.cancel_response_result();
                    Err(IoFailureKind::Deadline)
                }
                RaceEvent::Response(_result) => Err(IoFailureKind::Transport),
                RaceEvent::Operation(item) if deadline.is_expired_at(clock.now()) => {
                    drop(item);
                    self.cancel_response_result();
                    Err(IoFailureKind::Deadline)
                }
                RaceEvent::Operation(item) => Ok(item),
            }
        }

        async fn write(
            &mut self,
            bytes: Vec<u8>,
            deadline: Deadline,
            clock: &MonotonicClock,
        ) -> Result<WriteProgress, IoFailure> {
            if ensure_deadline(deadline, clock).is_err() {
                return Err(IoFailure {
                    accepted: 0,
                    kind: IoFailureKind::Deadline,
                });
            }
            let requested = bytes.len();
            let (result, cancel_response) = {
                let stream_writer = self.stream_writer.as_mut().ok_or(IoFailure {
                    accepted: 0,
                    kind: IoFailureKind::Transport,
                })?;
                let write = stream_writer.write(bytes);
                futures_util::pin_mut!(write);
                let event = race_operation(
                    deadline,
                    clock,
                    self.response_result.as_mut(),
                    write.as_mut(),
                )
                .await;
                match event {
                    RaceEvent::Deadline => (
                        Err(Self::cancelled_write(
                            write.as_mut(),
                            requested,
                            IoFailureKind::Deadline,
                        )),
                        true,
                    ),
                    RaceEvent::Response(_result) => (
                        Err(Self::cancelled_write(
                            write.as_mut(),
                            requested,
                            IoFailureKind::Transport,
                        )),
                        false,
                    ),
                    RaceEvent::Operation((status, buffer)) => {
                        let accepted = requested.saturating_sub(buffer.remaining());
                        if deadline.is_expired_at(clock.now()) {
                            (
                                Err(IoFailure {
                                    accepted,
                                    kind: IoFailureKind::Deadline,
                                }),
                                true,
                            )
                        } else {
                            let result = match status {
                                StreamResult::Complete(_) => Ok(WriteProgress {
                                    accepted,
                                    remaining: buffer.into_vec(),
                                }),
                                StreamResult::Cancelled | StreamResult::Dropped => Err(IoFailure {
                                    accepted,
                                    kind: IoFailureKind::Transport,
                                }),
                            };
                            (result, false)
                        }
                    }
                }
            };
            if cancel_response {
                self.cancel_response_result();
            }
            result
        }
    }

    fn cancel_result_write(
        write: Pin<&mut FutureWrite<BodyResult>>,
    ) -> Option<FutureWriter<BodyResult>> {
        match write.cancel() {
            FutureWriteCancel::AlreadySent | FutureWriteCancel::Dropped(_) => None,
            FutureWriteCancel::Cancelled(_value, writer) => Some(writer),
        }
    }

    /// Converts an owned response-egress envelope into a raw `WASIp3` response and spawns the sole
    /// body/transmission coordinator.
    ///
    /// # Errors
    /// Returns a category-only error if both the application response and bounded precommit fallback
    /// fail before the raw response is returned to the Spin export.
    pub(crate) fn from_egress_response(
        egress: ResponseEgressEnvelope,
    ) -> Result<SpinResponse, EdgeError> {
        let request_method = egress.request_method().clone();
        let mut committer = WasiCommitter;
        let result = match egress.begin() {
            Ok((prepared, policy, attempt, clock)) => {
                match start_prepared(
                    prepared,
                    policy,
                    attempt,
                    clock.clone(),
                    EgressKind::Application,
                    &mut committer,
                ) {
                    StartOutcome::Started(response) => Ok(response),
                    StartOutcome::Precommit {
                        attempt: failed_attempt,
                        cause,
                    } => start_fallback(
                        &request_method,
                        cause,
                        *failed_attempt,
                        &clock,
                        &mut committer,
                    ),
                }
            }
            Err(failure) => {
                let (attempt, clock, cause) = failure.into_parts();
                start_fallback(&request_method, cause, attempt, &clock, &mut committer)
            }
        };
        result.map_err(|error| EdgeError::internal(anyhow::anyhow!(public_message(error))))
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use platform::from_egress_response;

fn start_prepared<Committer>(
    prepared: PreparedResponseEgress,
    policy: ResponseEgressPolicy,
    mut attempt: ResponseEgressAttempt,
    clock: MonotonicClock,
    kind: EgressKind,
    committer: &mut Committer,
) -> StartOutcome<Committer::Response>
where
    Committer: SpinEgressCommitter,
{
    if policy.write_deadline.is_expired_at(clock.now()) {
        return StartOutcome::Precommit {
            attempt: Box::new(attempt),
            cause: ResponseEgressOutcome::DeadlineExceeded,
        };
    }
    let transmits_body = prepared.transmits_body();
    let response = prepared.into_response();
    let (parts, body) = response.into_parts();
    let (platform_response, mut io) =
        match committer.commit(parts.status, &parts.headers, transmits_body) {
            Ok(pair) => pair,
            Err(error) => {
                return StartOutcome::Precommit {
                    attempt: Box::new(attempt),
                    cause: error.outcome(),
                };
            }
        };
    if policy.write_deadline.is_expired_at(clock.now()) {
        abort(&mut io);
        return StartOutcome::Precommit {
            attempt: Box::new(attempt),
            cause: ResponseEgressOutcome::DeadlineExceeded,
        };
    }
    if !attempt.begin_writing() {
        abort(&mut io);
        return StartOutcome::Precommit {
            attempt: Box::new(attempt),
            cause: ResponseEgressOutcome::ConversionError,
        };
    }
    let task = AssertUnwindSafe(transmit_committed(body, policy, attempt, clock, kind, io))
        .catch_unwind()
        .map(|result| {
            if result.is_err() {
                log::error!("response-egress Spin coordinator panicked");
            }
        })
        .boxed_local();
    committer.spawn(task);
    StartOutcome::Started(platform_response)
}

fn start_fallback<Committer>(
    request_method: &Method,
    original_cause: ResponseEgressOutcome,
    mut attempt: ResponseEgressAttempt,
    clock: &MonotonicClock,
    committer: &mut Committer,
) -> Result<Committer::Response, DeliveryError>
where
    Committer: SpinEgressCommitter,
{
    if !attempt.begin_fallback(original_cause) {
        attempt.terminate(ResponseEgressOutcome::ConversionError, clock.now());
        return Err(DeliveryError::Conversion);
    }
    let started_at = clock.now();
    let fallback_deadline = started_at
        .checked_add(RESPONSE_EGRESS_FALLBACK_SAFETY_BUDGET)
        .unwrap_or(started_at);
    let prepared = prepare_response_egress(request_method, fallback_response(original_cause))
        .map_err(|_error| {
            attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
            DeliveryError::from_outcome(original_cause)
        })?;
    match start_prepared(
        prepared,
        ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(fallback_deadline),
        },
        attempt,
        clock.clone(),
        EgressKind::Fallback,
        committer,
    ) {
        StartOutcome::Started(response) => Ok(response),
        StartOutcome::Precommit {
            attempt: mut failed_attempt,
            cause: _fallback_cause,
        } => {
            failed_attempt.finish_fallback(ResponseEgressFallbackDisposition::Aborted, clock.now());
            Err(DeliveryError::from_outcome(original_cause))
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

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::future::{Future as _, pending};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;
    use edgezero_core::http::{HeaderMap, Method, Version, response_builder};
    use edgezero_core::response_egress::{
        ResponseEgressBodyKind, ResponseEgressHead, ResponseEgressObserver,
        ResponseEgressObserverHandle, ResponseEgressReport,
    };
    use futures::executor::block_on;
    use futures_util::future::LocalBoxFuture;
    use futures_util::stream;
    use futures_util::task::noop_waker_ref;

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

    #[derive(Clone, Copy)]
    enum MockWrite {
        Accept(usize),
        Fail(IoFailureKind, usize),
    }

    #[derive(Default)]
    struct MockState {
        aborts: usize,
        accepted: Vec<u8>,
        drops: usize,
        events: Vec<&'static str>,
        finish: Option<IoFailureKind>,
        next_body: Option<IoFailureKind>,
        pending: Option<PendingAt>,
        writes: VecDeque<MockWrite>,
    }

    struct MockIo(Rc<RefCell<MockState>>);

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum PendingAt {
        Finish,
        Source,
        Write,
    }

    impl Drop for MockIo {
        fn drop(&mut self) {
            let drops = self.0.borrow().drops;
            self.0.borrow_mut().drops = drops.saturating_add(1);
        }
    }

    #[derive(Clone)]
    struct MockResponse {
        status: StatusCode,
        transmits_body: bool,
    }

    struct MockCommitter {
        io: Rc<RefCell<MockState>>,
        responses: Rc<RefCell<Vec<MockResponse>>>,
        tasks: VecDeque<LocalBoxFuture<'static, ()>>,
    }

    impl MockCommitter {
        fn new(io: Rc<RefCell<MockState>>) -> Self {
            Self {
                io,
                responses: Rc::new(RefCell::new(Vec::new())),
                tasks: VecDeque::new(),
            }
        }

        fn run_task(&mut self) {
            let task = self.tasks.pop_front().expect("spawned task");
            block_on(task);
        }
    }

    impl SpinEgressCommitter for MockCommitter {
        type Io = MockIo;
        type Response = MockResponse;

        fn commit(
            &mut self,
            status: StatusCode,
            _headers: &HeaderMap,
            transmits_body: bool,
        ) -> Result<(Self::Response, Self::Io), DeliveryError> {
            let response = MockResponse {
                status,
                transmits_body,
            };
            self.responses.borrow_mut().push(response.clone());
            Ok((response, MockIo(Rc::clone(&self.io))))
        }

        fn spawn(&mut self, task: LocalBoxFuture<'static, ()>) {
            self.tasks.push_back(task);
        }
    }

    #[async_trait::async_trait(?Send)]
    impl SpinEgressIo for MockIo {
        fn abort(&mut self) {
            let mut state = self.0.borrow_mut();
            state.aborts = state.aborts.saturating_add(1);
            state.events.push("abort");
        }

        async fn finish(
            &mut self,
            _deadline: Deadline,
            _clock: &MonotonicClock,
        ) -> Result<(), IoFailureKind> {
            let should_wait = {
                let mut state = self.0.borrow_mut();
                state.events.push("finish");
                state.pending == Some(PendingAt::Finish)
            };
            if should_wait {
                pending::<()>().await;
            }
            self.0.borrow().finish.map_or(Ok(()), Err)
        }

        async fn next_body(
            &mut self,
            source: &mut BodyStream,
            _deadline: Deadline,
            _clock: &MonotonicClock,
        ) -> Result<Option<Result<Bytes, EdgeError>>, IoFailureKind> {
            let (should_wait, failure) = {
                let mut state = self.0.borrow_mut();
                state.events.push("source");
                (state.pending == Some(PendingAt::Source), state.next_body)
            };
            if should_wait {
                pending::<()>().await;
            }
            if let Some(next_failure) = failure {
                return Err(next_failure);
            }
            Ok(source.next().await)
        }

        async fn write(
            &mut self,
            mut bytes: Vec<u8>,
            _deadline: Deadline,
            _clock: &MonotonicClock,
        ) -> Result<WriteProgress, IoFailure> {
            let (plan, should_wait) = {
                let mut state = self.0.borrow_mut();
                state.events.push("write");
                (
                    state.writes.pop_front(),
                    state.pending == Some(PendingAt::Write),
                )
            };
            if should_wait {
                pending::<()>().await;
            }
            let (accepted, failure) = match plan {
                Some(MockWrite::Accept(accepted)) => (accepted, None),
                Some(MockWrite::Fail(kind, accepted)) => (accepted, Some(kind)),
                None => (bytes.len(), None),
            };
            let remaining = if accepted <= bytes.len() {
                let remaining = bytes.split_off(accepted);
                self.0.borrow_mut().accepted.extend_from_slice(&bytes);
                remaining
            } else {
                Vec::new()
            };
            let progress = WriteProgress {
                accepted,
                remaining,
            };
            match failure {
                Some(kind) => Err(IoFailure { accepted, kind }),
                None => Ok(progress),
            }
        }
    }

    fn stable_clock(now: MonotonicInstant) -> MonotonicClock {
        MonotonicClock::new(move || now)
    }

    fn future_policy(now: MonotonicInstant) -> ResponseEgressPolicy {
        ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(
                now.checked_add(Duration::from_secs(10)).expect("deadline"),
            ),
        }
    }

    fn new_attempt(observer: &RecordingObserver, now: MonotonicInstant) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let head =
            ResponseEgressHead::new(StatusCode::OK, Version::HTTP_11, &headers, None, now, None);
        ResponseEgressAttempt::new(
            &head,
            now,
            ResponseEgressObserverHandle::new(observer.clone()),
            MonotonicClock::default(),
        )
    }

    fn attempt(observer: &RecordingObserver, now: MonotonicInstant) -> ResponseEgressAttempt {
        let mut attempt = new_attempt(observer, now);
        assert!(attempt.begin_writing());
        attempt
    }

    fn transmit(
        body: Body,
        state: Rc<RefCell<MockState>>,
    ) -> (Result<(), DeliveryError>, Vec<ResponseEgressReport>) {
        let now = MonotonicInstant::now();
        let observer = RecordingObserver::default();
        let result = block_on(transmit_committed(
            body,
            future_policy(now),
            attempt(&observer, now),
            stable_clock(now),
            EgressKind::Application,
            MockIo(state),
        ));
        (result, observer.reports())
    }

    #[test]
    fn coordinator_streams_once_and_multiple_chunks_with_partial_writes() {
        for (body, writes, expected_bytes) in [
            (Body::empty(), VecDeque::new(), 0_u64),
            (
                Body::from("hello"),
                VecDeque::from([MockWrite::Accept(2), MockWrite::Accept(3)]),
                5,
            ),
            (
                Body::stream(stream::iter([
                    Bytes::from_static(b"ab"),
                    Bytes::from_static(b"cd"),
                ])),
                VecDeque::new(),
                4,
            ),
        ] {
            let state = Rc::new(RefCell::new(MockState {
                writes,
                ..MockState::default()
            }));

            let (result, reports) = transmit(body, Rc::clone(&state));

            assert_eq!(result, Ok(()));
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::HostHandoff);
            assert_eq!(reports[0].bytes_written, expected_bytes);
            assert_eq!(state.borrow().aborts, 0);
            assert_eq!(state.borrow().events.last(), Some(&"finish"));
        }
    }

    #[test]
    fn cancellation_accounts_the_accepted_prefix_before_deadline_wins() {
        let state = Rc::new(RefCell::new(MockState {
            writes: VecDeque::from([MockWrite::Fail(IoFailureKind::Deadline, 2)]),
            ..MockState::default()
        }));

        let (result, reports) = transmit(Body::from("hello"), Rc::clone(&state));

        assert_eq!(result, Err(DeliveryError::Deadline));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].bytes_written, 2);
        assert_eq!(reports[0].elapsed, Duration::from_secs(10));
        assert_eq!(state.borrow().aborts, 1);
    }

    #[test]
    fn zero_progress_source_error_and_finish_error_each_abort_once() {
        let zero_state = Rc::new(RefCell::new(MockState {
            writes: VecDeque::from([MockWrite::Accept(0)]),
            ..MockState::default()
        }));
        let (zero_result, zero_reports) = transmit(Body::from("x"), Rc::clone(&zero_state));
        assert_eq!(zero_result, Err(DeliveryError::Transport));
        assert_eq!(
            zero_reports[0].outcome,
            ResponseEgressOutcome::TransportError
        );
        assert_eq!(zero_state.borrow().aborts, 1);

        let source_state = Rc::new(RefCell::new(MockState::default()));
        let source_body = Body::from_stream(stream::iter([Err(EdgeError::internal(
            anyhow::anyhow!("private source detail"),
        ))]));
        let (source_result, source_reports) = transmit(source_body, Rc::clone(&source_state));
        assert_eq!(source_result, Err(DeliveryError::Source));
        assert_eq!(
            source_reports[0].outcome,
            ResponseEgressOutcome::SourceError
        );
        assert_eq!(source_state.borrow().aborts, 1);

        let finish_state = Rc::new(RefCell::new(MockState {
            finish: Some(IoFailureKind::Transport),
            ..MockState::default()
        }));
        let (finish_result, finish_reports) = transmit(Body::empty(), Rc::clone(&finish_state));
        assert_eq!(finish_result, Err(DeliveryError::Transport));
        assert_eq!(
            finish_reports[0].outcome,
            ResponseEgressOutcome::TransportError
        );
        assert_eq!(finish_state.borrow().aborts, 1);
    }

    #[test]
    fn fallback_uses_its_own_budget_and_preserves_original_cause() {
        let now = MonotonicInstant::now();
        let clock = stable_clock(now);
        let observer = RecordingObserver::default();
        let state = Rc::new(RefCell::new(MockState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let fallback = start_fallback(
            &Method::GET,
            ResponseEgressOutcome::ConversionError,
            new_attempt(&observer, now),
            &clock,
            &mut committer,
        )
        .expect("fallback response");

        assert_eq!(fallback.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(fallback.transmits_body);
        committer.run_task();
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
        assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
        assert_eq!(state.borrow().aborts, 0);
        assert_eq!(state.borrow().accepted, INTERNAL_FALLBACK_BODY);
        assert_eq!(
            reports[0].bytes_written,
            u64::try_from(INTERNAL_FALLBACK_BODY.len()).expect("fallback length")
        );
    }

    #[test]
    fn fallback_failures_abort_once_and_preserve_original_cause() {
        for (writes, finish, next_body, expected_bytes) in [
            (
                VecDeque::from([
                    MockWrite::Accept(3),
                    MockWrite::Fail(IoFailureKind::Transport, 0),
                ]),
                None,
                None,
                3,
            ),
            (
                VecDeque::new(),
                Some(IoFailureKind::Transport),
                None,
                u64::try_from(INTERNAL_FALLBACK_BODY.len()).expect("fallback length"),
            ),
            (VecDeque::new(), None, Some(IoFailureKind::Deadline), 0),
        ] {
            let now = MonotonicInstant::now();
            let observer = RecordingObserver::default();
            let state = Rc::new(RefCell::new(MockState {
                finish,
                next_body,
                writes,
                ..MockState::default()
            }));
            let mut committer = MockCommitter::new(Rc::clone(&state));
            let clock = stable_clock(now);

            start_fallback(
                &Method::GET,
                ResponseEgressOutcome::ConversionError,
                new_attempt(&observer, now),
                &clock,
                &mut committer,
            )
            .expect("fallback response");
            committer.run_task();

            let reports = observer.reports();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::ConversionError);
            assert_eq!(reports[0].body_kind, ResponseEgressBodyKind::Fallback);
            assert_eq!(
                reports[0].fallback_disposition,
                Some(ResponseEgressFallbackDisposition::Aborted)
            );
            assert_eq!(reports[0].bytes_written, expected_bytes);
            assert_eq!(state.borrow().aborts, 1);
        }
    }

    #[test]
    fn head_fallback_suppresses_body_without_writing() {
        let now = MonotonicInstant::now();
        let observer = RecordingObserver::default();
        let state = Rc::new(RefCell::new(MockState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let clock = stable_clock(now);
        let fallback = start_fallback(
            &Method::HEAD,
            ResponseEgressOutcome::DeadlineExceeded,
            new_attempt(&observer, now),
            &clock,
            &mut committer,
        )
        .expect("HEAD fallback response");
        assert_eq!(fallback.status, StatusCode::GATEWAY_TIMEOUT);
        assert!(!fallback.transmits_body);
        committer.run_task();

        assert!(state.borrow().accepted.is_empty());
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].bytes_written, 0);
        assert_eq!(
            reports[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
    }

    #[test]
    fn dropping_coordinator_in_each_pending_state_reports_once_and_releases_io() {
        for pending_at in [PendingAt::Source, PendingAt::Write, PendingAt::Finish] {
            let now = MonotonicInstant::now();
            let observer = RecordingObserver::default();
            let state = Rc::new(RefCell::new(MockState {
                pending: Some(pending_at),
                ..MockState::default()
            }));
            let mut future = Box::pin(transmit_committed(
                Body::from("body"),
                future_policy(now),
                attempt(&observer, now),
                stable_clock(now),
                EgressKind::Application,
                MockIo(Rc::clone(&state)),
            ));
            let mut context = Context::from_waker(noop_waker_ref());

            assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
            drop(future);

            assert_eq!(state.borrow().drops, 1);
            let reports = observer.reports();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::TransportError);
        }
    }

    #[test]
    fn deadline_equality_aborts_before_polling_the_source() {
        let now = MonotonicInstant::now();
        let observer = RecordingObserver::default();
        let state = Rc::new(RefCell::new(MockState::default()));
        let result = block_on(transmit_committed(
            Body::from("body"),
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(now),
            },
            attempt(&observer, now),
            stable_clock(now),
            EgressKind::Application,
            MockIo(Rc::clone(&state)),
        ));

        assert_eq!(result, Err(DeliveryError::Deadline));
        assert_eq!(state.borrow().events, ["abort"]);
        assert_eq!(state.borrow().aborts, 1);
        let reports = observer.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
    }

    #[test]
    fn suppressed_application_source_is_dropped_without_polling() {
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
        let body = Body::from_stream(stream::poll_fn(move |_context| {
            let _keep_alive = &signal;
            observed_polls.set(observed_polls.get() + 1);
            Poll::Pending
        }));
        let core_response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "99")
            .body(body)
            .expect("response");
        let prepared = prepare_response_egress(&Method::HEAD, core_response).expect("prepared");
        let now = MonotonicInstant::now();
        let observer = RecordingObserver::default();
        let state = Rc::new(RefCell::new(MockState::default()));
        let mut committer = MockCommitter::new(Rc::clone(&state));

        let platform_response = match start_prepared(
            prepared,
            future_policy(now),
            new_attempt(&observer, now),
            stable_clock(now),
            EgressKind::Application,
            &mut committer,
        ) {
            StartOutcome::Started(response) => response,
            StartOutcome::Precommit { cause, .. } => {
                panic!("unexpected start failure: {cause:?}")
            }
        };
        assert!(!platform_response.transmits_body);
        committer.run_task();

        assert_eq!(polls.get(), 0);
        assert_eq!(drops.get(), 1);
        assert!(state.borrow().accepted.is_empty());
        assert_eq!(observer.reports()[0].bytes_written, 0);
    }
}
