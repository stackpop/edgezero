//! Public API fixture: application types stay outside the generic collector.
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use edgezero_core::body::Body;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{Response, StatusCode, request_builder};
    use edgezero_core::middleware::RequestTimingMiddleware;
    use edgezero_core::request_timing::{RequestTimings, TimingError};
    use edgezero_core::response::response_with_body;
    use edgezero_core::router::RouterService;
    use futures::executor::block_on;

    // Deliberately neither Clone nor Sync: only Send is needed behind the mutex.
    #[derive(Default)]
    struct AppData {
        attempts: Cell<u32>,
        started: Option<Duration>,
    }

    type Handle = RequestTimings<1, AppData>;

    enum Phase {
        Fetch,
    }

    impl Phase {
        fn slot(self) -> usize {
            match self {
                Self::Fetch => 0,
            }
        }
    }

    struct AppTimings(Handle);

    impl AppTimings {
        fn from_context(ctx: &RequestContext) -> Option<Self> {
            ctx.request()
                .extensions()
                .get::<Handle>()
                .cloned()
                .map(Self)
        }

        fn record(&self, phase: Phase, duration: Duration) -> Result<(), TimingError> {
            self.0.record_with(phase.slot(), duration, |data, elapsed| {
                data.attempts.set(data.attempts.get().saturating_add(1));
                data.started.get_or_insert(elapsed);
            })
        }
    }

    #[edgezero_core::action]
    async fn timed_handler(ctx: RequestContext) -> Result<Response, EdgeError> {
        let timings = AppTimings::from_context(&ctx).expect("installed generic handle");
        timings
            .record(Phase::Fetch, Duration::from_millis(7))
            .expect("record");
        assert!(ctx.request().extensions().get::<AppTimings>().is_none());
        response_with_body(StatusCode::OK, Body::empty())
    }

    #[test]
    fn request_timing_public_facade_shares_extension_state_and_origin() {
        let handle = Handle::new();
        let before = handle.elapsed().expect("elapsed");
        let mut request = request_builder()
            .uri("/timed")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(handle.clone());
        let router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1, AppData>::default())
            .get("/timed", timed_handler)
            .build();
        let response = block_on(router.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        handle
            .snapshot(|view| {
                assert_eq!(view.phases, [Some(Duration::from_millis(7))]);
                assert_eq!(view.data.attempts.get(), 1);
                assert!(view.data.started.expect("application mark") >= before);
                assert!(view.elapsed >= view.data.started.expect("application mark"));
                assert_eq!(view.headers_ready_total, None);
                assert_eq!(view.request_elapsed, None);
            })
            .expect("snapshot");
        let fresh = request_builder()
            .uri("/timed")
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            block_on(router.oneshot(fresh)).expect("response").status(),
            StatusCode::OK
        );
        assert_eq!(
            handle
                .snapshot(|view| view.data.attempts.get())
                .expect("snapshot"),
            1
        );
    }
}
