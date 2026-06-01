use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use futures_util::StreamExt as _;
use spin_sdk::http::{FullBody, Response as SpinResponse};

use crate::SpinFullResponse;

/// Maximum body size (16 MiB) when collecting a streamed body into a buffer.
/// Prevents unbounded memory growth from malicious or misconfigured upstreams.
///
/// Note: this cap only applies to `Body::Stream` variants.  `Body::Once` is
/// already materialised in memory and bypasses this check.  The proxy module
/// uses a separate, larger limit ([`MAX_DECOMPRESSED_SIZE`](crate::proxy) =
/// 64 MiB) because proxy responses are untrusted external data that may
/// decompress to a much larger size.
const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Collect a `Body` into a `Vec<u8>`, consuming streamed chunks if necessary.
///
/// Stream bodies are capped at [`MAX_BODY_SIZE`] bytes. If the accumulated
/// size exceeds the limit, collection stops and an error is returned.
pub(crate) async fn collect_body_bytes(body: Body) -> Result<Vec<u8>, EdgeError> {
    match body {
        Body::Once(bytes) => Ok(bytes.to_vec()),
        Body::Stream(mut stream) => {
            let mut collected = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        // `usize::saturating_add` keeps the bound check
                        // honest against pathological inputs without
                        // triggering arithmetic_side_effects.
                        if collected.len().saturating_add(bytes.len()) > MAX_BODY_SIZE {
                            return Err(EdgeError::internal(anyhow::anyhow!(
                                "body exceeds maximum size of {MAX_BODY_SIZE} bytes"
                            )));
                        }
                        collected.extend_from_slice(&bytes);
                    }
                    Err(err) => return Err(EdgeError::internal(err)),
                }
            }
            Ok(collected)
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
