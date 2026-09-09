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
use crate::router::RouterService;
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
        let head = head_parts.into_head(request_start, resolved.resolution().clone());

        match self.admit_ingress(&head)? {
            IngressAdmissionOutcome::Admitted(admitted) => Ok(IngressBeginOutcome::Admitted(
                PreparedIngress::new(resolved, admitted),
            )),
            IngressAdmissionOutcome::Refused(response) => {
                Ok(IngressBeginOutcome::Refused(response))
            }
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
    ) -> Result<Response, EdgeError> {
        let (resolved, admitted) = prepared.into_parts();
        match self
            .router
            .dispatch_resolved(resolved, request, admitted)
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => error.into_response(),
        }
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
            IngressBeginOutcome::Admitted(prepared) => {
                self.dispatch_admitted(prepared, request).await
            }
            IngressBeginOutcome::Refused(response) => Ok(response),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use super::*;
    use crate::body::Body;
    use crate::config_store::ConfigExtractionLimits;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{Method, StatusCode, request_builder, response_builder};
    use crate::ingress::{IngressFraming, IngressHeadAccounting};
    use crate::manifest::BakedManifest;
    use crate::router::RouteResolution;
    use crate::time::{DEADLINE_FAR_FUTURE, MonotonicInstant};
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures::stream::poll_fn;
    use tower_service::Service as _;

    struct DefaultHooks;

    struct TestHooks;

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
                grant: crate::ingress::IngressGrant::empty(),
                read_deadline: crate::time::Deadline::after(std::time::Duration::from_secs(1)),
            }
        });
        let head = crate::ingress::IngressHeadParts::new(
            Method::POST,
            "/upload".parse().expect("URI"),
            crate::http::Version::HTTP_11,
            crate::http::HeaderMap::new(),
        );
        let start = MonotonicInstant::now();

        let crate::ingress::IngressBeginOutcome::Admitted(prepared) =
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
        assert_eq!(response.status(), StatusCode::OK);
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
