use axum::body::Body as AxumBody;
use axum::http::Response;

use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Response as CoreResponse;
use edgezero_core::outbound::collect_response_stream;

pub const AXUM_RESPONSE_STREAM_BUFFER_BYTES: u64 = 0x0100_0000;

/// Convert an `EdgeZero` response into one consumable by Axum/Hyper.
///
/// Streaming responses are collected under a fixed adapter boundary because Axum requires a
/// `Send` response body while the portable core stream is intentionally local.
///
/// # Errors
/// Returns the original typed stream failure, or a typed response-limit error when the adapter
/// conversion boundary exceeds [`AXUM_RESPONSE_STREAM_BUFFER_BYTES`].
#[inline]
pub async fn into_axum_response(response: CoreResponse) -> Result<Response<AxumBody>, EdgeError> {
    let (parts, core_body) = response.into_parts();
    let body = match core_body {
        Body::Once(bytes) => AxumBody::from(bytes),
        Body::Stream(stream) => AxumBody::from(
            collect_response_stream(stream, AXUM_RESPONSE_STREAM_BUFFER_BYTES).await?,
        ),
    };

    Ok(Response::from_parts(parts, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use edgezero_core::body::Body;
    use edgezero_core::error::ResponseLimitReason;
    use edgezero_core::http::{StatusCode, response_builder};
    use futures::stream;
    use futures_util::StreamExt as _;

    #[tokio::test]
    async fn converts_core_response_stream_into_axum_body() {
        let stream = stream::iter(vec![
            Ok::<_, anyhow::Error>(bytes::Bytes::from_static(b"hel")),
            Ok(bytes::Bytes::from_static(b"lo")),
        ]);
        let body = Body::from_external_stream(stream);
        let response = response_builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(body)
            .expect("response");

        let axum_response = into_axum_response(response).await.expect("conversion");
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

        let mut data = Vec::new();
        let mut body_stream = axum_response.into_body().into_data_stream();
        while let Some(result) = body_stream.next().await {
            let chunk = result.expect("chunk");
            data.extend_from_slice(&chunk);
        }

        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn response_stream_conversion_enforces_fixed_cap() {
        let cap = usize::try_from(AXUM_RESPONSE_STREAM_BUFFER_BYTES).expect("cap fits usize");
        let exact = response_builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(stream::iter([Ok(Bytes::from(vec![
                0;
                cap
            ]))])))
            .expect("response");
        into_axum_response(exact).await.expect("exact cap");

        let over = response_builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(stream::iter([Ok(Bytes::from(vec![
                0;
                cap + 1
            ]))])))
            .expect("response");
        let error = into_axum_response(over)
            .await
            .expect_err("one byte over cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BufferedBody,
                ..
            }
        ));
    }
}
