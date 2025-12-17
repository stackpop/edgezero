#![cfg(all(feature = "fastly", target_arch = "wasm32"))]

use bytes::Bytes;
use edgezero_adapter_fastly::{
    dispatch, from_core_response, into_core_request, FastlyRequestContext,
};
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{response_builder, Method, Response, StatusCode};
use edgezero_core::router::RouterService;
use fastly::http::{Method as FastlyMethod, StatusCode as FastlyStatus};
use fastly::Request as FastlyRequest;
use futures::stream;

fn build_test_app() -> App {
    async fn capture_uri(ctx: RequestContext) -> Result<Response, EdgeError> {
        let body = Body::text(ctx.request().uri().to_string());
        let response = response_builder()
            .status(StatusCode::OK)
            .body(body)
            .expect("response");
        Ok(response)
    }

    async fn mirror_body(ctx: RequestContext) -> Result<Response, EdgeError> {
        let bytes = ctx.request().body().as_bytes().to_vec();
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::from(bytes))
            .expect("response");
        Ok(response)
    }

    async fn stream_response(_ctx: RequestContext) -> Result<Response, EdgeError> {
        let chunks = stream::iter(vec![
            Bytes::from_static(b"chunk-1"),
            Bytes::from_static(b"chunk-2"),
        ]);

        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::stream(chunks))
            .expect("response");
        Ok(response)
    }

    let router = RouterService::builder()
        .get("/uri", capture_uri)
        .post("/mirror", mirror_body)
        .get("/stream", stream_response)
        .build();

    App::new(router)
}

fn fastly_request(method: FastlyMethod, path: &str, body: Option<&[u8]>) -> FastlyRequest {
    let mut req = FastlyRequest::new(method, path);
    req.set_header("host", "example.com");
    req.set_header("x-edgezero-test", "1");
    if let Some(bytes) = body {
        req.set_body(bytes.to_vec());
    }
    req
}

#[test]
fn into_core_request_preserves_method_uri_headers_body_and_context() {
    let mut req = fastly_request(FastlyMethod::POST, "/mirror?foo=bar", Some(b"payload"));
    let expected_ip = req.get_client_ip_addr();

    let core_request = into_core_request(req).expect("core request");

    assert_eq!(core_request.method(), &Method::POST);
    assert_eq!(core_request.uri().path(), "/mirror");
    assert_eq!(core_request.uri().query(), Some("foo=bar"));

    let headers = core_request.headers();
    assert_eq!(
        headers
            .get("x-edgezero-test")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );

    assert_eq!(core_request.body().as_bytes(), b"payload");

    let context = FastlyRequestContext::get(&core_request).expect("context");
    assert_eq!(context.client_ip, expected_ip);
}

#[test]
fn from_core_response_translates_status_headers_and_streaming_body() {
    let response = response_builder()
        .status(StatusCode::CREATED)
        .header("x-edgezero-res", "1")
        .body(Body::stream(stream::iter(vec![
            Bytes::from_static(b"hello"),
            Bytes::from_static(b" "),
            Bytes::from_static(b"world"),
        ])))
        .expect("response");

    let mut fastly_response = from_core_response(response).expect("fastly response");

    assert_eq!(fastly_response.get_status(), FastlyStatus::CREATED);
    assert!(fastly_response.get_header("x-edgezero-res").is_some());
    assert_eq!(fastly_response.take_body_bytes(), b"hello world");
}

#[test]
fn dispatch_runs_router_and_returns_response() {
    let app = build_test_app();
    let req = fastly_request(FastlyMethod::GET, "/uri", None);

    let mut response = dispatch(&app, req).expect("fastly response");

    assert_eq!(response.get_status(), FastlyStatus::OK);
    assert_eq!(response.take_body_bytes(), b"http://example.com/uri");
}

#[test]
fn dispatch_streaming_route_preserves_chunks() {
    let app = build_test_app();
    let req = fastly_request(FastlyMethod::GET, "/stream", None);

    let mut response = dispatch(&app, req).expect("fastly response");

    assert_eq!(response.get_status(), FastlyStatus::OK);
    assert_eq!(response.take_body_bytes(), b"chunk-1chunk-2");
}

#[test]
fn dispatch_passes_request_body_to_handlers() {
    let app = build_test_app();
    let req = fastly_request(FastlyMethod::POST, "/mirror", Some(b"echo"));

    let mut response = dispatch(&app, req).expect("fastly response");

    assert_eq!(response.get_status(), FastlyStatus::OK);
    assert_eq!(response.take_body_bytes(), b"echo");
}
