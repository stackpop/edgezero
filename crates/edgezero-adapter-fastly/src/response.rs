use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, Uri};
use fastly::Response as FastlyResponse;
use futures_util::StreamExt;
use std::io::Write;

pub fn from_core_response(response: Response) -> Result<FastlyResponse, EdgeError> {
    let (parts, body) = response.into_parts();
    let mut fastly_response = FastlyResponse::from_status(parts.status.as_u16());

    for (name, value) in parts.headers.iter() {
        fastly_response.set_header(name.as_str(), value.as_bytes());
    }

    match body {
        Body::Once(bytes) => {
            fastly_response.set_body(bytes.to_vec());
            Ok(fastly_response)
        }
        Body::Stream(mut stream) => {
            // Use stream_to_client() for true incremental delivery. This sends
            // the response headers immediately and returns a StreamingBody we
            // can write chunks to as they arrive.  After streaming, the Fastly
            // runtime ignores the return value of the main function.
            //
            // NOTE: The returned FastlyResponse is an empty placeholder — the
            // real response is already on the wire. Callers should not inspect
            // the returned value for status/headers/body when the original
            // response had a streaming body.
            let mut streaming = fastly_response.stream_to_client();
            while let Some(chunk) = futures::executor::block_on(stream.next()) {
                let chunk = chunk.map_err(EdgeError::internal)?;
                streaming.write_all(&chunk).map_err(EdgeError::internal)?;
            }
            streaming.finish().map_err(EdgeError::internal)?;
            Ok(FastlyResponse::new())
        }
    }
}

pub fn parse_uri(uri: &str) -> Result<Uri, EdgeError> {
    uri.parse::<Uri>()
        .map_err(|err| EdgeError::bad_request(format!("invalid request URI: {}", err)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use edgezero_core::body::Body;
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
}
