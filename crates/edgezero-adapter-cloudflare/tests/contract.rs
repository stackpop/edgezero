#![cfg(all(feature = "cloudflare", target_arch = "wasm32"))]

use edgezero_adapter_cloudflare::{
    dispatch, from_core_response, into_core_request, CloudflareRequestContext,
};
use edgezero_core::{
    response_builder, App, Body, EdgeError, Method, RequestContext, RouterService, StatusCode,
};
use bytes::Bytes;
use futures::stream;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::*;
use worker::{
    Context, Env, Method as CfMethod, Request as CfRequest, RequestInit, Response as CfResponse,
};

wasm_bindgen_test_configure!(run_in_browser);

fn build_test_app() -> App {
    async fn capture_uri(ctx: RequestContext) -> Result<edgezero_core::Response, EdgeError> {
        let body = Body::text(ctx.request().uri().to_string());
        let response = response_builder()
            .status(StatusCode::OK)
            .body(body)
            .expect("response");
        Ok(response)
    }

    async fn mirror_body(ctx: RequestContext) -> Result<edgezero_core::Response, EdgeError> {
        let bytes = ctx.request().body().as_bytes().to_vec();
        let response = response_builder()
            .status(StatusCode::OK)
            .body(Body::from(bytes))
            .expect("response");
        Ok(response)
    }

    async fn stream_response(_ctx: RequestContext) -> Result<edgezero_core::Response, EdgeError> {
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

fn cf_request(method: CfMethod, path: &str, body: Option<&[u8]>) -> CfRequest {
    use js_sys::Uint8Array;

    let mut init = RequestInit::new();
    init.with_method(method);

    let headers = worker::Headers::new().expect("headers");
    headers.set("host", "example.com").expect("host header");
    headers.set("x-edgezero-test", "1").expect("custom header");
    init.with_headers(headers);

    if let Some(bytes) = body {
        let array = Uint8Array::from(bytes);
        init.with_body(Some(JsValue::from(array))); // Uint8Array -> JsValue
    }

    let url = format!("https://example.com{}", path);
    CfRequest::new_with_init(&url, &init).expect("cf request")
}

fn test_env_ctx() -> (Env, Context) {
    (Env::default(), Context::default())
}

#[wasm_bindgen_test]
async fn into_core_request_preserves_method_uri_headers_body_and_context() {
    let req = cf_request(CfMethod::Post, "/mirror?foo=bar", Some(b"payload"));
    let (env, ctx) = test_env_ctx();

    let core_request = into_core_request(req, env, ctx)
        .await
        .expect("core request");

    assert_eq!(core_request.method(), &Method::POST);
    assert_eq!(core_request.uri().path(), "/mirror");
    assert_eq!(core_request.uri().query(), Some("foo=bar"));

    let header = core_request
        .headers()
        .get("x-edgezero-test")
        .and_then(|value| value.to_str().ok());
    assert_eq!(header, Some("1"));

    assert_eq!(core_request.body().as_bytes(), b"payload");

    assert!(CloudflareRequestContext::get(&core_request).is_some());
}

#[wasm_bindgen_test]
async fn from_core_response_translates_status_headers_and_streaming_body() {
    let response = response_builder()
        .status(StatusCode::CREATED)
        .header("x-edgezero-res", "1")
        .body(Body::stream(stream::iter(vec![
            Bytes::from_static(b"hello"),
            Bytes::from_static(b" "),
            Bytes::from_static(b"world"),
        ])))
        .expect("response");

    let cf_response = from_core_response(response).expect("cf response");

    assert_eq!(cf_response.status_code(), StatusCode::CREATED.as_u16());
    let header = cf_response.headers().get("x-edgezero-res").unwrap();
    assert_eq!(header.as_deref(), Some("1"));

    let bytes = cf_response.bytes().await.expect("bytes");
    assert_eq!(bytes.as_slice(), b"hello world");
}

#[wasm_bindgen_test]
async fn dispatch_runs_router_and_returns_response() {
    let app = build_test_app();
    let req = cf_request(CfMethod::Get, "/uri", None);
    let (env, ctx) = test_env_ctx();

    let response = dispatch(&app, req, env, ctx).await.expect("cf response");

    assert_eq!(response.status_code(), StatusCode::OK.as_u16());
    let body = response.text().await.expect("text");
    assert_eq!(body.unwrap(), "https://example.com/uri");
}

#[wasm_bindgen_test]
async fn dispatch_streaming_route_preserves_chunks() {
    let app = build_test_app();
    let req = cf_request(CfMethod::Get, "/stream", None);
    let (env, ctx) = test_env_ctx();

    let response = dispatch(&app, req, env, ctx).await.expect("cf response");

    assert_eq!(response.status_code(), StatusCode::OK.as_u16());
    let bytes = response.bytes().await.expect("bytes");
    assert_eq!(bytes.as_slice(), b"chunk-1chunk-2");
}

#[wasm_bindgen_test]
async fn dispatch_passes_request_body_to_handlers() {
    let app = build_test_app();
    let req = cf_request(CfMethod::Post, "/mirror", Some(b"echo"));
    let (env, ctx) = test_env_ctx();

    let response = dispatch(&app, req, env, ctx).await.expect("cf response");

    assert_eq!(response.status_code(), StatusCode::OK.as_u16());
    let bytes = response.bytes().await.expect("bytes");
    assert_eq!(bytes.as_slice(), b"echo");
}
