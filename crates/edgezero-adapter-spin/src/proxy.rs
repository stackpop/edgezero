use crate::response::collect_body_bytes;
use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::StatusCode;
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};

/// A proxy client that uses Spin's outbound HTTP (`spin_sdk::http::send`)
/// to forward requests to upstream services.
pub struct SpinProxyClient;

#[async_trait(?Send)]
impl ProxyClient for SpinProxyClient {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();

        let mut builder = spin_sdk::http::Request::builder();
        builder
            .method(into_spin_method(&method))
            .uri(uri.to_string());

        // Spin's WASI HTTP interface requires string-typed header values,
        // so non-UTF-8 values cannot be forwarded and are dropped with a warning.
        for (name, value) in headers.iter() {
            if let Ok(v) = value.to_str() {
                builder.header(name.as_str(), v);
            } else {
                log::warn!(
                    "dropping non-UTF-8 proxy request header (Spin WASI limitation): {}",
                    name
                );
            }
        }

        let body_bytes = collect_body_bytes(body).await?;

        builder.body(body_bytes);
        let spin_request = builder.build();

        let spin_response: spin_sdk::http::Response = spin_sdk::http::send(spin_request)
            .await
            .map_err(|e| EdgeError::internal(anyhow::anyhow!("Spin outbound HTTP error: {e}")))?;

        let status = StatusCode::from_u16(*spin_response.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Collect response headers before consuming the body.
        let mut response_headers = Vec::new();
        for (name, value) in spin_response.headers() {
            let Ok(hname) = edgezero_core::http::HeaderName::from_bytes(name.as_bytes()) else {
                log::warn!("dropping invalid proxy response header name: {}", name);
                continue;
            };
            match edgezero_core::http::HeaderValue::from_bytes(value.as_bytes()) {
                Ok(hval) => response_headers.push((hname, hval)),
                Err(_) => {
                    log::warn!("dropping invalid proxy response header value for: {}", name);
                }
            }
        }

        let response_body = spin_response.into_body();
        let mut proxy_response = ProxyResponse::new(status, Body::from(response_body));

        for (name, value) in response_headers {
            proxy_response.headers_mut().insert(name, value);
        }

        proxy_response.headers_mut().insert(
            "x-edgezero-proxy",
            "spin".parse().expect("static header value should parse"),
        );

        Ok(proxy_response)
    }
}

fn into_spin_method(method: &edgezero_core::http::Method) -> spin_sdk::http::Method {
    match *method {
        edgezero_core::http::Method::GET => spin_sdk::http::Method::Get,
        edgezero_core::http::Method::POST => spin_sdk::http::Method::Post,
        edgezero_core::http::Method::PUT => spin_sdk::http::Method::Put,
        edgezero_core::http::Method::DELETE => spin_sdk::http::Method::Delete,
        edgezero_core::http::Method::PATCH => spin_sdk::http::Method::Patch,
        edgezero_core::http::Method::HEAD => spin_sdk::http::Method::Head,
        edgezero_core::http::Method::OPTIONS => spin_sdk::http::Method::Options,
        edgezero_core::http::Method::CONNECT => spin_sdk::http::Method::Connect,
        edgezero_core::http::Method::TRACE => spin_sdk::http::Method::Trace,
        ref other => spin_sdk::http::Method::Other(other.to_string()),
    }
}
