use axum::body::Body as AxumBody;
use axum::http::{Response, StatusCode};
use futures::executor::block_on;
use futures_util::{pin_mut, StreamExt};
use tracing::error;

use edgezero_core::body::Body;
use edgezero_core::http::Response as CoreResponse;

/// Convert an EdgeZero response into one consumable by Axum/Hyper.
///
/// Streaming responses are collected into an in-memory buffer. While this sacrifices
/// incremental flushing, it keeps the adapter compatible with the non-`Send` streaming type used by
/// `edgezero_core::Body` and works well for local development.
pub fn into_axum_response(response: CoreResponse) -> Response<AxumBody> {
    let (parts, body) = response.into_parts();
    let body = match body {
        Body::Once(bytes) => AxumBody::from(bytes),
        Body::Stream(stream) => {
            let result = block_on(async {
                let mut buf = Vec::new();
                let stream = stream;
                pin_mut!(stream);
                while let Some(chunk) = stream.next().await {
                    let bytes = chunk?;
                    buf.extend_from_slice(&bytes);
                }
                Ok::<Vec<u8>, anyhow::Error>(buf)
            });
            match result {
                Ok(buf) => AxumBody::from(buf),
                Err(err) => {
                    error!("streaming response error: {err}");
                    let body = AxumBody::from("streaming response error");
                    let mut response = Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(body)
                        .expect("error response");
                    response.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    return response;
                }
            }
        }
    };

    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::{response_builder, StatusCode};
    use futures::stream;

    #[test]
    fn converts_core_response_stream_into_axum_body() {
        let stream = stream::iter(vec![
            Ok::<_, anyhow::Error>(bytes::Bytes::from_static(b"hel")),
            Ok(bytes::Bytes::from_static(b"lo")),
        ]);
        let body = Body::from_stream(stream);
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(body)
            .expect("response");

        let axum_response = into_axum_response(response);
        assert_eq!(axum_response.status(), StatusCode::OK);
        assert_eq!(
            axum_response
                .headers()
                .get("content-type")
                .expect("header")
                .to_str()
                .unwrap(),
            "text/plain"
        );

        let collected = block_on(async {
            let mut data = Vec::new();
            let mut stream = axum_response.into_body().into_data_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("chunk");
                data.extend_from_slice(&chunk);
            }
            data
        });

        assert_eq!(collected, b"hello");
    }
}
