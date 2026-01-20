use crate::body::Body;
use crate::error::EdgeError;
use crate::http::Request;
use crate::params::PathParams;
use crate::proxy::ProxyHandle;
use serde::de::DeserializeOwned;

/// Request context exposed to handlers and middleware.
pub struct RequestContext {
    request: Request,
    path_params: PathParams,
}

impl RequestContext {
    pub fn new(request: Request, params: PathParams) -> Self {
        Self {
            request,
            path_params: params,
        }
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn request_mut(&mut self) -> &mut Request {
        &mut self.request
    }

    pub fn into_request(self) -> Request {
        self.request
    }

    pub fn path_params(&self) -> &PathParams {
        &self.path_params
    }

    pub fn path<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        self.path_params
            .deserialize()
            .map_err(|err| EdgeError::bad_request(format!("invalid path parameters: {}", err)))
    }

    pub fn query<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        let query = self.request.uri().query().unwrap_or("");
        serde_urlencoded::from_str(query)
            .map_err(|err| EdgeError::bad_request(format!("invalid query string: {}", err)))
    }

    pub fn json<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        self.request
            .body()
            .to_json()
            .map_err(|err| EdgeError::bad_request(format!("invalid JSON payload: {}", err)))
    }

    pub fn body(&self) -> &Body {
        self.request.body()
    }

    pub fn form<T>(&self) -> Result<T, EdgeError>
    where
        T: DeserializeOwned,
    {
        match self.request.body() {
            Body::Once(bytes) => serde_urlencoded::from_bytes(bytes.as_ref())
                .map_err(|err| EdgeError::bad_request(format!("invalid form payload: {}", err))),
            Body::Stream(_) => Err(EdgeError::bad_request(
                "streaming bodies are not supported for form extraction",
            )),
        }
    }

    pub fn proxy_handle(&self) -> Option<ProxyHandle> {
        self.request.extensions().get::<ProxyHandle>().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{request_builder, HeaderValue, Method, StatusCode, Uri};
    use crate::params::PathParams;
    use crate::proxy::{ProxyClient, ProxyHandle, ProxyRequest, ProxyResponse};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    fn ctx(path: &str, body: Body, params: PathParams) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(body)
            .expect("request");
        RequestContext::new(request, params)
    }

    fn params(map: &[(&str, &str)]) -> PathParams {
        let inner = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>();
        PathParams::new(inner)
    }

    #[derive(Debug, PartialEq, Deserialize, Serialize)]
    struct PathData {
        id: String,
    }

    #[test]
    fn path_deserialises_successfully() {
        let ctx = ctx("/items/42", Body::empty(), params(&[("id", "42")]));
        let parsed: PathData = ctx.path().expect("path parameters");
        assert_eq!(parsed, PathData { id: "42".into() });
        let serialized = serde_json::to_string(&parsed).expect("serialize");
        assert!(serialized.contains("42"));
    }

    #[test]
    fn invalid_path_returns_bad_request() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct NumericPath {
            id: u32,
        }
        let debug = format!("{:?}", NumericPath { id: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items/foo", Body::empty(), params(&[("id", "foo")]));
        let err = ctx.path::<NumericPath>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid path parameters"));
    }

    #[test]
    fn query_deserialises_successfully() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: u8,
        }
        let ctx = ctx("/items?page=5", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed, Query { page: 5 });
    }

    #[test]
    fn query_defaults_to_empty_when_missing() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Query {
            page: Option<u8>,
        }
        let ctx = ctx("/items", Body::empty(), PathParams::default());
        let parsed: Query = ctx.query().expect("query");
        assert_eq!(parsed.page, None);
    }

    #[test]
    fn invalid_query_returns_bad_request() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct Query {
            page: u8,
        }
        let debug = format!("{:?}", Query { page: 0 });
        assert!(debug.contains('0'));
        let ctx = ctx("/items?page=foo", Body::empty(), PathParams::default());
        let err = ctx.query::<Query>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid query string"));
    }

    #[test]
    fn json_deserialises_from_body() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        struct Payload {
            name: String,
        }
        let body = Body::json(&Payload {
            name: "demo".into(),
        })
        .expect("json body");
        let ctx = ctx("/echo", body, PathParams::default());
        let parsed: Payload = ctx.json().expect("json payload");
        assert_eq!(
            parsed,
            Payload {
                name: "demo".into()
            }
        );
    }

    #[test]
    fn invalid_json_returns_bad_request() {
        let body = Body::from(&b"not json"[..]);
        let ctx = ctx("/echo", body, PathParams::default());
        let err = ctx.json::<serde_json::Value>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid JSON payload"));
    }

    #[test]
    fn form_deserialises_successfully() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct FormData {
            name: String,
        }
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: FormData = ctx.form().expect("form data");
        assert_eq!(
            parsed,
            FormData {
                name: "demo".into()
            }
        );
        let debug = format!("{:?}", parsed);
        assert!(debug.contains("demo"));
    }

    #[test]
    fn invalid_form_returns_bad_request() {
        #[allow(dead_code)]
        #[derive(Deserialize)]
        struct FormData {
            age: u8,
        }
        let body = Body::from("age=not-a-number");
        let ctx = ctx("/submit", body, PathParams::default());
        let err = ctx
            .form::<FormData>()
            .err()
            .expect("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.message().contains("invalid form payload"));
    }

    #[test]
    fn form_value_deserialises_successfully() {
        let body = Body::from("name=demo");
        let ctx = ctx("/submit", body, PathParams::default());
        let parsed: serde_json::Value = ctx.form().expect("form data");
        assert_eq!(parsed.get("name").and_then(|value| value.as_str()), Some("demo"));
    }

    #[test]
    fn form_streaming_body_not_supported() {
        let stream = stream::iter(vec![Ok::<Bytes, anyhow::Error>(Bytes::from("name=demo"))]);
        let body = Body::from_stream(stream);
        let ctx = ctx("/submit", body, PathParams::default());
        let err = ctx.form::<serde_json::Value>().expect_err("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err
            .message()
            .contains("streaming bodies are not supported for form extraction"));
    }

    struct DummyClient;

    #[async_trait(?Send)]
    impl ProxyClient for DummyClient {
        async fn send(&self, _request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            Ok(ProxyResponse::new(StatusCode::OK, Body::empty()))
        }
    }

    #[test]
    fn proxy_handle_is_retrieved_when_present() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/proxy")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ProxyHandle::with_client(DummyClient));

        let ctx = RequestContext::new(request, PathParams::default());
        assert!(ctx.proxy_handle().is_some());
    }

    #[test]
    fn request_context_accessors_return_expected_values() {
        let mut ctx = ctx("/items/123", Body::from("payload"), params(&[("id", "123")]));
        assert_eq!(ctx.request().uri().path(), "/items/123");
        ctx.request_mut()
            .headers_mut()
            .insert("x-test", HeaderValue::from_static("value"));
        assert_eq!(
            ctx.request()
                .headers()
                .get("x-test")
                .and_then(|v| v.to_str().ok()),
            Some("value")
        );
        assert_eq!(ctx.path_params().get("id"), Some("123"));
        assert_eq!(ctx.body().as_bytes(), b"payload");

        let request = ctx.into_request();
        assert_eq!(request.uri().path(), "/items/123");
    }

    #[test]
    fn proxy_handle_forwards_with_dummy_client() {
        let handle = ProxyHandle::with_client(DummyClient);
        let request = ProxyRequest::new(Method::GET, Uri::from_static("https://example.com"));
        let response = futures::executor::block_on(handle.forward(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
