use crate::decompress::decompress_body;
use crate::response::collect_body_bytes;
use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{header, HeaderValue};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse, PROXY_HEADER};
use spin_sdk::http::body::IncomingBodyExt as _;
use spin_sdk::http::{send, FullBody, Request as SpinRequest};

/// A proxy client that uses Spin's outbound HTTP (`spin_sdk::http::send`)
/// to forward requests to upstream services.
pub struct SpinProxyClient;

#[async_trait(?Send)]
impl ProxyClient for SpinProxyClient {
    #[inline]
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();

        let mut builder = SpinRequest::builder().method(method).uri(uri.to_string());

        for (name, value) in &headers {
            builder = builder.header(name, value);
        }

        let request_body_bytes = collect_body_bytes(body).await?;

        let spin_request = builder
            .body(FullBody::new(Bytes::from(request_body_bytes)))
            .map_err(|err| {
                EdgeError::internal(anyhow::anyhow!("failed to build proxy request: {err}"))
            })?;

        let spin_response = send(spin_request).await.map_err(|err| {
            EdgeError::internal(anyhow::anyhow!("Spin outbound HTTP error: {err}"))
        })?;

        let (response_parts, response_body) = spin_response.into_parts();

        let encoding = response_parts
            .headers
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase);

        let response_body_bytes = response_body.bytes().await.map_err(|err| {
            EdgeError::internal(anyhow::anyhow!("failed to read proxy response body: {err}"))
        })?;
        let decompressed = decompress_body(response_body_bytes.to_vec(), encoding.as_deref())?;
        let mut proxy_response =
            ProxyResponse::new(response_parts.status, Body::from(decompressed));

        for (name, value) in &response_parts.headers {
            proxy_response
                .headers_mut()
                .insert(name.clone(), value.clone());
        }

        // Strip encoding headers after decompression so downstream
        // handlers see plain bytes (consistent with Fastly/Cloudflare).
        if matches!(encoding.as_deref(), Some("gzip" | "br")) {
            proxy_response
                .headers_mut()
                .remove(header::CONTENT_ENCODING);
            proxy_response.headers_mut().remove(header::CONTENT_LENGTH);
        }

        // `HeaderValue::from_static("spin")` is infallible at compile time so
        // it cannot panic at runtime — replaces the previous
        // `.parse().expect(...)` which tripped expect_used under restriction.
        proxy_response
            .headers_mut()
            .insert(PROXY_HEADER, HeaderValue::from_static("spin"));

        Ok(proxy_response)
    }
}
