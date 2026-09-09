use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, Uri};
use edgezero_core::outbound::collect_response_stream;
use fastly::Response as FastlyResponse;
use futures::executor;

pub const FASTLY_RESPONSE_STREAM_BUFFER_BYTES: u64 = 0x0100_0000;

/// # Errors
/// Returns [`EdgeError::Internal`] if the response body cannot be streamed to the Fastly send-channel.
#[inline]
pub fn from_core_response(response: Response) -> Result<FastlyResponse, EdgeError> {
    let (parts, body) = response.into_parts();
    let mut fastly_response = FastlyResponse::from_status(parts.status.as_u16());

    match body {
        Body::Once(bytes) => fastly_response.set_body(bytes.to_vec()),
        Body::Stream(stream) => fastly_response.set_body(
            executor::block_on(collect_response_stream(
                stream,
                FASTLY_RESPONSE_STREAM_BUFFER_BYTES,
            ))?
            .to_vec(),
        ),
    }

    // `append_header` preserves multi-value headers (e.g. N `Set-Cookie`). The
    // response starts empty (`from_status`) and `http::HeaderMap` iteration
    // yields one entry per value, so appending is unconditionally correct.
    for (name, value) in &parts.headers {
        fastly_response.append_header(name.as_str(), value.as_bytes());
    }

    Ok(fastly_response)
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
    use futures_util::stream;

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
    fn multi_value_set_cookie_survives_conversion() {
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
}
