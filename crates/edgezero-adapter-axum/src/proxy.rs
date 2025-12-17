use std::time::Duration;

use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderName, HeaderValue, Method, StatusCode};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use futures_util::StreamExt;
use reqwest::{header, Client};

pub struct AxumProxyClient {
    client: Client,
}

impl Default for AxumProxyClient {
    fn default() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self { client }
    }
}

#[async_trait(?Send)]
impl ProxyClient for AxumProxyClient {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();
        let reqwest_method = reqwest_method(&method)?;
        let mut builder = self.client.request(reqwest_method, uri.to_string());

        for (name, value) in headers.iter() {
            let header_name = header::HeaderName::from_bytes(name.as_str().as_bytes())
                .map_err(EdgeError::internal)?;
            let header_value =
                header::HeaderValue::from_bytes(value.as_bytes()).map_err(EdgeError::internal)?;
            builder = builder.header(header_name, header_value);
        }

        builder = match body {
            Body::Once(bytes) => builder.body(bytes.to_vec()),
            Body::Stream(mut stream) => {
                let mut buf = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(EdgeError::internal)?;
                    buf.extend_from_slice(&chunk);
                }
                builder.body(buf)
            }
        };

        let response = builder.send().await.map_err(EdgeError::internal)?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).map_err(EdgeError::internal)?;
        let mut proxy_response = ProxyResponse::new(status, Body::empty());

        for (name, value) in response.headers().iter() {
            let header_name =
                HeaderName::from_bytes(name.as_str().as_bytes()).map_err(EdgeError::internal)?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).map_err(EdgeError::internal)?;
            proxy_response
                .headers_mut()
                .insert(header_name, header_value);
        }

        let bytes = response.bytes().await.map_err(EdgeError::internal)?;
        *proxy_response.body_mut() = Body::from(bytes.to_vec());

        Ok(proxy_response)
    }
}

fn reqwest_method(method: &Method) -> Result<reqwest::Method, EdgeError> {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(EdgeError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_method_to_reqwest() {
        let method = Method::POST;
        let req = reqwest_method(&method).expect("reqwest method");
        assert_eq!(req, reqwest::Method::POST);
    }
}
