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
mod configured_app {
    use edgezero_core::app::{App, Hooks as _};
    use edgezero_core::body::Body;
    use edgezero_core::http::{HeaderMap, Method, Response, StatusCode, Uri, Version};
    use edgezero_core::ingress::{AdmissionDecision, IngressBeginOutcome, IngressHeadParts};
    use edgezero_core::time::MonotonicInstant;

    fn configure_app(app: &mut App) {
        app.set_ingress_admission_policy(|_| {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            AdmissionDecision::Refuse(response)
        });
    }

    edgezero_core::app!(
        "tests/fixtures/owns_logging.toml",
        ConfiguredApp,
        configure = configure_app
    );

    #[test]
    fn app_macro_configure_callback_installs_ingress_policy() {
        let app = ConfiguredApp::build_app();
        let head = IngressHeadParts::new(
            Method::GET,
            Uri::from_static("/"),
            Version::HTTP_11,
            HeaderMap::new(),
        );
        let outcome = app
            .begin_ingress(head, MonotonicInstant::now())
            .expect("begin ingress");
        let IngressBeginOutcome::Refused(response) = outcome else {
            panic!("configured policy must refuse ingress");
        };
        assert_eq!(
            response.into_response().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
