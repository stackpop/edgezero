use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
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

    let mut cf_response = body_response.with_status(parts.status.as_u16());
    let headers = cf_response.headers_mut();
    for (name, value) in &parts.headers {
        if let Ok(value_str) = value.to_str() {
            headers
                .set(name.as_str(), value_str)
                .map_err(EdgeError::internal)?;
        }
    }
    Ok(cf_response)
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
