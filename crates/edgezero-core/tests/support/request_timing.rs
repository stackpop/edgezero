//! Shared attachment scenario and separate local-runtime clock check.
//!
//! The advancement check is only for Viceroy, Wasmtime and browser harnesses.
//! Deployed Cloudflare CPU-only work may legitimately leave timers unchanged.

use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, StatusCode, request_builder};
use edgezero_core::middleware::RequestTimingMiddleware;
use edgezero_core::request_timing::RequestTimings;
use edgezero_core::response::response_with_body;
use edgezero_core::router::RouterService;
use futures::executor::block_on;
use std::time::Duration;

type Handle = RequestTimings<1, Option<Duration>>;

#[edgezero_core::action]
async fn timed(ctx: RequestContext) -> Result<Response, EdgeError> {
    let handle = ctx.request().extensions().get::<Handle>().unwrap();
    let span = handle.span(0).unwrap();
    handle
        .update_data(|data, elapsed| *data = Some(elapsed))
        .unwrap();
    drop(span);
    handle
        .snapshot(|view| {
            assert!(view.phases[0].is_some());
            assert!(view.elapsed >= view.data.unwrap());
            assert_eq!(view.headers_ready, None);
            assert_eq!(view.request_complete, None);
        })
        .unwrap();
    response_with_body(StatusCode::OK, Body::empty())
}

pub(crate) fn clock_handle_and_attachment_work_on_runtime() {
    let handle = Handle::new();
    let before = handle.elapsed();
    let mut request = request_builder().uri("/timed").body(Body::empty()).unwrap();
    request.extensions_mut().insert(handle.clone());
    let router = RouterService::builder()
        .middleware(RequestTimingMiddleware::<1, Option<Duration>>::default())
        .get("/timed", timed)
        .build();
    assert_eq!(
        block_on(router.oneshot(request)).unwrap().status(),
        StatusCode::OK
    );
    handle
        .snapshot(|view| {
            assert!(view.phases[0].is_some());
            assert!(view.data.unwrap() >= before);
            assert!(view.elapsed >= view.data.unwrap());
            assert_eq!(view.headers_ready, None);
            assert_eq!(view.request_complete, None);
        })
        .unwrap();
    handle.mark_headers_ready().unwrap();
    assert!(
        handle
            .snapshot(|view| view.headers_ready.unwrap() >= before)
            .unwrap()
    );

    // The normal attachment path must create a collector on this runtime too.
    let uninstrumented = request_builder().uri("/timed").body(Body::empty()).unwrap();
    assert!(uninstrumented.extensions().get::<Handle>().is_none());
    assert_eq!(
        block_on(router.oneshot(uninstrumented)).unwrap().status(),
        StatusCode::OK
    );
}

/// Tests known advancing local clocks, bounded by an independent wall clock
/// supplied by the harness and an iteration cap if that clock also stops.
#[cfg(target_arch = "wasm32")]
pub(crate) fn clock_advances_on_local_runtime(mut timed_out: impl FnMut() -> bool) {
    let handle = RequestTimings::<0>::new();
    let before = handle.elapsed();
    for _ in 0_u32..100_000_000_u32 {
        if handle.elapsed() > before {
            return;
        }
        if timed_out() {
            break;
        }
    }
    panic!("local runtime clock never advanced");
}
