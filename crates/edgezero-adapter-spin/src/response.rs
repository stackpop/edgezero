use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use futures_util::StreamExt;
use spin_sdk::http as spin_http;

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
pub async fn from_core_response(response: Response) -> Result<spin_http::Response, EdgeError> {
    let (parts, body) = response.into_parts();

    let mut builder = spin_http::Response::builder();
    builder.status(parts.status.as_u16());

    // Spin's WASI HTTP interface requires string-typed header values,
    // so non-UTF-8 values cannot be forwarded and are dropped with a warning.
    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            builder.header(name.as_str(), v);
        } else {
            log::warn!(
                "dropping non-UTF-8 response header (Spin WASI limitation): {}",
                name
            );
        }
    }

    let body_bytes = collect_body_bytes(body).await?;

    builder.body(body_bytes);
    Ok(builder.build())
}
