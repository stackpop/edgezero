//! Integration coverage: `#[action]` composes the `State<T>` extractor with a
//! request-derived extractor (`Query<T>`) and runs end-to-end through the
//! router. Lives in `edgezero-macros/tests` because the `#[action]` macro
//! emits absolute `::edgezero_core::…` paths that only resolve when
//! `edgezero_core` is an external crate (as it is here, via the dev-dep).

#[cfg(test)]
mod tests {
    use edgezero_core::action;
    use edgezero_core::body::Body;
    use edgezero_core::error::EdgeError;
    use edgezero_core::extractor::{Query, State};
    use edgezero_core::http::{Method, StatusCode, request_builder};
    use edgezero_core::router::RouterService;
    use futures::executor::block_on;
    use serde::Deserialize;
    use std::sync::Arc;

    #[derive(Clone)]
    struct AppState {
        greeting: String,
    }

    #[derive(Deserialize)]
    struct Params {
        n: u32,
    }

    #[action]
    async fn handler(
        State(state): State<Arc<AppState>>,
        Query(params): Query<Params>,
    ) -> Result<String, EdgeError> {
        Ok(format!("{}:{}", state.greeting, params.n))
    }

    #[test]
    fn action_composes_state_and_query() {
        let service = RouterService::builder()
            .with_state(Arc::new(AppState {
                greeting: "hi".to_owned(),
            }))
            .get("/h", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/h?n=5")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"hi:5");
    }
}
