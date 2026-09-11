use std::time::Duration;

use edgezero_core::body::{Body, BodyStream};
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderMap, Response, StatusCode};
use edgezero_core::response_egress::{
    ResponseEgressAttempt, ResponseEgressEnvelope, ResponseEgressOutcome,
};
use edgezero_core::time::{Deadline, MonotonicInstant};
use futures_util::StreamExt as _;
use futures_util::future::{Either, select};
use futures_util::stream::{LocalBoxStream, unfold};
use worker::{Delay, Error as WorkerError, Response as CfResponse};

use crate::outbound::copy_header_values;

/// Convert an `EdgeZero` `Response` into a Cloudflare Worker `Response`.
///
/// # Errors
/// Returns an [`EdgeError`] if the response body cannot be materialised
/// into a Workers response (empty body construction failure, byte body
/// conversion failure, stream adoption failure) or if any response
/// header is non-UTF-8 and the Workers header table rejects it.
#[inline]
pub fn from_core_response(response: Response) -> Result<CfResponse, EdgeError> {
    let (parts, body) = response.into_parts();

    let body_response = match body {
        Body::Once(bytes) if bytes.is_empty() => {
            CfResponse::empty().map_err(EdgeError::internal)?
        }
        Body::Once(bytes) => CfResponse::from_bytes(bytes.to_vec()).map_err(EdgeError::internal)?,
        Body::Stream(stream) => {
            let worker_stream = stream
                .map(|res| match res {
                    Ok(bytes) => Ok::<Vec<u8>, WorkerError>(bytes.to_vec()),
                    Err(err) => Err(WorkerError::RustError(err.to_string())),
                })
                .boxed_local();
            CfResponse::from_stream(worker_stream).map_err(EdgeError::internal)?
        }
    };

    apply_response_head(body_response, parts.status, &parts.headers)
}

pub(crate) fn from_egress_response(
    egress: ResponseEgressEnvelope,
) -> Result<CfResponse, EdgeError> {
    let started_at = MonotonicInstant::now();
    let (core_response, policy, mut attempt) =
        egress.begin(started_at).map_err(|_policy_error| {
            EdgeError::internal(anyhow::anyhow!("response-egress policy failed"))
        })?;
    if policy.write_deadline.is_expired() {
        attempt.terminate(
            ResponseEgressOutcome::DeadlineExceeded,
            MonotonicInstant::now(),
        );
        return deadline_response();
    }

    let (parts, body) = core_response.into_parts();
    match body {
        Body::Once(bytes) => {
            let body_response_result = if bytes.is_empty() {
                CfResponse::empty().map_err(EdgeError::internal)
            } else {
                CfResponse::from_bytes(bytes.to_vec()).map_err(EdgeError::internal)
            };
            let body_response = match body_response_result {
                Ok(converted_response) => converted_response,
                Err(error) => {
                    attempt.terminate(
                        ResponseEgressOutcome::ConversionError,
                        MonotonicInstant::now(),
                    );
                    return Err(error);
                }
            };
            let converted_response =
                match apply_response_head(body_response, parts.status, &parts.headers) {
                    Ok(converted_response) => converted_response,
                    Err(error) => {
                        attempt.terminate(
                            ResponseEgressOutcome::ConversionError,
                            MonotonicInstant::now(),
                        );
                        return Err(error);
                    }
                };
            if policy.write_deadline.is_expired() {
                attempt.terminate(
                    ResponseEgressOutcome::DeadlineExceeded,
                    MonotonicInstant::now(),
                );
                return deadline_response();
            }
            attempt.terminate(
                ResponseEgressOutcome::ResponseReturned,
                MonotonicInstant::now(),
            );
            Ok(converted_response)
        }
        Body::Stream(stream) => {
            let worker_stream = response_egress_stream(stream, policy.write_deadline, attempt);
            let converted_response =
                CfResponse::from_stream(worker_stream).map_err(EdgeError::internal)?;
            apply_response_head(converted_response, parts.status, &parts.headers)
        }
    }
}

fn apply_response_head(
    body_response: CfResponse,
    status: StatusCode,
    source_headers: &HeaderMap,
) -> Result<CfResponse, EdgeError> {
    let mut response = body_response.with_status(status.as_u16());
    let headers = response.headers_mut();
    copy_header_values(source_headers, headers, |_name| {
        EdgeError::internal(anyhow::anyhow!(
            "response header cannot be represented by Workers"
        ))
    })?;
    Ok(response)
}

fn deadline_response() -> Result<CfResponse, EdgeError> {
    CfResponse::error("response write deadline exceeded", 504).map_err(EdgeError::internal)
}

#[expect(
    clippy::too_many_lines,
    reason = "the stream adapter keeps deadline, accounting, and exactly-once terminal transitions together"
)]
fn response_egress_stream(
    source_stream: BodyStream,
    deadline: Deadline,
    egress_attempt: ResponseEgressAttempt,
) -> LocalBoxStream<'static, Result<Vec<u8>, WorkerError>> {
    unfold(
        (source_stream, egress_attempt, false, false),
        move |(mut body_stream, mut attempt, writing, terminal)| async move {
            if terminal {
                return None;
            }
            if deadline.is_expired() {
                attempt.terminate(
                    ResponseEgressOutcome::DeadlineExceeded,
                    MonotonicInstant::now(),
                );
                return Some((
                    Err(WorkerError::RustError(
                        "response write deadline exceeded".to_owned(),
                    )),
                    (body_stream, attempt, writing, true),
                ));
            }

            #[cfg(target_arch = "wasm32")]
            Delay::from(Duration::ZERO).await;

            #[cfg(target_arch = "wasm32")]
            let item = {
                let Some(remaining) = deadline.remaining() else {
                    attempt.terminate(
                        ResponseEgressOutcome::DeadlineExceeded,
                        MonotonicInstant::now(),
                    );
                    return Some((
                        Err(WorkerError::RustError(
                            "response write deadline exceeded".to_owned(),
                        )),
                        (body_stream, attempt, writing, true),
                    ));
                };
                let timer = Delay::from(remaining);
                let next = body_stream.next();
                match select(timer, next).await {
                    Either::Left(((), _)) => {
                        attempt.terminate(
                            ResponseEgressOutcome::DeadlineExceeded,
                            MonotonicInstant::now(),
                        );
                        return Some((
                            Err(WorkerError::RustError(
                                "response write deadline exceeded".to_owned(),
                            )),
                            (body_stream, attempt, writing, true),
                        ));
                    }
                    Either::Right((item, _)) => item,
                }
            };
            #[cfg(not(target_arch = "wasm32"))]
            let item = body_stream.next().await;

            if deadline.is_expired() {
                attempt.terminate(
                    ResponseEgressOutcome::DeadlineExceeded,
                    MonotonicInstant::now(),
                );
                return Some((
                    Err(WorkerError::RustError(
                        "response write deadline exceeded".to_owned(),
                    )),
                    (body_stream, attempt, writing, true),
                ));
            }
            match item {
                Some(Ok(bytes)) => {
                    let has_started_writing = writing || attempt.begin_writing();
                    let Ok(len) = u64::try_from(bytes.len()) else {
                        attempt.terminate(
                            ResponseEgressOutcome::TransportError,
                            MonotonicInstant::now(),
                        );
                        return Some((
                            Err(WorkerError::RustError(
                                "response byte accounting overflow".to_owned(),
                            )),
                            (body_stream, attempt, has_started_writing, true),
                        ));
                    };
                    if !attempt.account_bytes(len, MonotonicInstant::now()) {
                        return Some((
                            Err(WorkerError::RustError(
                                "response byte accounting failed".to_owned(),
                            )),
                            (body_stream, attempt, has_started_writing, true),
                        ));
                    }
                    Some((
                        Ok(bytes.to_vec()),
                        (body_stream, attempt, has_started_writing, false),
                    ))
                }
                Some(Err(_error)) => {
                    attempt.terminate(ResponseEgressOutcome::SourceError, MonotonicInstant::now());
                    Some((
                        Err(WorkerError::RustError("response source failed".to_owned())),
                        (body_stream, attempt, writing, true),
                    ))
                }
                None => {
                    if !writing {
                        attempt.begin_writing();
                    }
                    attempt.terminate(ResponseEgressOutcome::HostHandoff, MonotonicInstant::now());
                    None
                }
            }
        },
    )
    .boxed_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use edgezero_core::body::Body;
    use edgezero_core::http::response_builder;
    use futures::executor::block_on;
    use futures_util::stream;

    #[test]
    #[ignore = "requires worker runtime — worker::Response cannot be constructed in unit tests"]
    fn propagates_status_and_headers() {
        let response = response_builder()
            .status(201)
            .header("x-test", "value")
            .body(Body::text("ok"))
            .expect("response");
        let cf = from_core_response(response).expect("cf response");
        assert_eq!(cf.status_code(), 201);
        let header = cf.headers().get("x-test").unwrap();
        assert_eq!(header.as_deref(), Some("value"));
    }

    #[test]
    fn streaming_body_converts_without_buffering() {
        let stream = stream::iter(vec![Bytes::from_static(b"foo"), Bytes::from_static(b"bar")]);
        let response = response_builder()
            .status(200)
            .body(Body::stream(stream))
            .expect("response");

        let mut cf = from_core_response(response).expect("cf response");
        let mut byte_stream = cf.stream().expect("byte stream");
        let collected = block_on(async {
            let mut out = Vec::new();
            while let Some(item) = byte_stream.next().await {
                let chunk = item.expect("chunk");
                out.extend_from_slice(&chunk);
            }
            out
        });

        assert_eq!(collected, b"foobar");
    }
}
