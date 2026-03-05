#![cfg(all(feature = "spin", target_arch = "wasm32"))]

use edgezero_adapter_spin::{dispatch, from_core_response, into_core_request, SpinRequestContext};
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{response_builder, Method, Response, StatusCode};
use edgezero_core::router::RouterService;

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

    let router = RouterService::builder()
        .get("/uri", capture_uri)
        .post("/mirror", mirror_body)
        .build();

    App::new(router)
}
