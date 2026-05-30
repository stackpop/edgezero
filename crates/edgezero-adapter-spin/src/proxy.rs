use crate::decompress::decompress_body;
use crate::response::collect_body_bytes;
use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{header, HeaderName, HeaderValue, Method as CoreMethod, StatusCode};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse, PROXY_HEADER};
use spin_sdk::http::{
    send, Method as SpinMethod, Request as SpinRequest, Response as SpinResponse,
};

/// A proxy client that uses Spin's outbound HTTP (`spin_sdk::http::send`)
/// to forward requests to upstream services.
pub struct SpinProxyClient;

#[async_trait(?Send)]
impl ProxyClient for SpinProxyClient {
    #[inline]
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();

        let mut builder = SpinRequest::builder();
        builder
            .method(into_spin_method(&method))
            .uri(uri.to_string());

        // Spin's WASI HTTP interface requires string-typed header values,
        // so non-UTF-8 values cannot be forwarded and are dropped with a warning.
        for (name, value) in &headers {
            if let Ok(text) = value.to_str() {
                builder.header(name.as_str(), text);
            } else {
                log::warn!(
                    "dropping non-UTF-8 proxy request header (Spin WASI limitation): {name}"
                );
            }
        }

        let body_bytes = collect_body_bytes(body).await?;

        builder.body(body_bytes);
        let spin_request = builder.build();

        let spin_response: SpinResponse = send(spin_request).await.map_err(|err| {
            EdgeError::internal(anyhow::anyhow!("Spin outbound HTTP error: {err}"))
        })?;

        let status = StatusCode::from_u16(*spin_response.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Collect response headers before consuming the body.
        let mut response_headers = Vec::new();
        for (name, value) in spin_response.headers() {
            let Ok(hname) = HeaderName::from_bytes(name.as_bytes()) else {
                log::warn!("dropping invalid proxy response header name: {name}");
                continue;
            };
            match HeaderValue::from_bytes(value.as_bytes()) {
                Ok(hval) => response_headers.push((hname, hval)),
                Err(_) => {
                    log::warn!("dropping invalid proxy response header value for: {name}");
                }
            }
        }

        // Check Content-Encoding for decompression, matching the
        // Fastly/Cloudflare adapter contract.
        let encoding = response_headers
            .iter()
            .find(|(name, _)| *name == header::CONTENT_ENCODING)
            .and_then(|(_, value)| value.to_str().ok())
            .map(str::to_ascii_lowercase);

        let response_body = spin_response.into_body();
        let decompressed = decompress_body(response_body, encoding.as_deref())?;
        let mut proxy_response = ProxyResponse::new(status, Body::from(decompressed));

        for (name, value) in response_headers {
            proxy_response.headers_mut().insert(name, value);
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
        // it cannot panic at runtime — the previous `.parse().expect(...)` had
        // the same effective behaviour but tripped expect_used.
        proxy_response
            .headers_mut()
            .insert(PROXY_HEADER, HeaderValue::from_static("spin"));

        Ok(proxy_response)
    }
}

fn into_spin_method(method: &CoreMethod) -> SpinMethod {
    match method.clone() {
        CoreMethod::GET => SpinMethod::Get,
        CoreMethod::POST => SpinMethod::Post,
        CoreMethod::PUT => SpinMethod::Put,
        CoreMethod::DELETE => SpinMethod::Delete,
        CoreMethod::PATCH => SpinMethod::Patch,
        CoreMethod::HEAD => SpinMethod::Head,
        CoreMethod::OPTIONS => SpinMethod::Options,
        CoreMethod::CONNECT => SpinMethod::Connect,
        CoreMethod::TRACE => SpinMethod::Trace,
        other => SpinMethod::Other(other.to_string()),
    }
}
