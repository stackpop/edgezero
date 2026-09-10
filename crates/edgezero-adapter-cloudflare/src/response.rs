use edgezero_core::body::{Body, BodyStream};
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderMap, Response, StatusCode};
use edgezero_core::response_egress::{
    ResponseEgressAttempt, ResponseEgressEnvelope, ResponseEgressOutcome,
};
use edgezero_core::time::{Deadline, MonotonicInstant};
use futures_util::StreamExt as _;
use worker::{Error as WorkerError, Response as CfResponse};

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
    let (response, policy, attempt) = egress
        .begin(started_at)
        .map_err(|_| EdgeError::internal(anyhow::anyhow!("response-egress policy failed")))?;
    if policy.write_deadline.is_expired() {
        let mut attempt = attempt;
        attempt.terminate(
            ResponseEgressOutcome::DeadlineExceeded,
            MonotonicInstant::now(),
        );
        return deadline_response();
    }

    let (parts, body) = response.into_parts();
    match body {
        Body::Once(bytes) => {
            let body_response = if bytes.is_empty() {
                CfResponse::empty().map_err(EdgeError::internal)
            } else {
                CfResponse::from_bytes(bytes.to_vec()).map_err(EdgeError::internal)
            };
            let mut attempt = attempt;
            let body_response = match body_response {
                Ok(response) => response,
                Err(error) => {
                    attempt.terminate(
                        ResponseEgressOutcome::ConversionError,
                        MonotonicInstant::now(),
                    );
                    return Err(error);
                }
            };
            let response = match apply_response_head(body_response, parts.status, &parts.headers) {
                Ok(response) => response,
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
            Ok(response)
        }
        Body::Stream(stream) => {
            let worker_stream = response_egress_stream(stream, policy.write_deadline, attempt);
            let response = CfResponse::from_stream(worker_stream).map_err(EdgeError::internal)?;
            apply_response_head(response, parts.status, &parts.headers)
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
    for (name, value) in source_headers {
        let value = value.to_str().map_err(|_| {
            EdgeError::internal(anyhow::anyhow!(
                "response header cannot be represented by Workers"
            ))
        })?;
        headers
            .set(name.as_str(), value)
            .map_err(EdgeError::internal)?;
    }
    Ok(response)
}

fn deadline_response() -> Result<CfResponse, EdgeError> {
    CfResponse::error("response write deadline exceeded", 504).map_err(EdgeError::internal)
}

fn response_egress_stream(
    source: BodyStream,
    deadline: Deadline,
    attempt: ResponseEgressAttempt,
) -> futures_util::stream::LocalBoxStream<'static, Result<Vec<u8>, WorkerError>> {
    futures_util::stream::unfold(
        (source, attempt, false, false),
        move |(mut source, mut attempt, writing, terminal)| async move {
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
                    (source, attempt, writing, true),
                ));
            }

            #[cfg(target_arch = "wasm32")]
            worker::Delay::from(std::time::Duration::ZERO).await;

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
                        (source, attempt, writing, true),
                    ));
                };
                let timer = worker::Delay::from(remaining);
                let next = source.next();
                match futures_util::future::select(timer, next).await {
                    futures_util::future::Either::Left(((), _)) => {
                        attempt.terminate(
                            ResponseEgressOutcome::DeadlineExceeded,
                            MonotonicInstant::now(),
                        );
                        return Some((
                            Err(WorkerError::RustError(
                                "response write deadline exceeded".to_owned(),
                            )),
                            (source, attempt, writing, true),
                        ));
                    }
                    futures_util::future::Either::Right((item, _)) => item,
                }
            };
            #[cfg(not(target_arch = "wasm32"))]
            let item = source.next().await;

            if deadline.is_expired() {
                attempt.terminate(
                    ResponseEgressOutcome::DeadlineExceeded,
                    MonotonicInstant::now(),
                );
                return Some((
                    Err(WorkerError::RustError(
                        "response write deadline exceeded".to_owned(),
                    )),
                    (source, attempt, writing, true),
                ));
            }
            match item {
                Some(Ok(bytes)) => {
                    let writing = writing || attempt.begin_writing();
                    let Ok(len) = u64::try_from(bytes.len()) else {
                        attempt.terminate(
                            ResponseEgressOutcome::TransportError,
                            MonotonicInstant::now(),
                        );
                        return Some((
                            Err(WorkerError::RustError(
                                "response byte accounting overflow".to_owned(),
                            )),
                            (source, attempt, writing, true),
                        ));
                    };
                    if !attempt.account_bytes(len, MonotonicInstant::now()) {
                        return Some((
                            Err(WorkerError::RustError(
                                "response byte accounting failed".to_owned(),
                            )),
                            (source, attempt, writing, true),
                        ));
                    }
                    Some((Ok(bytes.to_vec()), (source, attempt, writing, false)))
                }
                Some(Err(_error)) => {
                    attempt.terminate(ResponseEgressOutcome::SourceError, MonotonicInstant::now());
                    Some((
                        Err(WorkerError::RustError("response source failed".to_owned())),
                        (source, attempt, writing, true),
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
