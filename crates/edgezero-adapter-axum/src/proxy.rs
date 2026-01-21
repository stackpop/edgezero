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

    #[test]
    fn converts_all_methods_to_reqwest() {
        let cases = [
            (Method::GET, reqwest::Method::GET),
            (Method::POST, reqwest::Method::POST),
            (Method::PUT, reqwest::Method::PUT),
            (Method::DELETE, reqwest::Method::DELETE),
            (Method::PATCH, reqwest::Method::PATCH),
            (Method::HEAD, reqwest::Method::HEAD),
            (Method::OPTIONS, reqwest::Method::OPTIONS),
        ];
        for (input, expected) in cases {
            let result = reqwest_method(&input).expect("method conversion");
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn default_client_creates_successfully() {
        let client = AxumProxyClient::default();
        // Just verify it builds without panicking
        assert!(std::mem::size_of_val(&client) > 0);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::{routing::get, routing::post, Router};
    use edgezero_core::http::Uri;
    use edgezero_core::proxy::ProxyClient;
    use tokio::net::TcpListener;

    async fn start_test_server(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn proxy_client_sends_get_request() {
        let app = Router::new().route("/test", get(|| async { "hello from server" }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/test", base_url).parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"hello from server"),
            _ => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_sends_post_with_body() {
        let app = Router::new().route("/echo", post(|body: axum::body::Bytes| async move { body }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/echo", base_url).parse().unwrap();
        let mut request = ProxyRequest::new(Method::POST, uri);
        *request.body_mut() = Body::from("request body data");

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"request body data"),
            _ => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_forwards_request_headers() {
        let app = Router::new().route(
            "/headers",
            get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("x-custom-header")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("missing")
                    .to_string()
            }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/headers", base_url).parse().unwrap();
        let mut request = ProxyRequest::new(Method::GET, uri);
        request
            .headers_mut()
            .insert("x-custom-header", HeaderValue::from_static("custom-value"));

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"custom-value"),
            _ => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_receives_response_headers() {
        let app = Router::new().route(
            "/with-headers",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "{}",
                )
            }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/with-headers", base_url).parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok());
        assert_eq!(content_type, Some("application/json"));
    }

    #[tokio::test]
    async fn proxy_client_handles_404() {
        let app = Router::new();
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/nonexistent", base_url).parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn proxy_client_handles_500() {
        let app = Router::new().route(
            "/error",
            get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "error") }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/error", base_url).parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn proxy_client_handles_various_methods() {
        let app = Router::new()
            .route("/method", get(|| async { "GET" }))
            .route("/method", post(|| async { "POST" }))
            .route("/method", axum::routing::put(|| async { "PUT" }))
            .route("/method", axum::routing::delete(|| async { "DELETE" }))
            .route("/method", axum::routing::patch(|| async { "PATCH" }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();

        for (method, expected_body) in [
            (Method::GET, "GET"),
            (Method::POST, "POST"),
            (Method::PUT, "PUT"),
            (Method::DELETE, "DELETE"),
            (Method::PATCH, "PATCH"),
        ] {
            let uri: Uri = format!("{}/method", base_url).parse().unwrap();
            let request = ProxyRequest::new(method, uri);
            let response = client.send(request).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            match response.body() {
                Body::Once(bytes) => assert_eq!(bytes.as_ref(), expected_body.as_bytes()),
                _ => panic!("expected buffered body"),
            }
        }
    }

    #[tokio::test]
    async fn proxy_client_handles_connection_refused() {
        let client = AxumProxyClient::default();
        // Use a port that's unlikely to have anything running
        let uri: Uri = "http://127.0.0.1:1".parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let result = client.send(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn proxy_client_sends_streaming_body() {
        use bytes::Bytes;
        use futures::stream;

        let app = Router::new().route(
            "/stream-echo",
            post(|body: axum::body::Bytes| async move { body }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/stream-echo", base_url).parse().unwrap();
        let mut request = ProxyRequest::new(Method::POST, uri);

        // Create a streaming body - Body::stream expects Stream<Item = Bytes>
        let chunks = vec![
            Bytes::from("chunk1"),
            Bytes::from("chunk2"),
            Bytes::from("chunk3"),
        ];
        let stream = stream::iter(chunks);
        *request.body_mut() = Body::stream(stream);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"chunk1chunk2chunk3"),
            _ => panic!("expected buffered body"),
        }
    }
}
