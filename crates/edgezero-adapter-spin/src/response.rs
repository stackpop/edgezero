use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use edgezero_core::outbound::collect_response_stream;
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
pub(crate) async fn collect_body_bytes(body: Body) -> Result<Vec<u8>, EdgeError> {
    match body {
        Body::Once(bytes) => Ok(bytes.to_vec()),
        Body::Stream(stream) => Ok(collect_response_stream(
            stream,
            SPIN_RESPONSE_STREAM_BUFFER_BYTES,
        )
        .await?
        .to_vec()),
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
    let (parts, body) = response.into_parts();

    let mut builder = SpinResponse::builder().status(parts.status);

    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }

    let collected = collect_body_bytes(body).await?;

    builder
        .body(FullBody::new(Bytes::from(collected)))
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("failed to build response: {err}")))
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
