use std::io;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_core::body::Body;
use edgezero_core::compression::{
    decode_brotli_stream, decode_deflate_stream, decode_gzip_stream, ContentEncoding,
};
use edgezero_core::error::EdgeError;
use edgezero_core::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use futures_util::stream::{BoxStream, StreamExt};
use reqwest::Client;

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
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
                .map_err(EdgeError::internal)?;
            let header_value = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                .map_err(EdgeError::internal)?;
            builder = builder.header(header_name, header_value);
        }

        // Use collect() for streaming bodies so we don't lose data.
        let body_bytes = body.collect().await.map_err(EdgeError::internal)?;
        if !body_bytes.is_empty() {
            builder = builder.body(body_bytes.to_vec());
        }

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

        // Detect Content-Encoding for decompression.
        let encoding = proxy_response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .and_then(ContentEncoding::parse);

        // Stream the response body instead of buffering the whole thing.
        let byte_stream = response.bytes_stream();
        let chunk_stream: BoxStream<'static, Result<Vec<u8>, io::Error>> = byte_stream
            .map(|res| match res {
                Ok(bytes) => Ok(bytes.to_vec()),
                Err(err) => Err(io::Error::other(err.to_string())),
            })
            .boxed();

        let body_stream = transform_stream(chunk_stream, encoding);
        *proxy_response.body_mut() = Body::from_stream(body_stream);

        if matches!(
            encoding,
            Some(ContentEncoding::Gzip)
                | Some(ContentEncoding::Brotli)
                | Some(ContentEncoding::Deflate)
        ) {
            proxy_response
                .headers_mut()
                .remove(header::CONTENT_ENCODING);
            proxy_response.headers_mut().remove(header::CONTENT_LENGTH);
        }

        Ok(proxy_response)
    }
}

fn transform_stream(
    stream: BoxStream<'static, Result<Vec<u8>, io::Error>>,
    encoding: Option<ContentEncoding>,
) -> BoxStream<'static, Result<Bytes, io::Error>> {
    match encoding {
        Some(ContentEncoding::Gzip) => decode_gzip_stream(stream).boxed(),
        Some(ContentEncoding::Brotli) => decode_brotli_stream(stream).boxed(),
        Some(ContentEncoding::Deflate) => decode_deflate_stream(stream).boxed(),
        _ => stream.map(|res| res.map(Bytes::from)).boxed(),
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

        let bytes = response.into_body().collect().await.expect("collect body");
        assert_eq!(bytes.as_ref(), b"hello from server");
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

        let bytes = response.into_body().collect().await.expect("collect body");
        assert_eq!(bytes.as_ref(), b"request body data");
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

        let bytes = response.into_body().collect().await.expect("collect body");
        assert_eq!(bytes.as_ref(), b"custom-value");
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
            let bytes = response.into_body().collect().await.expect("collect body");
            assert_eq!(bytes.as_ref(), expected_body.as_bytes());
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

        let bytes = response.into_body().collect().await.expect("collect body");
        assert_eq!(bytes.as_ref(), b"chunk1chunk2chunk3");
    }

    #[tokio::test]
    async fn proxy_response_body_is_streamed() {
        // Verify the response body comes back as a stream, not a buffered body.
        let app = Router::new().route("/test", get(|| async { "streamed" }));
        let base_url = start_test_server(app).await;

        let client = AxumProxyClient::default();
        let uri: Uri = format!("{}/test", base_url).parse().unwrap();
        let request = ProxyRequest::new(Method::GET, uri);

        let response = client.send(request).await.expect("response");
        assert!(
            response.body().is_stream(),
            "proxy response should be a stream"
        );
        let bytes = response.into_body().collect().await.expect("collect body");
        assert_eq!(bytes.as_ref(), b"streamed");
    }
}
