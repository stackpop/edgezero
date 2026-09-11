use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, Uri};
use edgezero_core::outbound::{collect_response_stream, collect_response_stream_until_with_clock};
use edgezero_core::response_egress::{ResponseEgressEnvelope, ResponseEgressOutcome};
use edgezero_core::time::{Deadline, MonotonicClock};
use fastly::Response as FastlyResponse;
use futures::executor;

pub const FASTLY_RESPONSE_STREAM_BUFFER_BYTES: u64 = 0x0100_0000;

/// # Errors
/// Returns [`EdgeError::Internal`] if the response body cannot be streamed to the Fastly send-channel.
#[inline]
pub fn from_core_response(response: Response) -> Result<FastlyResponse, EdgeError> {
    from_core_response_with_deadline(response, None, &MonotonicClock::default())
}

pub(crate) fn from_egress_response(
    egress: ResponseEgressEnvelope,
) -> Result<FastlyResponse, EdgeError> {
    let (response, policy, mut attempt, clock) = egress.begin().map_err(|_outcome| {
        EdgeError::internal(anyhow::anyhow!("response-egress policy failed"))
    })?;
    if policy.write_deadline.is_expired_at(clock.now()) {
        attempt.terminate(ResponseEgressOutcome::DeadlineExceeded, clock.now());
        return Ok(deadline_response());
    }

    let converted = from_core_response_with_deadline(response, Some(policy.write_deadline), &clock);
    let observed_at = clock.now();
    if policy.write_deadline.is_expired_at(observed_at) {
        attempt.terminate(ResponseEgressOutcome::DeadlineExceeded, observed_at);
        return Ok(deadline_response());
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
                Ok(deadline_response())
            } else {
                Err(error)
            }
        }
    }
}

fn from_core_response_with_deadline(
    response: Response,
    deadline: Option<Deadline>,
    clock: &MonotonicClock,
) -> Result<FastlyResponse, EdgeError> {
    ensure_write_deadline(deadline, clock)?;
    let (parts, body) = response.into_parts();
    let mut fastly_response = FastlyResponse::from_status(parts.status.as_u16());

    match body {
        Body::Once(bytes) => fastly_response.set_body(bytes.to_vec()),
        Body::Stream(stream) => {
            let collected = match deadline {
                Some(write_deadline) => {
                    executor::block_on(collect_response_stream_until_with_clock(
                        stream,
                        FASTLY_RESPONSE_STREAM_BUFFER_BYTES,
                        write_deadline,
                        clock,
                    ))?
                }
                None => executor::block_on(collect_response_stream(
                    stream,
                    FASTLY_RESPONSE_STREAM_BUFFER_BYTES,
                ))?,
            };
            fastly_response.set_body(collected.to_vec());
        }
    }

    // `append_header` preserves multi-value headers (e.g. N `Set-Cookie`). The
    // response starts empty (`from_status`) and `http::HeaderMap` iteration
    // yields one entry per value, so appending is unconditionally correct.
    for (name, value) in &parts.headers {
        fastly_response.append_header(name.as_str(), value.as_bytes());
    }

    ensure_write_deadline(deadline, clock)?;
    Ok(fastly_response)
}

fn deadline_response() -> FastlyResponse {
    let mut response = FastlyResponse::from_status(504);
    response.set_body("response write deadline exceeded");
    response
}

fn ensure_write_deadline(
    deadline: Option<Deadline>,
    clock: &MonotonicClock,
) -> Result<(), EdgeError> {
    if deadline.is_some_and(|candidate| candidate.is_expired_at(clock.now())) {
        return Err(EdgeError::gateway_timeout(
            "response write deadline exceeded",
        ));
    }
    Ok(())
}

pub(crate) fn parse_uri(uri: &str) -> Result<Uri, EdgeError> {
    uri.parse::<Uri>()
        .map_err(|err| EdgeError::bad_request(format!("invalid request URI: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use edgezero_core::body::Body;
    use edgezero_core::error::ResponseLimitReason;
    use edgezero_core::http::response_builder;
    use edgezero_core::response::IntoResponse as _;
    use edgezero_core::time::MonotonicInstant;
    use futures_util::stream;
    use std::time::Duration;

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
    fn repeated_set_cookie_survives_response_conversion() {
        // http::response::Builder::header APPENDS, so this is two Set-Cookie values.
        let response = response_builder()
            .status(200)
            .header("set-cookie", "a=1")
            .header("set-cookie", "b=2")
            .body(Body::empty())
            .expect("response");

        let fastly_response = from_core_response(response).expect("fastly response");

        let cookies: Vec<String> = fastly_response
            .get_header_all("set-cookie")
            .map(|value| value.to_str().expect("utf8").to_owned())
            .collect();
        assert_eq!(cookies, vec!["a=1".to_owned(), "b=2".to_owned()]);
    }

    #[test]
    fn stream_body_is_written_to_fastly_response() {
        let response = response_builder()
            .status(200)
            .body(Body::stream(stream::iter(vec![
                Bytes::from_static(b"hello "),
                Bytes::from_static(b"world"),
            ])))
            .expect("response");

        let mut fastly_response = from_core_response(response).expect("fastly response");
        let body_bytes = fastly_response.take_body_bytes();
        assert_eq!(body_bytes, b"hello world");
    }

    #[test]
    fn stream_body_conversion_enforces_fixed_cap() {
        let cap = usize::try_from(FASTLY_RESPONSE_STREAM_BUFFER_BYTES).expect("cap fits usize");
        let over = response_builder()
            .status(200)
            .body(Body::from_stream(stream::iter([Ok(Bytes::from(vec![
                0;
                cap + 1
            ]))])))
            .expect("response");

        let error = from_core_response(over).expect_err("one byte over cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BufferedBody,
                ..
            }
        ));
    }

    #[test]
    fn downstream_fallback_preserves_typed_error_envelope() {
        let response = EdgeError::response_too_large_with_reason(
            "private upstream detail",
            ResponseLimitReason::BufferedBody,
        )
        .into_response()
        .expect("error response");

        let mut fastly_response = from_core_response(response).expect("Fastly response");
        let body: serde_json::Value = serde_json::from_slice(&fastly_response.take_body_bytes())
            .expect("JSON error envelope");

        assert_eq!(fastly_response.get_status().as_u16(), 502);
        assert_eq!(body["error"]["kind"], "response_too_large");
        assert_eq!(
            body["error"]["message"],
            "upstream response exceeded configured limits"
        );
        assert!(body["error"].get("reason").is_none());
    }

    #[test]
    fn injected_clock_controls_response_write_deadline() {
        let start = MonotonicInstant::now();
        let deadline = start.checked_add(Duration::from_secs(1)).expect("deadline");
        let clock = MonotonicClock::new(move || deadline);
        let response = response_builder()
            .status(200)
            .body(Body::empty())
            .expect("response");

        let error = from_core_response_with_deadline(
            response,
            Some(Deadline::at_instant(deadline)),
            &clock,
        )
        .expect_err("deadline must win");

        assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
    }
}
