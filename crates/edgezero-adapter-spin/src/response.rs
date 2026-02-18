use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response;
use futures_util::StreamExt;
use spin_sdk::http as spin_http;

/// Convert an EdgeZero core `Response` into a Spin SDK `Response`.
///
/// Both `Body::Once` and `Body::Stream` are converted to a buffered
/// byte body. Streaming bodies are collected into a single `Vec<u8>`.
pub async fn from_core_response(response: Response) -> Result<spin_http::Response, EdgeError> {
    let (parts, body) = response.into_parts();

    let mut builder = spin_http::Response::builder();
    builder.status(parts.status.as_u16());

    for (name, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            builder.header(name.as_str(), v);
        }
    }

    let body_bytes = match body {
        Body::Once(bytes) => bytes.to_vec(),
        Body::Stream(mut stream) => {
            let mut collected = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => collected.extend_from_slice(&bytes),
                    Err(err) => return Err(EdgeError::internal(err)),
                }
            }
            collected
        }
    };

    builder.body(body_bytes);
    Ok(builder.build())
}
