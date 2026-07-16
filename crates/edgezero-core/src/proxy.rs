use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::body::Body;
use crate::error::EdgeError;
use crate::http::{
    Extensions, HeaderMap, Method, Request, Response, StatusCode, Uri, response_builder,
};

/// Header name attached to proxied responses to identify which adapter
/// forwarded the request (e.g. "fastly", "cloudflare", "spin").
pub const PROXY_HEADER: &str = "x-edgezero-proxy";

#[async_trait(?Send)]
pub trait ProxyClient: Send + Sync {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError>;
}

#[derive(Clone)]
pub struct ProxyHandle {
    client: Arc<dyn ProxyClient>,
}

impl ProxyHandle {
    #[must_use]
    #[inline]
    pub fn client(&self) -> Arc<dyn ProxyClient> {
        Arc::clone(&self.client)
    }

    /// # Errors
    /// Returns [`EdgeError`] if the underlying [`ProxyClient`] fails or the
    /// response cannot be assembled.
    #[inline]
    pub async fn forward(&self, request: ProxyRequest) -> Result<Response, EdgeError> {
        let response = self.client.send(request).await?;
        response.into_response()
    }

    #[inline]
    pub fn new(client: Arc<dyn ProxyClient>) -> Self {
        Self { client }
    }

    #[inline]
    pub fn with_client<C>(client: C) -> Self
    where
        C: ProxyClient + 'static,
    {
        Self {
            client: Arc::new(client),
        }
    }
}

/// Outbound request description for a proxy operation.
pub struct ProxyRequest {
    body: Body,
    extensions: Extensions,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
}

impl fmt::Debug for ProxyRequest {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyRequest")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl ProxyRequest {
    #[inline]
    pub fn body(&self) -> &Body {
        &self.body
    }

    #[inline]
    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    #[inline]
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    #[inline]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    #[inline]
    pub fn from_request(request: Request, uri: Uri) -> Self {
        let (parts, body) = request.into_parts();
        Self {
            body,
            extensions: parts.extensions,
            headers: parts.headers,
            method: parts.method,
            uri,
        }
    }

    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[inline]
    pub fn into_parts(self) -> (Method, Uri, HeaderMap, Body, Extensions) {
        (
            self.method,
            self.uri,
            self.headers,
            self.body,
            self.extensions,
        )
    }

    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    #[inline]
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            body: Body::empty(),
            extensions: Extensions::new(),
            headers: HeaderMap::new(),
            method,
            uri,
        }
    }

    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

pub struct ProxyResponse {
    body: Body,
    extensions: Extensions,
    headers: HeaderMap,
    status: StatusCode,
}

impl fmt::Debug for ProxyResponse {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl ProxyResponse {
    #[inline]
    pub fn body(&self) -> &Body {
        &self.body
    }

    #[inline]
    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    #[inline]
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    #[inline]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// # Errors
    /// Returns [`EdgeError::internal`] if the underlying `http::Response::builder()`
    /// rejects a header — should be unreachable since we only store names/values
    /// that were already validated, but propagation lets a faulty upstream stream
    /// fail the request instead of crashing the worker.
    #[inline]
    pub fn into_response(self) -> Result<Response, EdgeError> {
        let mut builder = response_builder().status(self.status);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder.body(self.body).map_err(EdgeError::internal)
    }

    #[inline]
    pub fn new(status: StatusCode, body: Body) -> Self {
        Self {
            body,
            extensions: Extensions::new(),
            headers: HeaderMap::new(),
            status,
        }
    }

    #[inline]
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

pub struct ProxyService<C> {
    client: C,
}

impl<C> ProxyService<C> {
    #[inline]
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

impl<C> ProxyService<C>
where
    C: ProxyClient,
{
    /// # Errors
    /// Returns [`EdgeError`] if the underlying [`ProxyClient`] fails or the
    /// response cannot be assembled.
    #[inline]
    pub async fn forward(&self, request: ProxyRequest) -> Result<Response, EdgeError> {
        let response = self.client.send(request).await?;
        response.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::header::HeaderName;
    use crate::http::{HeaderValue, Method, StatusCode, Uri, request_builder};
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures_util::{StreamExt as _, stream};

    struct EchoBodyClient;

    struct EchoHeadersClient;

    struct EchoMethodClient;

    struct ErrorClient;

    struct StreamingClient;

    struct TestClient;

    #[async_trait(?Send)]
    impl ProxyClient for EchoBodyClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let (_, _, _, body, _) = request.into_parts();
            Ok(ProxyResponse::new(StatusCode::OK, body))
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for EchoHeadersClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let mut resp = ProxyResponse::new(StatusCode::OK, Body::empty());
            // Echo back headers with x-echo- prefix
            for (name, value) in request.headers() {
                let echo_name = format!("x-echo-{}", name.as_str());
                if let Ok(header_name) = echo_name.parse::<HeaderName>() {
                    resp.headers_mut().insert(header_name, value.clone());
                }
            }
            Ok(resp)
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for EchoMethodClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let method_str = request.method().as_str();
            Ok(ProxyResponse::new(
                StatusCode::OK,
                Body::from(method_str.to_owned()),
            ))
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for ErrorClient {
        async fn send(&self, _request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            Err(EdgeError::bad_request("connection failed"))
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for StreamingClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let (_method, _uri, _headers, _body, _ext) = request.into_parts();
            let chunks = stream::iter(vec![
                Bytes::from_static(b"stream-one"),
                Bytes::from_static(b"stream-two"),
            ]);
            Ok(ProxyResponse::new(StatusCode::OK, Body::stream(chunks)))
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for TestClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let (method, uri, headers, _body, _) = request.into_parts();
            assert_eq!(method, Method::GET);
            assert_eq!(uri, Uri::from_static("https://example.com"));
            assert_eq!(
                headers.get("x-demo"),
                Some(&HeaderValue::from_static("true"))
            );

            let chunks = stream::iter(vec![
                Bytes::from_static(b"hello"),
                Bytes::from_static(b" world"),
            ]);
            Ok(ProxyResponse::new(StatusCode::OK, Body::stream(chunks)))
        }
    }

    fn collect_body(body: Body) -> Vec<u8> {
        match body {
            Body::Once(bytes) => bytes.to_vec(),
            Body::Stream(mut stream) => block_on(async {
                let mut data = Vec::new();
                while let Some(result) = stream.next().await {
                    let chunk = result.expect("chunk");
                    data.extend_from_slice(&chunk);
                }
                data
            }),
        }
    }

    #[test]
    fn proxy_forward_preserves_streaming_body() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/local-stream")
            .body(Body::empty())
            .expect("request");

        let target = Uri::from_static("https://example.com/stream");
        let proxy_request = ProxyRequest::from_request(request, target);
        let service = ProxyService::new(StreamingClient);
        let response = block_on(service.forward(proxy_request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let collected = collect_body(body);
        assert_eq!(collected, b"stream-onestream-two");
    }

    #[test]
    fn proxy_forward_roundtrips() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/local")
            .header("x-demo", "true")
            .body(Body::empty())
            .expect("request");

        let target = Uri::from_static("https://example.com");
        let proxy_request = ProxyRequest::from_request(request, target);
        let service = ProxyService::new(TestClient);
        let response = block_on(service.forward(proxy_request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn proxy_forwards_request_body() {
        let service = ProxyService::new(EchoBodyClient);
        let request = request_builder()
            .method(Method::POST)
            .uri("/test")
            .body(Body::from("request body content"))
            .expect("request");

        let proxy_req =
            ProxyRequest::from_request(request, Uri::from_static("https://example.com"));
        let response = block_on(service.forward(proxy_req)).expect("response");

        let body_bytes = collect_body(response.into_body());
        assert_eq!(body_bytes, b"request body content");
    }

    #[test]
    fn proxy_forwards_request_headers() {
        let service = ProxyService::new(EchoHeadersClient);
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .header("x-custom-header", "custom-value")
            .header("authorization", "Bearer token123")
            .body(Body::empty())
            .expect("request");

        let proxy_req =
            ProxyRequest::from_request(request, Uri::from_static("https://example.com"));
        let response = block_on(service.forward(proxy_req)).expect("response");

        assert_eq!(
            response
                .headers()
                .get("x-echo-x-custom-header")
                .and_then(|value| value.to_str().ok()),
            Some("custom-value")
        );
        assert_eq!(
            response
                .headers()
                .get("x-echo-authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer token123")
        );
    }

    #[test]
    fn proxy_forwards_various_methods() {
        let service = ProxyService::new(EchoMethodClient);

        for method in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::HEAD,
            Method::OPTIONS,
        ] {
            let req = ProxyRequest::new(method.clone(), Uri::from_static("https://example.com"));
            let response = block_on(service.forward(req)).expect("response");
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[test]
    fn proxy_handle_forward_returns_response() {
        let handle = ProxyHandle::with_client(TestClient);
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .header("x-demo", "true")
            .body(Body::empty())
            .expect("request");

        let proxy_req =
            ProxyRequest::from_request(request, Uri::from_static("https://example.com"));
        let response = block_on(handle.forward(proxy_req)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn proxy_handle_new_wraps_client() {
        let client = Arc::new(TestClient);
        let handle = ProxyHandle::new(client);
        assert!(Arc::strong_count(&handle.client()) >= 1);
    }

    #[test]
    fn proxy_handle_propagates_client_errors() {
        let handle = ProxyHandle::with_client(ErrorClient);
        let req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        block_on(handle.forward(req)).expect_err("ErrorClient propagates an error");
    }

    #[test]
    fn proxy_handle_with_client_creates_arc() {
        let handle = ProxyHandle::with_client(TestClient);
        assert!(Arc::strong_count(&handle.client()) >= 1);
    }

    #[test]
    fn proxy_request_body_mut_allows_modification() {
        let mut req = ProxyRequest::new(Method::POST, Uri::from_static("https://example.com"));
        *req.body_mut() = Body::from("new body content");
        assert!(matches!(
            req.body(),
            Body::Once(bytes) if bytes.as_ref() == b"new body content"
        ));
    }

    #[test]
    fn proxy_request_debug_format() {
        let mut req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        req.headers_mut()
            .insert("x-debug", HeaderValue::from_static("test"));
        let debug = format!("{req:?}");
        assert!(debug.contains("ProxyRequest"));
        assert!(debug.contains("GET"));
        assert!(debug.contains("example.com"));
    }

    #[test]
    fn proxy_request_extensions_mut_allows_modification() {
        let mut req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        req.extensions_mut().insert("custom-data".to_owned());
        assert_eq!(
            req.extensions().get::<String>(),
            Some(&"custom-data".to_owned())
        );
    }

    #[test]
    fn proxy_request_from_request_preserves_all_parts() {
        let request = request_builder()
            .method(Method::POST)
            .uri("/original")
            .header("x-custom", "value")
            .body(Body::from("request body"))
            .expect("request");

        let target = Uri::from_static("https://backend.example.com/api");
        let proxy_req = ProxyRequest::from_request(request, target.clone());

        assert_eq!(proxy_req.method(), &Method::POST);
        assert_eq!(proxy_req.uri(), &target);
        assert_eq!(
            proxy_req
                .headers()
                .get("x-custom")
                .and_then(|value| value.to_str().ok()),
            Some("value")
        );
    }

    #[test]
    fn proxy_request_headers_mut_allows_modification() {
        let mut req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        req.headers_mut()
            .insert("authorization", HeaderValue::from_static("Bearer token"));
        assert!(req.headers().get("authorization").is_some());
    }

    #[test]
    fn proxy_request_into_parts_destructures() {
        let mut req = ProxyRequest::new(
            Method::DELETE,
            Uri::from_static("https://example.com/resource"),
        );
        req.headers_mut()
            .insert("x-test", HeaderValue::from_static("value"));
        *req.body_mut() = Body::from("body");

        let (method, uri, headers, body, _extensions) = req.into_parts();
        assert_eq!(method, Method::DELETE);
        assert_eq!(uri, Uri::from_static("https://example.com/resource"));
        assert!(headers.get("x-test").is_some());
        assert!(matches!(
            &body,
            Body::Once(bytes) if bytes.as_ref() == b"body"
        ));
    }

    #[test]
    fn proxy_request_new_creates_empty_request() {
        let req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        assert_eq!(req.method(), &Method::GET);
        assert_eq!(req.uri(), &Uri::from_static("https://example.com"));
        assert!(req.headers().is_empty());
        assert!(matches!(req.body(), Body::Once(bytes) if bytes.is_empty()));
    }

    #[test]
    fn proxy_response_body_mut_allows_modification() {
        let mut resp = ProxyResponse::new(StatusCode::OK, Body::empty());
        *resp.body_mut() = Body::from("updated body");
        assert!(matches!(
            resp.body(),
            Body::Once(bytes) if bytes.as_ref() == b"updated body"
        ));
    }

    #[test]
    fn proxy_response_debug_format() {
        let resp = ProxyResponse::new(StatusCode::NOT_FOUND, Body::empty());
        let debug = format!("{resp:?}");
        assert!(debug.contains("ProxyResponse"));
        assert!(debug.contains("404"));
    }

    #[test]
    fn proxy_response_extensions_mut_allows_modification() {
        let mut resp = ProxyResponse::new(StatusCode::OK, Body::empty());
        resp.extensions_mut().insert(42_i32);
        assert_eq!(resp.extensions().get::<i32>(), Some(&42_i32));
    }

    #[test]
    fn proxy_response_headers_mut_allows_modification() {
        let mut resp = ProxyResponse::new(StatusCode::OK, Body::empty());
        resp.headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
        assert!(resp.headers().get("content-type").is_some());
    }

    #[test]
    fn proxy_response_into_response_converts() {
        let mut resp = ProxyResponse::new(StatusCode::CREATED, Body::from("created"));
        resp.headers_mut()
            .insert("x-custom", HeaderValue::from_static("header"));

        let http_resp = resp.into_response().expect("response");
        assert_eq!(http_resp.status(), StatusCode::CREATED);
        assert!(http_resp.headers().get("x-custom").is_some());
    }

    #[test]
    fn proxy_response_new_creates_response() {
        let resp = ProxyResponse::new(StatusCode::OK, Body::from("response body"));
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(matches!(
            resp.body(),
            Body::Once(bytes) if bytes.as_ref() == b"response body"
        ));
    }

    #[test]
    fn proxy_service_propagates_client_errors() {
        let service = ProxyService::new(ErrorClient);
        let req = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        let result = block_on(service.forward(req));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
