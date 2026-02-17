use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::StatusCode;
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use futures_util::StreamExt;

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

        for (name, value) in headers.iter() {
            if let Ok(v) = value.to_str() {
                builder.header(name.as_str(), v);
            }
        }

        let body_bytes = match body {
            Body::Once(bytes) => bytes.to_vec(),
            Body::Stream(mut stream) => {
                // Spin doesn't support streaming outbound bodies; collect into bytes.
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
        let spin_request = builder.build();

        let spin_response: spin_sdk::http::Response = spin_sdk::http::send(spin_request)
            .await
            .map_err(|e| EdgeError::internal(anyhow::anyhow!("Spin outbound HTTP error: {}", e)))?;

        let status = StatusCode::from_u16(*spin_response.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // Collect response headers before consuming the body.
        let response_headers: Vec<_> = spin_response
            .headers()
            .filter_map(|(name, value)| {
                let v = value.as_str()?;
                let hname = edgezero_core::http::HeaderName::from_bytes(name.as_bytes()).ok()?;
                let hval: edgezero_core::http::HeaderValue = v.parse().ok()?;
                Some((hname, hval))
            })
            .collect();

        let response_body = spin_response.into_body();
        let mut proxy_response = ProxyResponse::new(status, Body::from(response_body));

        for (name, value) in response_headers {
            proxy_response.headers_mut().insert(name, value);
        }

        proxy_response
            .headers_mut()
            .insert("x-edgezero-proxy", "spin".parse().unwrap());

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
