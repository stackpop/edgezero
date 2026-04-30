use std::time::Duration;

use async_trait::async_trait;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderName, HeaderValue, Method, StatusCode};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use futures_util::StreamExt as _;
use reqwest::{header, Client};

pub struct AxumProxyClient {
    client: Client,
}

impl AxumProxyClient {
    /// Construct a proxy client with the workspace-default 30-second timeout.
    ///
    /// **Breaking change (pre-1.0):** previously `AxumProxyClient` implemented
    /// `Default` and panicked if reqwest's TLS backend could not be initialised.
    /// Construction is now fallible so callers can decide how to handle a
    /// missing or misconfigured TLS backend.
    ///
    /// # Errors
    /// Returns the underlying [`reqwest::Error`] if `reqwest::Client::builder().build()`
    /// fails — typically because the TLS backend cannot be initialised on this target.
    #[inline]
    pub fn try_new() -> Result<Self, reqwest::Error> {
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self { client })
    }
}

#[async_trait(?Send)]
impl ProxyClient for AxumProxyClient {
    #[inline]
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _extensions) = request.into_parts();
        let reqwest_method = reqwest_method(&method)?;
        let mut builder = self.client.request(reqwest_method, uri.to_string());

        for (name, value) in &headers {
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
                while let Some(result) = stream.next().await {
                    let chunk = result.map_err(EdgeError::internal)?;
                    buf.extend_from_slice(&chunk);
                }
                builder.body(buf)
            }
        };

        let response = builder.send().await.map_err(EdgeError::internal)?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).map_err(EdgeError::internal)?;
        let mut proxy_response = ProxyResponse::new(status, Body::empty());

        for (name, value) in response.headers() {
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
    use std::mem;

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
        let client = AxumProxyClient::try_new().expect("reqwest client init");
        // Just verify it builds without panicking
        assert!(mem::size_of_val(&client) > 0);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::body::Bytes as AxumBytes;
    use axum::http::header::CONTENT_TYPE;
    use axum::http::{HeaderMap as AxumHeaderMap, StatusCode as AxumStatusCode};
    use axum::routing::{delete, get, patch, post, put};
    use axum::Router;
    use edgezero_core::http::Uri;
    use tokio::net::TcpListener;

    async fn start_test_server(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn proxy_client_sends_get_request() {
        let app = Router::new().route("/test", get(|| async { "hello from server" }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/test").parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"hello from server"),
            Body::Stream(_) => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_sends_post_with_body() {
        let app = Router::new().route("/echo", post(|body: AxumBytes| async move { body }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/echo").parse().unwrap();
        let mut request = ProxyRequest::new(Method::POST, uri);
        *request.body_mut() = Body::from("request body data");

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"request body data"),
            Body::Stream(_) => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_forwards_request_headers() {
        let app = Router::new().route(
            "/headers",
            get(|headers: AxumHeaderMap| async move {
                headers
                    .get("x-custom-header")
                    .and_then(|val| val.to_str().ok())
                    .unwrap_or("missing")
                    .to_owned()
            }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/headers").parse().unwrap();
        let mut request = ProxyRequest::new(Method::GET, uri);
        request
            .headers_mut()
            .insert("x-custom-header", HeaderValue::from_static("custom-value"));

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        match response.body() {
            Body::Once(bytes) => assert_eq!(bytes.as_ref(), b"custom-value"),
            Body::Stream(_) => panic!("expected buffered body"),
        }
    }

    #[tokio::test]
    async fn proxy_client_receives_response_headers() {
        let app = Router::new().route(
            "/with-headers",
            get(|| async { ([(CONTENT_TYPE, "application/json")], "{}") }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/with-headers").parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|val| val.to_str().ok());
        assert_eq!(content_type, Some("application/json"));
    }

    #[tokio::test]
    async fn proxy_client_handles_404() {
        let app = Router::new();
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/nonexistent").parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn proxy_client_handles_500() {
        let app = Router::new().route(
            "/error",
            get(|| async { (AxumStatusCode::INTERNAL_SERVER_ERROR, "error") }),
        );
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/error").parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn proxy_client_handles_various_methods() {
        let app = Router::new()
            .route("/method", get(|| async { "GET" }))
            .route("/method", post(|| async { "POST" }))
            .route("/method", put(|| async { "PUT" }))
            .route("/method", delete(|| async { "DELETE" }))
            .route("/method", patch(|| async { "PATCH" }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");

        for (method, expected_body) in [
            (Method::GET, "GET"),
            (Method::POST, "POST"),
            (Method::PUT, "PUT"),
            (Method::DELETE, "DELETE"),
            (Method::PATCH, "PATCH"),
        ] {
            let uri: Uri = format!("{base_url}/method").parse().unwrap();
            let request = ProxyRequest::new(method, uri);
            let response = client.send(request).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            match response.body() {
                Body::Once(bytes) => assert_eq!(bytes.as_ref(), expected_body.as_bytes()),
                Body::Stream(_) => panic!("expected buffered body"),
            }
        }
    }

    #[tokio::test]
    async fn proxy_client_handles_connection_refused() {
        let client = AxumProxyClient::try_new().expect("reqwest client init");
        // Use a port that's unlikely to have anything running
        let uri: Uri = "http://127.0.0.1:1".parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        client
            .send(request)
            .await
            .expect_err("expected connection refused");
    }

    #[tokio::test]
    async fn proxy_client_sends_streaming_body() {
        use bytes::Bytes;
        use futures::stream;

        let app = Router::new().route("/stream-echo", post(|body: AxumBytes| async move { body }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::try_new().expect("reqwest client init");
        let uri: Uri = format!("{base_url}/stream-echo").parse().unwrap();
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
            Body::Stream(_) => panic!("expected buffered body"),
        }
    }
}
