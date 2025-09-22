use std::fmt;

use async_trait::async_trait;

use crate::{
    response_builder, Body, EdgeError, Extensions, HeaderMap, Method, Request, Response,
    StatusCode, Uri,
};

/// Outbound request description for a proxy operation.
pub struct ProxyRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
    extensions: Extensions,
}

impl ProxyRequest {
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            body: Body::empty(),
            extensions: Extensions::new(),
        }
    }

    pub fn from_request(request: Request, uri: Uri) -> Self {
        let (parts, body) = request.into_parts();
        Self {
            method: parts.method,
            uri,
            headers: parts.headers,
            body,
            extensions: parts.extensions,
        }
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    pub fn into_parts(self) -> (Method, Uri, HeaderMap, Body, Extensions) {
        (
            self.method,
            self.uri,
            self.headers,
            self.body,
            self.extensions,
        )
    }
}

impl fmt::Debug for ProxyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyRequest")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("headers", &self.headers)
            .finish()
    }
}

pub struct ProxyResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
    extensions: Extensions,
}

impl ProxyResponse {
    pub fn new(status: StatusCode, body: Body) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body,
            extensions: Extensions::new(),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    pub fn into_response(self) -> Response {
        let mut builder = response_builder().status(self.status);
        for (name, value) in self.headers.iter() {
            builder = builder.header(name, value);
        }
        builder
            .body(self.body)
            .expect("proxy response builder should not fail")
    }
}

impl fmt::Debug for ProxyResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyResponse")
            .field("status", &self.status)
            .finish()
    }
}

#[async_trait(?Send)]
pub trait ProxyClient: Send + Sync {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError>;
}

pub struct ProxyService<C> {
    client: C,
}

impl<C> ProxyService<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

impl<C> ProxyService<C>
where
    C: ProxyClient,
{
    pub async fn forward(&self, request: ProxyRequest) -> Result<Response, EdgeError> {
        let response = self.client.send(request).await?;
        Ok(response.into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, HeaderValue, Method, StatusCode, Uri};
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures_util::{stream, StreamExt};

    struct TestClient;

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

    struct StreamingClient;

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

    #[test]
    fn proxy_forward_roundtrips() {
        let request = crate::request_builder()
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
    fn proxy_forward_preserves_streaming_body() {
        let request = crate::request_builder()
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

    fn collect_body(body: Body) -> Vec<u8> {
        match body {
            Body::Once(bytes) => bytes.to_vec(),
            Body::Stream(mut stream) => block_on(async {
                let mut data = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.expect("chunk");
                    data.extend_from_slice(&chunk);
                }
                data
            }),
        }
    }
}
