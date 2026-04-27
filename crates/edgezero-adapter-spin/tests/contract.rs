// Integration test target (`tests/contract.rs`) — clippy doesn't apply
// `allow-*-in-tests` to integration tests by default, so opt back in here.
#![allow(
    clippy::expect_used,
    clippy::tests_outside_test_module,
    reason = "integration test target — top-level test fns are correct here"
)]

use bytes::Bytes;
use edgezero_adapter_spin::context::SpinRequestContext;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, response_builder, Response, StatusCode};
use edgezero_core::router::RouterService;
use futures::executor::block_on;
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
        let bytes = ctx.request().body().as_bytes().expect("buffered").to_vec();
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

// ---------------------------------------------------------------------------
// Tests that run on the host (no WASI runtime required)
// ---------------------------------------------------------------------------

#[test]
fn context_default_is_empty() {
    let ctx = SpinRequestContext {
        client_addr: None,
        full_url: None,
    };
    assert!(ctx.client_addr.is_none());
    assert!(ctx.full_url.is_none());
}

#[test]
fn build_test_app_creates_valid_router() {
    // Smoke test: ensure the router builds without panicking and that
    // the test helpers are usable for future integration tests.
    let _app = build_test_app();
}

#[test]
fn router_dispatches_get_and_returns_response() {
    let app = build_test_app();
    let request = request_builder()
        .method("GET")
        .uri("http://example.com/uri")
        .body(Body::empty())
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered"),
        b"http://example.com/uri"
    );
}

#[test]
fn router_dispatches_post_with_body() {
    let app = build_test_app();
    let request = request_builder()
        .method("POST")
        .uri("http://example.com/mirror")
        .body(Body::from(b"echo-payload".to_vec()))
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.body().as_bytes().expect("buffered"),
        b"echo-payload"
    );
}

#[test]
fn router_dispatches_streaming_route() {
    let app = build_test_app();
    let request = request_builder()
        .method("GET")
        .uri("http://example.com/stream")
        .body(Body::empty())
        .expect("request");

    let response = block_on(app.router().oneshot(request)).expect("response");

    assert_eq!(response.status(), StatusCode::OK);

    let (_, body) = response.into_parts();
    let mut stream = body.into_stream().expect("should be a stream");
    let collected = block_on(async {
        use futures::StreamExt as _;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    });
    assert_eq!(collected, b"chunk-1chunk-2");
}

// ---------------------------------------------------------------------------
// Tests that require `spin_sdk` types (wasm32 + spin feature only)
//
// `from_core_response` returns `spin_sdk::http::Response` which is only
// available on wasm32.  `into_core_request` and `dispatch` additionally
// require a WASI `IncomingRequest` handle from the Spin runtime.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod wasm {
    use super::*;
    use edgezero_adapter_spin::response::from_core_response;

    #[test]
    fn from_core_response_translates_status_and_headers() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::CREATED)
                .header("x-edgezero-res", "1")
                .body(Body::from(b"hello".to_vec()))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 201);
            let header = spin_response
                .headers()
                .find(|(name, _)| *name == "x-edgezero-res");
            assert!(header.is_some());
        });
    }

    #[test]
    fn from_core_response_collects_streaming_body() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::OK)
                .body(Body::stream(stream::iter(vec![
                    Bytes::from_static(b"chunk-1"),
                    Bytes::from_static(b"chunk-2"),
                ])))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 200);
            assert_eq!(spin_response.into_body(), b"chunk-1chunk-2");
        });
    }

    #[test]
    fn from_core_response_handles_empty_body() {
        futures::executor::block_on(async {
            let response = response_builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::from(Vec::new()))
                .expect("response");

            let spin_response = from_core_response(response).await.expect("spin response");

            assert_eq!(*spin_response.status(), 204);
            assert!(spin_response.into_body().is_empty());
        });
    }
}
