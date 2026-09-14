use std::mem;

use crate::body::{Body, BodyStream};
use crate::error::EdgeError;
use crate::http::header::{
    CONNECTION, CONTENT_LENGTH, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use crate::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use futures_util::StreamExt as _;
use futures_util::stream::unfold;

/// Typed streamed-body violation used by adapter coordinators for terminal classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseEgressBodyLengthError {
    #[error("response body exceeded declared content-length")]
    Exceeded,
    #[error("response body ended before declared content-length")]
    Incomplete,
}

/// Category-only response framing failure reported before platform commitment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResponseEgressFramingError {
    #[error("response body length does not match content-length")]
    ContentLengthMismatch,
    #[error("invalid connection response header")]
    InvalidConnectionHeader,
    #[error("invalid response content-length")]
    InvalidContentLength,
    #[error("unsupported final response status")]
    UnsupportedStatus,
    #[error("response trailers are unsupported")]
    UnsupportedTrailers,
    #[error("successful connect response requires a tunnel lifecycle")]
    UnsupportedTunnel,
}

/// Canonical response head and body ready for one adapter-owned writer.
#[doc(hidden)]
pub struct PreparedResponseEgress {
    declared_length: Option<u64>,
    response: Response,
    transmits_body: bool,
}

impl PreparedResponseEgress {
    #[must_use]
    #[inline]
    pub fn declared_length(&self) -> Option<u64> {
        self.declared_length
    }

    #[must_use]
    #[inline]
    pub fn into_response(self) -> Response {
        self.response
    }

    #[must_use]
    #[inline]
    pub fn transmits_body(&self) -> bool {
        self.transmits_body
    }
}

/// Validates and normalizes response framing before an adapter commits the head.
///
/// # Errors
/// Returns a category-only framing error for invalid or conflicting field values and for a
/// buffered body whose length disagrees with an explicit `Content-Length`.
#[doc(hidden)]
#[inline]
pub fn prepare_response_egress(
    request_method: &Method,
    mut response: Response,
) -> Result<PreparedResponseEgress, ResponseEgressFramingError> {
    let mut declared_length = parse_content_length(response.headers())?;
    if response.status().is_informational() {
        return Err(ResponseEgressFramingError::UnsupportedStatus);
    }
    if *request_method == Method::CONNECT && response.status().is_success() {
        return Err(ResponseEgressFramingError::UnsupportedTunnel);
    }

    let suppress_body = if response.status() == StatusCode::NO_CONTENT {
        response.headers_mut().remove(CONTENT_LENGTH);
        declared_length = None;
        true
    } else if response.status() == StatusCode::RESET_CONTENT {
        if declared_length.is_some_and(|length| length != 0) {
            return Err(ResponseEgressFramingError::ContentLengthMismatch);
        }
        true
    } else {
        response.status() == StatusCode::NOT_MODIFIED || *request_method == Method::HEAD
    };

    if suppress_body {
        let original_body = mem::take(response.body_mut());
        drop(original_body);
    }

    if response.headers().contains_key(TRAILER) {
        return Err(ResponseEgressFramingError::UnsupportedTrailers);
    }
    let content_length_was_nominated = strip_connection_fields(response.headers_mut())?;
    response.headers_mut().remove(TRANSFER_ENCODING);
    if content_length_was_nominated {
        declared_length = None;
    }

    if let Some(length) = declared_length {
        response.headers_mut().remove(CONTENT_LENGTH);
        let canonical = HeaderValue::from_str(&length.to_string())
            .map_err(|_error| ResponseEgressFramingError::InvalidContentLength)?;
        response.headers_mut().insert(CONTENT_LENGTH, canonical);
        if !suppress_body {
            match response.body() {
                Body::Once(bytes) => {
                    let actual = u64::try_from(bytes.len())
                        .map_err(|_error| ResponseEgressFramingError::ContentLengthMismatch)?;
                    if actual != length {
                        return Err(ResponseEgressFramingError::ContentLengthMismatch);
                    }
                }
                Body::Stream(_) => {
                    let body = mem::take(response.body_mut());
                    if let Body::Stream(stream) = body {
                        *response.body_mut() =
                            Body::from_stream(limit_declared_stream(stream, length));
                    }
                }
            }
        }
    }

    Ok(PreparedResponseEgress {
        declared_length,
        response,
        transmits_body: !suppress_body,
    })
}

fn limit_declared_stream(stream: BodyStream, length: u64) -> BodyStream {
    unfold(
        (Some(stream), length),
        |(optional_source, remaining)| async move {
            let mut source = optional_source?;
            match source.next().await {
                Some(Ok(bytes)) => {
                    let Ok(chunk_length) = u64::try_from(bytes.len()) else {
                        drop(source);
                        return Some((
                            Err(EdgeError::internal(ResponseEgressBodyLengthError::Exceeded)),
                            (None, remaining),
                        ));
                    };
                    let Some(next_remaining) = remaining.checked_sub(chunk_length) else {
                        drop(source);
                        return Some((
                            Err(EdgeError::internal(ResponseEgressBodyLengthError::Exceeded)),
                            (None, remaining),
                        ));
                    };
                    Some((Ok(bytes), (Some(source), next_remaining)))
                }
                Some(Err(error)) => {
                    drop(source);
                    Some((Err(error), (None, remaining)))
                }
                None if remaining == 0 => None,
                None => {
                    drop(source);
                    Some((
                        Err(EdgeError::internal(
                            ResponseEgressBodyLengthError::Incomplete,
                        )),
                        (None, remaining),
                    ))
                }
            }
        },
    )
    .boxed_local()
}

/// Returns the typed streamed-body length violation carried by an internal body error.
#[must_use]
#[doc(hidden)]
#[inline]
pub fn response_egress_body_length_error(
    error: &EdgeError,
) -> Option<ResponseEgressBodyLengthError> {
    let EdgeError::Internal { source } = error else {
        return None;
    };
    source
        .downcast_ref::<ResponseEgressBodyLengthError>()
        .copied()
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, ResponseEgressFramingError> {
    let mut parsed = None;
    for header_value in headers.get_all(CONTENT_LENGTH) {
        let text = header_value
            .to_str()
            .map_err(|_error| ResponseEgressFramingError::InvalidContentLength)?;
        for member in text.split(',') {
            let candidate = member.trim();
            if candidate.is_empty() || !candidate.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ResponseEgressFramingError::InvalidContentLength);
            }
            let parsed_value = candidate
                .parse::<u64>()
                .map_err(|_error| ResponseEgressFramingError::InvalidContentLength)?;
            if parsed.is_some_and(|existing| existing != parsed_value) {
                return Err(ResponseEgressFramingError::InvalidContentLength);
            }
            parsed = Some(parsed_value);
        }
    }
    Ok(parsed)
}

fn strip_connection_fields(headers: &mut HeaderMap) -> Result<bool, ResponseEgressFramingError> {
    let mut nominated = Vec::new();
    for header_value in headers.get_all(CONNECTION) {
        let text = header_value
            .to_str()
            .map_err(|_error| ResponseEgressFramingError::InvalidConnectionHeader)?;
        for token in text.split(',') {
            let name = HeaderName::from_bytes(token.trim().as_bytes())
                .map_err(|_error| ResponseEgressFramingError::InvalidConnectionHeader)?;
            nominated.push(name);
        }
    }
    let content_length_was_nominated = nominated.contains(&CONTENT_LENGTH);
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        CONNECTION,
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
    Ok(content_length_was_nominated)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::task::Poll;

    use bytes::Bytes;
    use futures::executor::block_on;
    use futures_util::StreamExt as _;
    use futures_util::stream::{self, poll_fn};

    use crate::body::Body;
    use crate::error::EdgeError;
    use crate::http::{Method, Response, StatusCode, response_builder};

    use super::{
        ResponseEgressBodyLengthError, ResponseEgressFramingError, prepare_response_egress,
        response_egress_body_length_error,
    };

    struct DropSignal(Rc<Cell<usize>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.set(self.0.get().saturating_add(1));
        }
    }

    fn tracked_body(source_items: Vec<Result<Bytes, EdgeError>>, drops: &Rc<Cell<usize>>) -> Body {
        let signal = DropSignal(Rc::clone(drops));
        let mut pending_items = VecDeque::from(source_items);
        Body::from_stream(poll_fn(move |_context| {
            let _keep_signal_alive = &signal;
            Poll::Ready(pending_items.pop_front())
        }))
    }

    fn prepare_error(
        method: &Method,
        response: Response,
        failure_message: &str,
    ) -> ResponseEgressFramingError {
        let Err(error) = prepare_response_egress(method, response) else {
            panic!("{failure_message}");
        };
        error
    }

    #[test]
    fn normalizes_duplicate_content_length_and_strips_connection_fields() {
        let source_response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "5, 5")
            .header("content-length", "5")
            .header("connection", "x-private, keep-alive")
            .header("x-private", "secret")
            .header("keep-alive", "timeout=5")
            .body(Body::from("hello"))
            .expect("response");

        let prepared =
            prepare_response_egress(&Method::GET, source_response).expect("prepared response");
        assert!(prepared.transmits_body());
        assert_eq!(prepared.declared_length(), Some(5));
        let converted_response = prepared.into_response();
        assert_eq!(
            converted_response
                .headers()
                .get("content-length")
                .expect("length"),
            "5"
        );
        assert!(!converted_response.headers().contains_key("connection"));
        assert!(!converted_response.headers().contains_key("x-private"));
        assert!(!converted_response.headers().contains_key("keep-alive"));
    }

    #[test]
    fn connection_nominated_content_length_stays_removed() {
        let source_response = response_builder()
            .status(StatusCode::OK)
            .header("connection", "content-length")
            .header("content-length", "5")
            .body(Body::from("hello"))
            .expect("response");

        let prepared =
            prepare_response_egress(&Method::GET, source_response).expect("prepared response");
        assert_eq!(prepared.declared_length(), None);
        assert!(
            !prepared
                .into_response()
                .headers()
                .contains_key("content-length")
        );
    }

    #[test]
    fn strips_every_hop_by_hop_field_across_connection_lines() {
        let source_response = response_builder()
            .status(StatusCode::OK)
            .header("connection", "x-first")
            .header("connection", "x-second, upgrade")
            .header("x-first", "private")
            .header("x-second", "private")
            .header("keep-alive", "timeout=5")
            .header("proxy-authenticate", "challenge")
            .header("proxy-authorization", "credential")
            .header("proxy-connection", "keep-alive")
            .header("te", "trailers")
            .header("transfer-encoding", "chunked")
            .header("upgrade", "websocket")
            .body(Body::from("body"))
            .expect("response");

        let converted_response = prepare_response_egress(&Method::GET, source_response)
            .expect("prepared response")
            .into_response();
        for name in [
            "connection",
            "x-first",
            "x-second",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(
                !converted_response.headers().contains_key(name),
                "retained {name}"
            );
        }
    }

    #[test]
    fn accepts_whitespace_and_rejects_buffered_length_mismatch() {
        let spaced = response_builder()
            .status(StatusCode::OK)
            .header("content-length", " 5 , 5 ")
            .body(Body::from("hello"))
            .expect("response");
        assert_eq!(
            prepare_response_egress(&Method::GET, spaced)
                .expect("whitespace is valid")
                .declared_length(),
            Some(5)
        );

        let mismatch = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "4")
            .body(Body::from("hello"))
            .expect("response");
        let error = prepare_error(
            &Method::GET,
            mismatch,
            "buffered mismatch must fail before commit",
        );
        assert_eq!(error, ResponseEgressFramingError::ContentLengthMismatch);
    }

    #[test]
    fn exact_declared_stream_reaches_clean_eof() {
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "4")
            .body(Body::from_stream(stream::iter([
                Ok(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"cd")),
            ])))
            .expect("response");
        let mut body = prepare_response_egress(&Method::GET, response)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream");

        block_on(async {
            assert_eq!(body.next().await.expect("first").expect("chunk"), "ab");
            assert_eq!(body.next().await.expect("second").expect("chunk"), "cd");
            assert!(body.next().await.is_none());
        });
    }

    #[test]
    fn head_suppression_releases_stream_without_polling_and_preserves_length() {
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
            let _keep_signal_alive = &signal;
            observed_polls.set(observed_polls.get() + 1_usize);
            Poll::Ready(Some(Ok(Bytes::from_static(b"body"))))
        }));
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "99")
            .body(body)
            .expect("response");

        let prepared = prepare_response_egress(&Method::HEAD, response).expect("prepared response");

        assert!(!prepared.transmits_body());
        assert_eq!(prepared.declared_length(), Some(99));
        assert_eq!(polls.get(), 0_usize);
        assert_eq!(drops.get(), 1_usize);
        assert_eq!(
            prepared
                .into_response()
                .body()
                .as_bytes()
                .expect("suppressed body"),
            b""
        );
    }

    #[test]
    fn declared_stream_length_rejects_overrun_and_early_eof() {
        let overrun = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "3")
            .body(Body::from_stream(stream::iter([
                Ok(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"cd")),
            ])))
            .expect("response");
        let mut overrun_stream = prepare_response_egress(&Method::GET, overrun)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream body");
        block_on(async {
            assert_eq!(
                overrun_stream
                    .next()
                    .await
                    .expect("first item")
                    .expect("first chunk"),
                Bytes::from_static(b"ab")
            );
            let error = overrun_stream
                .next()
                .await
                .expect("overrun item")
                .expect_err("overrun error");
            assert_eq!(
                response_egress_body_length_error(&error),
                Some(ResponseEgressBodyLengthError::Exceeded)
            );
            assert!(overrun_stream.next().await.is_none());
        });

        let early_eof = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "3")
            .body(Body::from_stream(stream::iter([Ok(Bytes::from_static(
                b"ab",
            ))])))
            .expect("response");
        let mut early_eof_stream = prepare_response_egress(&Method::GET, early_eof)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream body");
        block_on(async {
            assert_eq!(
                early_eof_stream
                    .next()
                    .await
                    .expect("chunk")
                    .expect("valid first chunk"),
                Bytes::from_static(b"ab")
            );
            let error = early_eof_stream
                .next()
                .await
                .expect("early EOF item")
                .expect_err("early EOF error");
            assert_eq!(
                response_egress_body_length_error(&error),
                Some(ResponseEgressBodyLengthError::Incomplete)
            );
            assert!(early_eof_stream.next().await.is_none());
        });
    }

    #[test]
    fn terminal_stream_error_releases_source_before_yield() {
        let overrun_drops = Rc::new(Cell::new(0_usize));
        let overrun = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "1")
            .body(tracked_body(
                vec![Ok(Bytes::from_static(b"over"))],
                &overrun_drops,
            ))
            .expect("response");
        let mut overrun_stream = prepare_response_egress(&Method::GET, overrun)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream body");
        block_on(overrun_stream.next())
            .expect("overrun result")
            .expect_err("overrun error");
        assert_eq!(overrun_drops.get(), 1_usize);

        let source_drops = Rc::new(Cell::new(0_usize));
        let source_error = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "1")
            .body(tracked_body(
                vec![Err(EdgeError::bad_gateway("source failed"))],
                &source_drops,
            ))
            .expect("response");
        let mut source_stream = prepare_response_egress(&Method::GET, source_error)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream body");
        block_on(source_stream.next())
            .expect("source result")
            .expect_err("source error");
        assert_eq!(source_drops.get(), 1_usize);

        let eof_drops = Rc::new(Cell::new(0_usize));
        let early_eof = response_builder()
            .status(StatusCode::OK)
            .header("content-length", "1")
            .body(tracked_body(Vec::new(), &eof_drops))
            .expect("response");
        let mut eof_stream = prepare_response_egress(&Method::GET, early_eof)
            .expect("prepared response")
            .into_response()
            .into_body()
            .into_stream()
            .expect("stream body");
        block_on(eof_stream.next())
            .expect("early EOF result")
            .expect_err("early EOF error");
        assert_eq!(eof_drops.get(), 1_usize);
    }

    #[test]
    fn rejects_application_trailer_declaration_before_commit() {
        let response = response_builder()
            .status(StatusCode::OK)
            .header("trailer", "x-checksum")
            .body(Body::from("body"))
            .expect("response");

        let error = prepare_error(&Method::GET, response, "trailers must be rejected");
        assert_eq!(
            error,
            super::ResponseEgressFramingError::UnsupportedTrailers
        );
    }

    #[test]
    fn rejects_every_invalid_content_length_shape_before_suppression() {
        for value in ["", "+1", "-1", "1a", "1,,1", "1, 2", "18446744073709551616"] {
            let response = response_builder()
                .status(StatusCode::NO_CONTENT)
                .header("content-length", value)
                .body(Body::empty())
                .expect("response");
            let error = prepare_error(
                &Method::GET,
                response,
                "invalid content-length was accepted",
            );
            assert_eq!(error, ResponseEgressFramingError::InvalidContentLength);
        }
    }

    #[test]
    fn applies_status_suppression_before_method_suppression() {
        let no_content = response_builder()
            .status(StatusCode::NO_CONTENT)
            .header("content-length", "9")
            .body(Body::from("ignored"))
            .expect("response");
        let no_content_prepared =
            prepare_response_egress(&Method::HEAD, no_content).expect("204 response");
        assert!(!no_content_prepared.transmits_body());
        assert_eq!(no_content_prepared.declared_length(), None);
        assert!(
            !no_content_prepared
                .into_response()
                .headers()
                .contains_key("content-length")
        );

        let reset_content = response_builder()
            .status(StatusCode::RESET_CONTENT)
            .header("content-length", "1")
            .body(Body::empty())
            .expect("response");
        let reset_error = prepare_error(
            &Method::GET,
            reset_content,
            "205 with nonzero length must fail",
        );
        assert_eq!(
            reset_error,
            ResponseEgressFramingError::ContentLengthMismatch
        );

        let not_modified = response_builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("content-length", "9")
            .body(Body::from("ignored"))
            .expect("response");
        let not_modified_prepared =
            prepare_response_egress(&Method::GET, not_modified).expect("304 response");
        assert!(!not_modified_prepared.transmits_body());
        assert_eq!(not_modified_prepared.declared_length(), Some(9));

        let zero_reset_content = response_builder()
            .status(StatusCode::RESET_CONTENT)
            .header("content-length", "0")
            .body(Body::empty())
            .expect("response");
        let zero_reset_prepared =
            prepare_response_egress(&Method::GET, zero_reset_content).expect("zero-length 205");
        assert!(!zero_reset_prepared.transmits_body());
        assert_eq!(zero_reset_prepared.declared_length(), Some(0));

        let nominated_nonzero = response_builder()
            .status(StatusCode::RESET_CONTENT)
            .header("connection", "content-length")
            .header("content-length", "1")
            .body(Body::empty())
            .expect("response");
        let nominated_error = prepare_error(
            &Method::GET,
            nominated_nonzero,
            "205 validation must precede connection nomination stripping",
        );
        assert_eq!(
            nominated_error,
            ResponseEgressFramingError::ContentLengthMismatch
        );
    }

    #[test]
    fn rejects_tunnel_and_informational_responses_and_removes_transfer_encoding() {
        for status in [StatusCode::CONTINUE, StatusCode::SWITCHING_PROTOCOLS] {
            let informational = response_builder()
                .status(status)
                .body(Body::empty())
                .expect("response");
            let informational_error = prepare_error(
                &Method::GET,
                informational,
                "final informational response must fail",
            );
            assert_eq!(
                informational_error,
                ResponseEgressFramingError::UnsupportedStatus
            );
        }

        let tunnel = response_builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .expect("response");
        let tunnel_error = prepare_error(&Method::CONNECT, tunnel, "successful CONNECT must fail");
        assert_eq!(tunnel_error, ResponseEgressFramingError::UnsupportedTunnel);

        let transfer_encoded = response_builder()
            .status(StatusCode::OK)
            .header("transfer-encoding", "chunked")
            .body(Body::from("body"))
            .expect("response");
        let transfer_response = prepare_response_egress(&Method::GET, transfer_encoded)
            .expect("response")
            .into_response();
        assert!(
            !transfer_response
                .headers()
                .contains_key("transfer-encoding")
        );

        let informational_with_bad_connection = response_builder()
            .status(StatusCode::CONTINUE)
            .header("connection", "not a valid field name")
            .header("trailer", "x-checksum")
            .body(Body::empty())
            .expect("response");
        let informational_precedence_error = prepare_error(
            &Method::GET,
            informational_with_bad_connection,
            "status rejection must win",
        );
        assert_eq!(
            informational_precedence_error,
            ResponseEgressFramingError::UnsupportedStatus
        );

        let tunnel_with_trailer = response_builder()
            .status(StatusCode::OK)
            .header("trailer", "x-checksum")
            .body(Body::empty())
            .expect("response");
        let tunnel_precedence_error = prepare_error(
            &Method::CONNECT,
            tunnel_with_trailer,
            "tunnel rejection must win",
        );
        assert_eq!(
            tunnel_precedence_error,
            ResponseEgressFramingError::UnsupportedTunnel
        );
    }
}
