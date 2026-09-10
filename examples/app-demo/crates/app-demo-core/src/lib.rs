pub mod config;
// `handlers` is `pub` so downstream integration tests
// can dispatch them directly against a wired `ConfigRegistry` /
// `KvRegistry` / `SecretRegistry` — the same fixture shape the
// runtime sets up. This avoids spinning a real HTTP server in
// tests that only need to verify the push → read-back → handler
// contract end to end. The `app!` macro still uses the handlers
// internally; pub visibility is purely additive.
pub mod handlers;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use edgezero_core::app::App as EdgeZeroApp;
use edgezero_core::{AdmissionDecision, IngressGrant, RouteResolution};

const DEFAULT_INGRESS_READ_BUDGET: Duration = Duration::from_secs(30);
const FALLBACK_INGRESS_BODY_BYTES: usize = 4 * 1024;
const FALLBACK_INGRESS_READ_BUDGET: Duration = Duration::from_secs(5);
const OUTBOUND_INGRESS_READ_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Eq, PartialEq)]
struct AdmissionLease {
    route_class: Option<String>,
}

/// App-owned shared state for the `app!(..., state = ...)` demonstration,
/// handed to handlers via `State<Arc<DemoState>>`.
#[derive(Debug)]
pub struct DemoState {
    /// A greeting the handler echoes, proving the value reached the handler.
    pub greeting: String,
}

/// Installs request-lifecycle policy before any adapter begins polling a body.
fn configure_app(app: &mut EdgeZeroApp) {
    app.set_ingress_admission_policy(|head| match head.route_resolution().clone() {
        RouteResolution::Matched(metadata) => {
            let route_class = metadata.class().map(str::to_owned);
            let read_budget = if route_class.as_deref() == Some("outbound") {
                OUTBOUND_INGRESS_READ_BUDGET
            } else {
                DEFAULT_INGRESS_READ_BUDGET
            };
            AdmissionDecision::Admit {
                grant: IngressGrant::new(AdmissionLease { route_class }),
                read_deadline: head.read_deadline_after(read_budget),
            }
        }
        RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound | _ => {
            AdmissionDecision::ReadBodyBeforeFallback {
                max_body_bytes: FALLBACK_INGRESS_BODY_BYTES,
                read_deadline: head.read_deadline_after(FALLBACK_INGRESS_READ_BUDGET),
            }
        }
    });
}

/// Returns the shared app state, referenced by `app!(..., state = crate::app_state())`.
///
/// IMPORTANT: `app!(state = <expr>)` emits this call inside the macro-generated
/// `build_router()`, which every adapter's `run_app` invokes via `A::build_app()`
/// — once at startup for long-lived runtimes (Axum), but **once per request** on
/// Fastly Compute (each request is a fresh Wasm instance). So `app_state()` must
/// be **cheap**: build the heavy state once and hand out clones. Here a
/// `OnceLock<Arc<DemoState>>` builds it lazily and every call just bumps the
/// `Arc` refcount — do NOT `Arc::new(..)` a heavy object on each call.
#[must_use]
#[inline]
pub fn app_state() -> Arc<DemoState> {
    static STATE: OnceLock<Arc<DemoState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| {
        Arc::new(DemoState {
            greeting: "hello from app state".to_owned(),
        })
    }))
}

edgezero_core::app!(
    "../../edgezero.toml",
    configure = crate::configure_app,
    state = crate::app_state()
);

#[cfg(test)]
mod lifecycle_tests {
    use bytes::Bytes;
    use edgezero_core::app::{App as EdgeZeroApp, Hooks as _};
    use edgezero_core::body::Body;
    use edgezero_core::http::{request_builder, HeaderMap, Method, StatusCode, Version};
    use edgezero_core::ingress::{IngressBeginOutcome, IngressHeadParts};
    use edgezero_core::router::RouteResolution;
    use edgezero_core::time::MonotonicInstant;
    use futures::executor::block_on;
    use futures::stream::iter;
    use std::time::Duration;

    #[test]
    fn manifest_route_classes_reach_route_resolution() {
        let resolved = crate::build_router().resolve(&Method::GET, "/proxy/status/200");
        let RouteResolution::Matched(metadata) = resolved.resolution().clone() else {
            panic!("proxy route must resolve");
        };
        assert_eq!(metadata.class(), Some("outbound"));
    }

    #[test]
    fn configured_admission_uses_finite_class_aware_deadlines() {
        let app = super::App::build_app();
        let start = MonotonicInstant::now();

        let outbound = begin_ingress(&app, "/proxy/status/200", start);
        assert_eq!(
            outbound.read_deadline().instant(),
            start
                .checked_add(Duration::from_secs(10))
                .expect("deadline")
        );

        let health = begin_ingress(&app, "/", start);
        assert_eq!(
            health.read_deadline().instant(),
            start
                .checked_add(Duration::from_secs(30))
                .expect("deadline")
        );

        let fallback = begin_ingress(&app, "/missing", start);
        assert_eq!(
            fallback.read_deadline().instant(),
            start.checked_add(Duration::from_secs(5)).expect("deadline")
        );
    }

    #[test]
    fn admission_handler_consumes_the_typed_grant_once() {
        let app = super::App::build_app();
        let start = MonotonicInstant::now();
        let prepared = begin_ingress(&app, "/admission", start);
        let request = request_builder()
            .method(Method::GET)
            .uri("/admission")
            .body(Body::empty())
            .expect("request");

        let response = block_on(app.dispatch_admitted(prepared, request))
            .expect("dispatch")
            .into_response();
        let payload: serde_json::Value = response.body().to_json().expect("json");
        assert_eq!(payload["route_class"], "diagnostic");
        assert_eq!(payload["grant_consumed_once"], true);
    }

    #[test]
    fn configured_admission_bounds_fallback_bodies() {
        let app = super::App::build_app();
        for (method, path, chunks, expected) in [
            (
                Method::POST,
                "/missing",
                vec![Bytes::from(vec![b'a'; 4_096])],
                StatusCode::NOT_FOUND,
            ),
            (
                Method::POST,
                "/",
                vec![Bytes::from(vec![b'a'; 4_096])],
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                Method::POST,
                "/missing",
                vec![Bytes::from(vec![b'a'; 4_096]), Bytes::from_static(b"b")],
                StatusCode::BAD_REQUEST,
            ),
            (
                Method::POST,
                "/",
                vec![Bytes::from(vec![b'a'; 4_096]), Bytes::from_static(b"b")],
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let request = request_builder()
                .method(method)
                .uri(path)
                .body(Body::stream(iter(chunks)))
                .expect("request");
            let response = block_on(app.dispatch_ingress(
                request,
                MonotonicInstant::now(),
                edgezero_core::IngressHeadAccounting::HostManaged,
                edgezero_core::IngressFraming::HostManaged,
            ))
            .expect("dispatch");
            assert_eq!(response.status(), expected);
        }
    }

    fn begin_ingress(
        app: &EdgeZeroApp,
        path: &str,
        start: MonotonicInstant,
    ) -> edgezero_core::PreparedIngress {
        let head = IngressHeadParts::new(
            Method::GET,
            path.parse().expect("URI"),
            Version::HTTP_11,
            HeaderMap::new(),
        );
        match app.begin_ingress(head, start).expect("begin ingress") {
            IngressBeginOutcome::Admitted(prepared) => prepared,
            IngressBeginOutcome::Refused(_) => panic!("demo policy must admit request"),
            _ => panic!("unknown admission outcome"),
        }
    }
}
