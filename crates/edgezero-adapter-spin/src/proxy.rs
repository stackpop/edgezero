use crate::decompress::decompress_body;
use crate::response::collect_body_bytes;
use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::header;
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use spin_sdk::http::body::IncomingBodyExt;
use spin_sdk::http::FullBody;

/// A proxy client that uses Spin's outbound HTTP (`spin_sdk::http::send`)
/// to forward requests to upstream services.
pub struct SpinProxyClient;

#[async_trait(?Send)]
impl ProxyClient for SpinProxyClient {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();

        let mut builder = spin_sdk::http::Request::builder()
            .method(method)
            .uri(uri.to_string());

        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }

        let body_bytes = collect_body_bytes(body).await?;

        let spin_request = builder
            .body(FullBody::new(Bytes::from(body_bytes)))
            .map_err(|e| {
                EdgeError::internal(anyhow::anyhow!("failed to build proxy request: {e}"))
            })?;

        let spin_response = spin_sdk::http::send(spin_request)
            .await
            .map_err(|e| EdgeError::internal(anyhow::anyhow!("Spin outbound HTTP error: {e}")))?;

        let (response_parts, response_body) = spin_response.into_parts();

        let encoding = response_parts
            .headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);

        let body_bytes = response_body.bytes().await.map_err(|e| {
            EdgeError::internal(anyhow::anyhow!("failed to read proxy response body: {e}"))
        })?;
        let decompressed = decompress_body(body_bytes.to_vec(), encoding.as_deref())?;
        let mut proxy_response =
            ProxyResponse::new(response_parts.status, Body::from(decompressed));

        for (name, value) in response_parts.headers.iter() {
            proxy_response
                .headers_mut()
                .insert(name.clone(), value.clone());
        }

        // Strip encoding headers after decompression so downstream
        // handlers see plain bytes (consistent with Fastly/Cloudflare).
        if matches!(encoding.as_deref(), Some("gzip") | Some("br")) {
            proxy_response
                .headers_mut()
                .remove(header::CONTENT_ENCODING);
            proxy_response.headers_mut().remove(header::CONTENT_LENGTH);
        }

        proxy_response.headers_mut().insert(
            edgezero_core::proxy::PROXY_HEADER,
            "spin".parse().expect("static header value should parse"),
        );

        Ok(proxy_response)
    }
}
