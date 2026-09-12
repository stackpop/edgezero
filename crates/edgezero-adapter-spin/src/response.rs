#[cfg(feature = "test-utils")]
use std::time::Duration;

use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
#[cfg(feature = "test-utils")]
use edgezero_core::http::{StatusCode, response_builder};
use edgezero_core::outbound::{collect_response_stream, collect_response_stream_until_with_clock};
use edgezero_core::response_egress::{ResponseEgressEnvelope, ResponseEgressOutcome};
#[cfg(feature = "test-utils")]
use edgezero_core::time::MonotonicInstant;
use edgezero_core::time::{Deadline, MonotonicClock};
use spin_sdk::http::{FullBody, Response as SpinResponse};

use crate::SpinFullResponse;

/// Maximum body size (16 MiB) when collecting a streamed body into a buffer.
/// Prevents unbounded memory growth from malicious or misconfigured upstreams.
///
/// `Body::Once` is already materialized and bypasses this converter boundary.
pub const SPIN_RESPONSE_STREAM_BUFFER_BYTES: u64 = 0x0100_0000;

/// Collect a `Body` into a `Vec<u8>`, consuming streamed chunks if necessary.
///
/// Stream bodies are capped at [`MAX_BODY_SIZE`] bytes. If the accumulated
/// size exceeds the limit, collection stops and an error is returned.
#[cfg(test)]
pub(crate) async fn collect_body_bytes(body: Body) -> Result<Vec<u8>, EdgeError> {
    collect_body_bytes_with_deadline(body, None, &MonotonicClock::default()).await
}

async fn collect_body_bytes_with_deadline(
    body: Body,
    deadline: Option<Deadline>,
    clock: &MonotonicClock,
) -> Result<Vec<u8>, EdgeError> {
    ensure_write_deadline(deadline, clock)?;
    match body {
        Body::Once(bytes) => {
            ensure_write_deadline(deadline, clock)?;
            Ok(bytes.to_vec())
        }
        Body::Stream(stream) => {
            let collected = match deadline {
                Some(write_deadline) => {
                    collect_response_stream_until_with_clock(
                        stream,
                        SPIN_RESPONSE_STREAM_BUFFER_BYTES,
                        write_deadline,
                        clock,
                    )
                    .await?
                }
                None => collect_response_stream(stream, SPIN_RESPONSE_STREAM_BUFFER_BYTES).await?,
            };
            Ok(collected.to_vec())
        }
    }
}

/// Convert an `EdgeZero` core `Response` into a Spin SDK `Response`.
///
/// Both `Body::Once` and `Body::Stream` are converted to a buffered
/// byte body. Streaming bodies are collected into a single `Vec<u8>`.
///
/// # Errors
/// Returns [`EdgeError::internal`] if the response body cannot be collected
/// (stream error or size cap exceeded) or if the resulting Spin response
/// cannot be built from the collected bytes.
#[inline]
pub async fn from_core_response(response: Response) -> Result<SpinFullResponse, EdgeError> {
    from_core_response_with_deadline(response, None, &MonotonicClock::default()).await
}

pub(crate) async fn from_egress_response(
    egress: ResponseEgressEnvelope,
) -> Result<SpinFullResponse, EdgeError> {
    let (core_response, policy, mut attempt, clock) = egress.begin().map_err(|_policy_error| {
        EdgeError::internal(anyhow::anyhow!("response-egress policy failed"))
    })?;
    if policy.write_deadline.is_expired_at(clock.now()) {
        attempt.terminate(ResponseEgressOutcome::DeadlineExceeded, clock.now());
        return deadline_response();
    }

    let converted =
        from_core_response_with_deadline(core_response, Some(policy.write_deadline), &clock).await;
    let observed_at = clock.now();
    if policy.write_deadline.is_expired_at(observed_at) {
        attempt.terminate(ResponseEgressOutcome::DeadlineExceeded, observed_at);
        return deadline_response();
    }
    match converted {
        Ok(converted_response) => {
            attempt.terminate(ResponseEgressOutcome::ResponseReturned, observed_at);
            Ok(converted_response)
        }
        Err(error) => {
            let outcome = if matches!(error, EdgeError::ResponseTooLarge { .. }) {
                ResponseEgressOutcome::ConversionError
            } else if matches!(error, EdgeError::GatewayTimeout { .. }) {
                ResponseEgressOutcome::DeadlineExceeded
            } else {
                ResponseEgressOutcome::SourceError
            };
            attempt.terminate(outcome, observed_at);
            if outcome == ResponseEgressOutcome::DeadlineExceeded {
                deadline_response()
            } else {
                Err(error)
            }
        }
    }
}

async fn from_core_response_with_deadline(
    response: Response,
    deadline: Option<Deadline>,
    clock: &MonotonicClock,
) -> Result<SpinFullResponse, EdgeError> {
    ensure_write_deadline(deadline, clock)?;
    let (parts, body) = response.into_parts();

    let mut builder = SpinResponse::builder().status(parts.status);

    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }

    let collected = collect_body_bytes_with_deadline(body, deadline, clock).await?;
    ensure_write_deadline(deadline, clock)?;

    builder
        .body(FullBody::new(Bytes::from(collected)))
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("failed to build response: {err}")))
}

fn deadline_response() -> Result<SpinFullResponse, EdgeError> {
    SpinResponse::builder()
        .status(504)
        .body(FullBody::new(Bytes::from_static(
            b"response write deadline exceeded",
        )))
        .map_err(|error| {
            EdgeError::internal(anyhow::anyhow!(
                "failed to build deadline response: {error}"
            ))
        })
}

fn ensure_write_deadline(
    deadline: Option<Deadline>,
    clock: &MonotonicClock,
) -> Result<(), EdgeError> {
    if deadline.is_some_and(|write_deadline| write_deadline.is_expired_at(clock.now())) {
        return Err(EdgeError::gateway_timeout(
            "response write deadline exceeded",
        ));
    }
    Ok(())
}

#[cfg(feature = "test-utils")]
#[doc(hidden)]
#[inline]
pub async fn response_write_deadline_uses_injected_clock_for_test() -> bool {
    let start = MonotonicInstant::now();
    let Some(deadline) = start.checked_add(Duration::from_secs(1)) else {
        return false;
    };
    let clock = MonotonicClock::new(move || deadline);
    let Ok(response) = response_builder()
        .status(StatusCode::OK)
        .body(Body::empty())
    else {
        return false;
    };

    matches!(
        from_core_response_with_deadline(response, Some(Deadline::at_instant(deadline)), &clock,)
            .await,
        Err(EdgeError::GatewayTimeout { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::error::ResponseLimitReason;
    use futures::executor::block_on;
    use futures_util::stream;

    #[test]
    fn stream_body_conversion_enforces_fixed_cap() {
        let cap = usize::try_from(SPIN_RESPONSE_STREAM_BUFFER_BYTES).expect("cap fits usize");
        let body = Body::from_stream(stream::iter([Ok(Bytes::from(vec![0; cap + 1]))]));

        let error = block_on(collect_body_bytes(body)).expect_err("one byte over cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BufferedBody,
                ..
            }
        ));
    }
}
