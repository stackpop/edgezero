//! Integration coverage: `app!(..., owns_logging = true)` emits a `Hooks` impl
//! whose `owns_logging()` returns `true`. The manifest path resolves against
//! this crate's `CARGO_MANIFEST_DIR`, so the fixture is `tests/fixtures/...`.

// The macro emits `pub struct OwnedLoggingApp;`, a `Hooks` impl, and a free
// `build_router()` at this module scope.
edgezero_core::app!(
    "tests/fixtures/owns_logging.toml",
    OwnedLoggingApp,
    owns_logging = true
);

#[cfg(test)]
mod tests {
    use edgezero_core::app::Hooks as _;

    #[test]
    fn app_macro_emits_owns_logging_true() {
        assert!(super::OwnedLoggingApp::owns_logging());
    }
}

#[cfg(test)]
mod timing {
    use edgezero_core::body::Body;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Response, StatusCode, request_builder};
    use edgezero_core::middleware::RequestTimingMiddleware;
    use edgezero_core::request_timing::RequestTimings;
    use edgezero_core::response::response_with_body;
    use futures::executor::block_on;

    pub const TIMING: RequestTimingMiddleware<1> =
        RequestTimingMiddleware::new().with_excluded_paths(&["/health"]);

    edgezero_core::app!("tests/fixtures/request_timing.toml", TimingApp);

    #[edgezero_core::action]
    async fn presence(ctx: RequestContext) -> Result<Response, EdgeError> {
        let installed = ctx
            .request()
            .extensions()
            .get::<RequestTimings<1>>()
            .is_some();
        response_with_body(
            StatusCode::OK,
            Body::text(if installed { "yes" } else { "no" }),
        )
    }

    #[test]
    fn app_macro_registers_const_request_timing_middleware() {
        let router = build_router();
        for (uri, expected) in [("/timed", "yes"), ("/health?check=1", "no")] {
            let request = request_builder().uri(uri).body(Body::empty()).unwrap();
            let response = block_on(router.oneshot(request)).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.body().as_bytes().unwrap(), expected.as_bytes());
        }
    }
}
