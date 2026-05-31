use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use futures_util::StreamExt;
use spin_sdk::http::FullBody;

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
                        if collected.len() + bytes.len() > MAX_BODY_SIZE {
                            return Err(EdgeError::internal(anyhow::anyhow!(
                                "body exceeds maximum size of {} bytes",
                                MAX_BODY_SIZE
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

/// Convert an EdgeZero core `Response` into a Spin SDK `Response`.
///
/// Both `Body::Once` and `Body::Stream` are converted to a buffered
/// byte body. Streaming bodies are collected into a single `Vec<u8>`.
pub async fn from_core_response(
    response: Response,
) -> Result<spin_sdk::http::Response<FullBody<Bytes>>, EdgeError> {
    let (parts, body) = response.into_parts();

    let mut builder = spin_sdk::http::Response::builder().status(parts.status);

    for (name, value) in parts.headers.iter() {
        builder = builder.header(name, value);
    }

    let body_bytes = collect_body_bytes(body).await?;

    builder
        .body(FullBody::new(Bytes::from(body_bytes)))
        .map_err(|e| EdgeError::internal(anyhow::anyhow!("failed to build response: {e}")))
}
