//! Compile this same handler fixture for native and every supported WASM target.

#[path = "fixtures/production_handlers.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use edgezero_core::Body;
    use edgezero_core::http::{StatusCode, request_builder};
    use edgezero_core::probe;
    use edgezero_core::router::RouterService;
    use futures::executor::block_on;

    use super::fixture;

    #[test]
    fn shared_state_config_and_probe_actions_use_portable_routes() {
        let router = RouterService::builder()
            .get("/state", fixture::state)
            .get("/typed", fixture::typed)
            .get("/ready", probe::readiness)
            .with_state(Arc::new(fixture::HostState {
                greeting: "retained".to_owned(),
            }))
            .build();
        for (path, status) in [
            ("/state", StatusCode::OK),
            ("/ready", StatusCode::SERVICE_UNAVAILABLE),
            ("/typed", StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let request = request_builder()
                .uri(path)
                .body(Body::empty())
                .expect("request");
            let response = block_on(router.oneshot(request)).expect("response");
            assert_eq!(response.status(), status);
        }
    }
}
