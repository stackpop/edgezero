use std::sync::Arc;

use crate::config_store::ConfigExtractionLimits;
use crate::error::EdgeError;
use crate::http::{Request, Response};
use crate::ingress::{
    AdmissionDecision, IngressAdmissionOutcome, IngressAdmissionPolicy, IngressBeginOutcome,
    IngressFraming, IngressHead, IngressHeadAccounting, IngressHeadLimits, IngressHeadParts,
    PreparedIngress, apply_admission_policy, default_admission_policy,
};
use crate::manifest::BakedManifest;
use crate::response::IntoResponse as _;
use crate::response_egress::{
    ResponseEgressEnvelope, ResponseEgressHead, ResponseEgressObserver,
    ResponseEgressObserverHandle, ResponseEgressPolicy, ResponseEgressPolicyCallback,
    default_response_egress_policy,
};
use crate::router::{RouteMetadata, RouteResolution, RouterService};
use crate::time::MonotonicInstant;

/// Canonical adapter name for the Axum adapter.
pub const AXUM_ADAPTER: &str = "axum";
/// Canonical adapter name for the Cloudflare adapter.
pub const CLOUDFLARE_ADAPTER: &str = "cloudflare";
const DEFAULT_APP_NAME: &str = "EdgeZero App";
/// Canonical adapter name for the Fastly adapter.
pub const FASTLY_ADAPTER: &str = "fastly";
/// Canonical adapter name for the Spin adapter.
pub const SPIN_ADAPTER: &str = "spin";

/// Lightweight container around a `RouterService` that can be extended via hook implementations.
pub struct App {
    config_extraction_limits: ConfigExtractionLimits,
    ingress_head_limits: IngressHeadLimits,
    ingress_policy: IngressAdmissionPolicy,
    name: String,
    response_egress_observer: ResponseEgressObserverHandle,
    response_egress_policy: ResponseEgressPolicyCallback,
    router: RouterService,
}

impl App {
    /// Runs the configured body-blind admission policy exactly once.
    ///
    /// # Errors
    /// Returns an internal policy error if the resulting absolute deadline cannot be
    /// normalized safely.
    #[inline]
    pub fn admit_ingress(&self, head: &IngressHead) -> Result<IngressAdmissionOutcome, EdgeError> {
        match apply_admission_policy(&self.ingress_policy, head)? {
            IngressAdmissionOutcome::Admitted(admitted) => Ok(IngressAdmissionOutcome::Admitted(
                admitted.with_config_extraction_limits(self.config_extraction_limits),
            )),
            IngressAdmissionOutcome::Refused(response) => {
                Ok(IngressAdmissionOutcome::Refused(response))
            }
        }
    }

    /// Resolves and admits a normalized request head before an adapter transfers native body
    /// ownership into a core [`Request`].
    ///
    /// # Errors
    /// Returns an internal policy error if the admission deadline cannot be normalized safely.
    #[inline]
    pub fn begin_ingress(
        &self,
        head_parts: IngressHeadParts,
        request_start: MonotonicInstant,
    ) -> Result<IngressBeginOutcome, EdgeError> {
        let resolved = self
            .router
            .resolve(head_parts.method(), head_parts.target().path());
        let route = match resolved.resolution() {
            RouteResolution::Matched(route) => Some(route.clone()),
            RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound => None,
        };
        let head = head_parts.into_head(request_start, resolved.resolution().clone());

        match self.admit_ingress(&head)? {
            IngressAdmissionOutcome::Admitted(admitted) => Ok(IngressBeginOutcome::Admitted(
                PreparedIngress::new(resolved, admitted),
            )),
            IngressAdmissionOutcome::Refused(response) => Ok(IngressBeginOutcome::Refused(
                self.response_egress_envelope(response, request_start, route),
            )),
        }
    }

    #[must_use]
    #[inline]
    pub fn config_extraction_limits(&self) -> ConfigExtractionLimits {
        self.config_extraction_limits
    }

    /// Default name used when none is provided.
    #[must_use]
    #[inline]
    pub fn default_name() -> &'static str {
        DEFAULT_APP_NAME
    }

    /// Dispatches a request whose native body was wrapped after admission.
    ///
    /// # Errors
    /// Returns an error only when handler/routing error rendering fails.
    #[inline]
    pub async fn dispatch_admitted(
        &self,
        prepared: PreparedIngress,
        request: Request,
    ) -> Result<ResponseEgressEnvelope, EdgeError> {
        let request_start = prepared.request_start();
        let route = prepared.route_metadata().cloned();
        let (resolved, admitted) = prepared.into_parts();
        let response = match self
            .router
            .dispatch_resolved(resolved, request, admitted)
            .await
        {
            Ok(response) => response,
            Err(error) => error.into_response()?,
        };
        Ok(self.response_egress_envelope(response, request_start, route))
    }

    /// Resolves, admits, and dispatches one normalized inbound request.
    ///
    /// Adapters must call this before polling the request body. The route selected for admission
    /// is consumed during dispatch, so handlers cannot observe a different route identity.
    ///
    /// # Errors
    /// Returns an error only when admission or error rendering fails. Handler and routing errors
    /// are rendered with the same semantics as [`RouterService::oneshot`].
    #[inline]
    pub async fn dispatch_ingress(
        &self,
        request: Request,
        request_start: MonotonicInstant,
        head_accounting: IngressHeadAccounting,
        framing: IngressFraming,
    ) -> Result<Response, EdgeError> {
        let head_parts = IngressHeadParts::from_request(&request, head_accounting, framing);
        match self.begin_ingress(head_parts, request_start)? {
            IngressBeginOutcome::Admitted(prepared) => self
                .dispatch_admitted(prepared, request)
                .await
                .map(ResponseEgressEnvelope::into_response),
            IngressBeginOutcome::Refused(response) => Ok(response.into_response()),
        }
    }

    #[must_use]
    #[inline]
    pub fn ingress_head_limits(&self) -> IngressHeadLimits {
        self.ingress_head_limits
    }

    /// Consume the app and return the contained router service.
    #[must_use]
    #[inline]
    pub fn into_router(self) -> RouterService {
        self.router
    }

    /// Name assigned to the application.
    #[must_use]
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Create a new application wrapper from the supplied router service.
    #[must_use]
    #[inline]
    pub fn new(router: RouterService) -> Self {
        Self::with_name(router, DEFAULT_APP_NAME)
    }

    fn response_egress_envelope(
        &self,
        response: Response,
        request_start: MonotonicInstant,
        route: Option<RouteMetadata>,
    ) -> ResponseEgressEnvelope {
        ResponseEgressEnvelope::new(
            response,
            request_start,
            route,
            self.response_egress_policy(),
            self.response_egress_observer(),
        )
    }

    /// Returns an owned handle to the configured terminal response-egress observer.
    #[must_use]
    #[inline]
    pub fn response_egress_observer(&self) -> ResponseEgressObserverHandle {
        self.response_egress_observer.clone()
    }

    /// Returns an owned handle to the configured synchronous response-egress policy callback.
    #[must_use]
    #[inline]
    pub fn response_egress_policy(&self) -> ResponseEgressPolicyCallback {
        Arc::clone(&self.response_egress_policy)
    }

    /// Access the underlying router service.
    #[must_use]
    #[inline]
    pub fn router(&self) -> &RouterService {
        &self.router
    }

    /// Installs validated typed-config extraction limits.
    ///
    /// # Errors
    /// Returns an internal startup-policy error when limits are zero, inconsistent, or unbounded.
    #[inline]
    pub fn set_config_extraction_limits(
        &mut self,
        limits: ConfigExtractionLimits,
    ) -> Result<(), EdgeError> {
        self.config_extraction_limits = limits.validate()?;
        Ok(())
    }

    /// Installs the synchronous body-blind ingress admission callback.
    #[inline]
    pub fn set_ingress_admission_policy<Policy>(&mut self, policy: Policy)
    where
        Policy: Fn(&IngressHead) -> AdmissionDecision + Send + Sync + 'static,
    {
        self.ingress_policy = Arc::new(policy);
    }

    /// Installs already-validated finite request-head limits.
    #[inline]
    pub fn set_ingress_head_limits(&mut self, limits: IngressHeadLimits) {
        self.ingress_head_limits = limits;
    }

    /// Update the application name.
    #[inline]
    pub fn set_name<S>(&mut self, name: S)
    where
        S: Into<String>,
    {
        self.name = name.into();
    }

    /// Installs the terminal response-egress observer used by adapter conversion attempts.
    #[inline]
    pub fn set_response_egress_observer<Observer>(&mut self, observer: Observer)
    where
        Observer: ResponseEgressObserver,
    {
        self.response_egress_observer = ResponseEgressObserverHandle::new(observer);
    }

    /// Installs the synchronous body-blind response-egress policy callback.
    #[inline]
    pub fn set_response_egress_policy<Policy>(&mut self, policy: Policy)
    where
        Policy: for<'head> Fn(&ResponseEgressHead<'head>, MonotonicInstant) -> ResponseEgressPolicy
            + Send
            + Sync
            + 'static,
    {
        self.response_egress_policy = Arc::new(policy);
    }

    /// Construct a new application with the provided router and name.
    #[inline]
    pub fn with_name<S>(router: RouterService, name: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            config_extraction_limits: ConfigExtractionLimits::default(),
            ingress_head_limits: IngressHeadLimits::default(),
            ingress_policy: default_admission_policy(),
            name: name.into(),
            response_egress_observer: ResponseEgressObserverHandle::default(),
            response_egress_policy: Arc::new(default_response_egress_policy),
            router,
        }
    }
}

/// Compile-time metadata for one logical store kind, baked by the `app!` macro.
///
/// Carries only the portable facts declared in `[stores.<kind>]`: the logical
/// store ids and the resolved default. Platform names are resolved at runtime
/// from `EDGEZERO__STORES__*` environment variables.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreMetadata {
    /// Resolved default logical store id.
    pub default: &'static str,
    /// All declared logical store ids (non-empty).
    pub ids: &'static [&'static str],
}

/// Portable store config baked into the `App` by the `app!` macro.
///
/// A `Hooks` implementation built without the macro leaves every field `None`,
/// so a downstream binary compiles and runs with no `edgezero.toml` present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoresMetadata {
    /// `[stores.config]` declaration, if present.
    pub config: Option<StoreMetadata>,
    /// `[stores.kv]` declaration, if present.
    pub kv: Option<StoreMetadata>,
    /// `[stores.secrets]` declaration, if present.
    pub secrets: Option<StoreMetadata>,
}

/// Trait implemented by application hook adapters.
pub trait Hooks {
    /// Construct an `App` by wiring the routes and invoking the configuration hook.
    #[must_use]
    #[inline]
    fn build_app() -> App
    where
        Self: Sized,
    {
        let mut app = App::with_name(Self::routes(), Self::name());
        Self::configure(&mut app);
        app
    }

    /// Allow implementations to mutate the freshly constructed application before use.
    /// The default implementation performs no changes.
    #[inline]
    fn configure(_app: &mut App) {}

    /// Parsed and finalized manifest contract baked by `app!`.
    ///
    /// The default is deliberately uncached: every macro-generated application
    /// supplies its own per-implementation cache.
    #[must_use]
    #[inline]
    fn manifest() -> BakedManifest {
        BakedManifest::Absent
    }

    /// Raw manifest JSON baked at compile time by `app!`.
    #[must_use]
    #[inline]
    fn manifest_json() -> Option<&'static str> {
        None
    }

    /// Display name for the application. Defaults to `"EdgeZero App"`.
    #[must_use]
    #[inline]
    fn name() -> &'static str {
        App::default_name()
    }

    /// When `true`, an adapter's `run_app` skips its own logger initialization;
    /// the app is responsible for installing a `log` backend. Default `false`.
    #[must_use]
    #[inline]
    fn owns_logging() -> bool {
        false
    }

    /// Build the router service for the application.
    fn routes() -> RouterService;

    /// Portable store metadata for the application.
    ///
    /// Macro-generated apps derive this from `[stores.*]` in `edgezero.toml`.
    /// The default is empty, so an `App` built without the `app!` macro — and a
    /// downstream binary built without an `edgezero.toml` — still compiles.
    #[must_use]
    #[inline]
    fn stores() -> StoresMetadata {
        StoresMetadata::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use super::*;
    use crate::body::Body;
    use crate::config_store::ConfigExtractionLimits;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{HeaderMap, Method, StatusCode, Version, request_builder, response_builder};
    use crate::ingress::{
        IngressBeginOutcome, IngressFraming, IngressGrant, IngressHeadAccounting, IngressHeadParts,
    };
    use crate::manifest::BakedManifest;
    use crate::response_egress::{
        DEFAULT_RESPONSE_WRITE_BUDGET, ResponseEgressAttempt, ResponseEgressHead,
        ResponseEgressObserver, ResponseEgressOutcome, ResponseEgressPolicy, ResponseEgressReport,
    };
    use crate::router::{RouteMetadata, RouteResolution};
    use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicInstant};
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures::stream::poll_fn;
    use tower_service::Service as _;

    struct DefaultHooks;

    struct TestHooks;

    #[derive(Clone)]
    struct AppEgressObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    impl ResponseEgressObserver for AppEgressObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "test stub — only `routes` is overridden; every other Hooks method intentionally uses its trait default"
    )]
    impl Hooks for DefaultHooks {
        fn routes() -> RouterService {
            RouterService::builder().build()
        }

        fn stores() -> StoresMetadata {
            StoresMetadata::default()
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "test stub — `build_app` intentionally uses the trait default; other methods are overridden for test coverage"
    )]
    impl Hooks for TestHooks {
        fn configure(app: &mut App) {
            app.set_name("configured");
        }

        fn name() -> &'static str {
            "hooks-name"
        }

        fn routes() -> RouterService {
            async fn handler(_ctx: RequestContext) -> Result<String, EdgeError> {
                Ok("ok".to_owned())
            }

            RouterService::builder().get("/test", handler).build()
        }

        fn stores() -> StoresMetadata {
            StoresMetadata {
                config: Some(StoreMetadata {
                    default: "app_config",
                    ids: &["app_config"],
                }),
                kv: Some(StoreMetadata {
                    default: "sessions",
                    ids: &["sessions", "cache"],
                }),
                secrets: None,
            }
        }
    }

    fn empty_router() -> RouterService {
        RouterService::builder().build()
    }

    #[test]
    fn build_app_invokes_hooks_for_routes_and_configuration() {
        let app = TestHooks::build_app();
        assert_eq!(app.name(), "configured");
        let stores = TestHooks::stores();
        let config = stores.config.expect("config store metadata");
        assert_eq!(config.default, "app_config");
        assert_eq!(config.ids, &["app_config"]);
        let kv = stores.kv.expect("kv store metadata");
        assert_eq!(kv.default, "sessions");
        assert_eq!(kv.ids, &["sessions", "cache"]);
        assert!(stores.secrets.is_none());

        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");

        let response = block_on(app.router().clone().call(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"ok");
    }

    #[test]
    fn default_app_uses_constant_name() {
        let app = App::new(empty_router());
        assert_eq!(app.name(), App::default_name());
    }

    #[test]
    fn app_owns_default_and_configurable_response_egress_policy() {
        let mut app = App::new(empty_router());
        let headers = HeaderMap::new();
        let request_start = MonotonicInstant::now();
        let egress_started_at = request_start
            .checked_add(Duration::from_millis(5))
            .expect("egress start");
        let route = RouteMetadata::new(Method::GET, "/items/{id}");
        let head = ResponseEgressHead::new(
            StatusCode::OK,
            Version::HTTP_11,
            &headers,
            request_start,
            Some(&route),
        );

        let default_policy = app.response_egress_policy();
        assert_eq!(
            default_policy(&head, egress_started_at)
                .write_deadline
                .instant(),
            egress_started_at
                .checked_add(DEFAULT_RESPONSE_WRITE_BUDGET)
                .expect("default deadline")
        );

        app.set_response_egress_policy(move |response_head, started_at| {
            assert_eq!(response_head.status(), StatusCode::OK);
            assert_eq!(response_head.request_start(), request_start);
            assert_eq!(
                response_head.route().map(RouteMetadata::pattern),
                Some("/items/{id}")
            );
            assert_eq!(started_at, egress_started_at);
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(started_at),
            }
        });
        let configured_policy = app.response_egress_policy();
        assert_eq!(
            configured_policy(&head, egress_started_at)
                .write_deadline
                .instant(),
            egress_started_at
        );
    }

    #[test]
    fn app_owns_configurable_response_egress_observer() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new(empty_router());
        app.set_response_egress_observer(AppEgressObserver(Arc::clone(&reports)));

        let started_at = MonotonicInstant::now();
        let headers = HeaderMap::new();
        let head = ResponseEgressHead::new(
            StatusCode::NO_CONTENT,
            Version::HTTP_11,
            &headers,
            started_at,
            None,
        );
        let mut attempt =
            ResponseEgressAttempt::new(&head, started_at, app.response_egress_observer());
        assert!(attempt.begin_writing());
        assert!(attempt.complete(started_at));

        let observed_reports = reports.lock().expect("reports lock");
        assert_eq!(observed_reports.len(), 1);
        assert_eq!(
            observed_reports[0].outcome,
            ResponseEgressOutcome::Completed
        );
    }

    #[test]
    fn config_extraction_limits_are_validated_and_reach_routed_context() {
        let limits = ConfigExtractionLimits {
            max_backend_bytes: 64,
            max_blob_bytes: 32,
            max_secret_bytes: 16,
            max_total_bytes: 48,
            timeout: Duration::from_secs(2),
        };
        let router = RouterService::builder()
            .get("/limits", move |ctx: RequestContext| async move {
                assert_eq!(ctx.config_extraction_limits(), limits);
                Ok::<_, EdgeError>("ok")
            })
            .build();
        let mut app = App::new(router);
        app.set_config_extraction_limits(limits)
            .expect("valid extraction limits");

        let request = request_builder()
            .method(Method::GET)
            .uri("/limits")
            .body(Body::empty())
            .expect("request");
        let response = block_on(app.dispatch_ingress(
            request,
            MonotonicInstant::now(),
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        ))
        .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn default_app_admits_with_finite_deadline_and_empty_grant() {
        let app = App::new(empty_router());
        let start = MonotonicInstant::now();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let head = IngressHead::from_request(
            &request,
            start,
            RouteResolution::NotFound,
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        );

        let IngressAdmissionOutcome::Admitted(admitted) =
            app.admit_ingress(&head).expect("admitted")
        else {
            panic!("expected admission");
        };
        assert_eq!(admitted.request_start(), start);
        assert!(admitted.read_deadline().instant() > start);
        assert!(
            admitted.read_deadline().instant()
                <= start.checked_add(DEADLINE_FAR_FUTURE).expect("maximum")
        );
        let (_, _, grant, _) = admitted.into_parts();
        grant.downcast::<()>().expect_err("empty grant");
    }

    #[test]
    fn ingress_refusal_skips_handler_and_body_poll() {
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let handler_counter = Arc::clone(&handler_calls);
        let router = RouterService::builder()
            .post("/upload", move |_ctx: RequestContext| {
                let call_counter = Arc::clone(&handler_counter);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("unexpected")
                }
            })
            .build();
        let mut app = App::new(router);
        let admission_calls = Arc::new(AtomicUsize::new(0));
        let admission_counter = Arc::clone(&admission_calls);
        app.set_ingress_admission_policy(move |_| {
            admission_counter.fetch_add(1, Ordering::SeqCst);
            AdmissionDecision::Refuse(
                response_builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::empty())
                    .expect("response"),
            )
        });
        let body_polls = Arc::new(AtomicUsize::new(0));
        let poll_counter = Arc::clone(&body_polls);
        let body = Body::stream(poll_fn(move |_| {
            poll_counter.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(None::<Bytes>)
        }));
        let request = request_builder()
            .method(Method::POST)
            .uri("/upload")
            .body(body)
            .expect("request");

        let response = block_on(app.dispatch_ingress(
            request,
            MonotonicInstant::now(),
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        ))
        .expect("response");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(admission_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(body_polls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ingress_refusal_preserves_metadata_through_response_egress() {
        let router = RouterService::builder()
            .post("/upload/{id}", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("unexpected")
            })
            .build();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new(router);
        app.set_ingress_admission_policy(|_| {
            AdmissionDecision::Refuse(
                response_builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::empty())
                    .expect("response"),
            )
        });
        app.set_response_egress_observer(AppEgressObserver(Arc::clone(&reports)));
        let request_start = MonotonicInstant::now();
        app.set_response_egress_policy(move |head, egress_started_at| {
            assert_eq!(head.request_start(), request_start);
            assert_eq!(
                head.route().map(RouteMetadata::pattern),
                Some("/upload/{id}")
            );
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(
                    egress_started_at
                        .checked_add(Duration::from_secs(1))
                        .expect("deadline"),
                ),
            }
        });
        let head = IngressHeadParts::new(
            Method::POST,
            "/upload/42".parse().expect("URI"),
            Version::HTTP_11,
            HeaderMap::new(),
        );

        let IngressBeginOutcome::Refused(egress) =
            app.begin_ingress(head, request_start).expect("refusal")
        else {
            panic!("expected refusal");
        };
        let started_at = MonotonicInstant::now();
        let (response, _, mut attempt) = egress.begin(started_at).expect("begin egress");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(attempt.begin_writing());
        assert!(attempt.complete(started_at));

        let observed = reports.lock().expect("reports lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].request_start, request_start);
        assert_eq!(
            observed[0].route.as_ref().map(RouteMetadata::pattern),
            Some("/upload/{id}")
        );
    }

    #[test]
    fn ingress_two_phase_admission_precedes_body_construction() {
        let router = RouterService::builder()
            .post("/upload", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("accepted")
            })
            .build();
        let mut app = App::new(router);
        app.set_ingress_admission_policy(|head| {
            assert!(matches!(
                head.route_resolution(),
                RouteResolution::Matched(metadata) if metadata.pattern() == "/upload"
            ));
            AdmissionDecision::Admit {
                grant: IngressGrant::empty(),
                read_deadline: Deadline::after(Duration::from_secs(1)),
            }
        });
        let head = IngressHeadParts::new(
            Method::POST,
            "/upload".parse().expect("URI"),
            Version::HTTP_11,
            HeaderMap::new(),
        );
        let start = MonotonicInstant::now();

        let IngressBeginOutcome::Admitted(prepared) =
            app.begin_ingress(head, start).expect("begin ingress")
        else {
            panic!("expected admission");
        };
        assert_eq!(prepared.request_start(), start);

        let request = request_builder()
            .method(Method::POST)
            .uri("/upload")
            .body(Body::from("payload"))
            .expect("request");
        let response = block_on(app.dispatch_admitted(prepared, request)).expect("dispatch");
        assert_eq!(response.into_response().status(), StatusCode::OK);
    }

    #[test]
    fn default_hooks_do_not_own_logging() {
        assert!(!DefaultHooks::owns_logging());
    }

    #[test]
    fn default_hooks_use_default_name_and_into_router() {
        let app = DefaultHooks::build_app();
        assert_eq!(app.name(), App::default_name());
        assert!(matches!(DefaultHooks::manifest(), BakedManifest::Absent));
        assert_eq!(DefaultHooks::manifest_json(), None);
        assert_eq!(DefaultHooks::stores(), StoresMetadata::default());
        let router = app.into_router();
        assert!(router.routes().is_empty());
    }
}
