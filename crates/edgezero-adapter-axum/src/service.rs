use std::future::Future;
use std::net::SocketAddr;
use std::panic::catch_unwind;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, Response};
use edgezero_core::app::{App, FrameworkErrorDetail};
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Method, RequestParts};
use edgezero_core::ingress::{
    IngressBeginOutcome, IngressDispatchOutcome, IngressFraming, IngressHeadAccounting,
    IngressHeadParts, PreparedIngress, validate_normalized_ingress_parts_with_summary,
};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::probe::{LifecyclePhase, LifecycleReader};
#[cfg(test)]
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry,
};
use edgezero_core::time::MonotonicInstant;
use hyper::body::Incoming;
use hyper::service::Service as HyperService;
use tokio::sync::watch;
#[cfg(test)]
use tower::Service as TowerService;

#[cfg(test)]
use crate::connection::{ConnectionExit, serve_http1};
use crate::context::AxumRequestContext;
#[cfg(test)]
use crate::diagnostics::NativeDiagnosticsHandle;
use crate::diagnostics::{
    ConnectionActivity, ConnectionGuard, IngressDisposition, NativeIngressMetadata, NativeSession,
    RequestGuard,
};
#[cfg(test)]
use crate::outbound::AxumOutboundClient;
use crate::proxy::{TrustedProxyPolicy, normalize, strip_forwarding_headers};
use crate::request::into_core_request_parts;
use crate::response::{AxumEgressBody, EgressConnection, prepare_egress_response};
use crate::run_options::JsonBodyLimit;

/// Private Hyper service error used only to close/reset an admission-aborted connection.
#[derive(Debug, thiserror::Error)]
#[error("ingress admission aborted")]
pub(crate) struct AxumIngressAbort;

/// Shared application state used to construct one private Hyper service per HTTP/1 connection.
#[derive(Clone)]
pub(crate) struct AxumServiceState {
    activity: Option<ConnectionActivity>,
    app: Arc<App>,
    config_registry: Option<ConfigRegistry>,
    config_store_handle: Option<ConfigStoreHandle>,
    json_body_limit: JsonBodyLimit,
    kv_handle: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    native: NativeSession,
    outbound_transport: Option<Arc<reqwest::Client>>,
    phase: Option<watch::Receiver<LifecyclePhase>>,
    secret_handle: Option<SecretHandle>,
    secret_registry: Option<SecretRegistry>,
    trusted_proxy_policy: TrustedProxyPolicy,
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "constructor and store wiring methods remain grouped ahead of request dispatch"
)]
impl AxumServiceState {
    /// Creates a service that preserves all policies configured on `App`.
    #[must_use]
    #[inline]
    #[cfg(test)]
    pub fn from_app(app: App) -> Self {
        Self::with_transport(
            app,
            AxumOutboundClient::try_transport().expect("test transport"),
            JsonBodyLimit::DEFAULT,
            NativeSession::new(NativeDiagnosticsHandle::new(), true).expect("test native identity"),
            TrustedProxyPolicy::no_trust(),
        )
    }

    pub(crate) fn with_transport(
        app: App,
        transport: Arc<reqwest::Client>,
        json_body_limit: JsonBodyLimit,
        native: NativeSession,
        trusted_proxy_policy: TrustedProxyPolicy,
    ) -> Self {
        Self {
            activity: None,
            app: Arc::new(app),
            config_registry: None,
            config_store_handle: None,
            json_body_limit,
            kv_handle: None,
            kv_registry: None,
            native,
            outbound_transport: Some(transport),
            phase: None,
            secret_handle: None,
            secret_registry: None,
            trusted_proxy_policy,
        }
    }

    #[must_use]
    #[inline]
    #[cfg(test)]
    pub fn new(router: RouterService) -> Self {
        Self::from_app(App::new(router))
    }

    /// Attach an id-keyed config-store registry to this service.
    #[must_use]
    #[inline]
    pub fn with_config_registry(mut self, registry: ConfigRegistry) -> Self {
        self.config_registry = Some(registry);
        self
    }

    /// Attach a shared config store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `ConfigRegistry` keyed under `"default"`. Handlers read it
    /// via `ctx.config_store_default()` or the `Config` extractor
    /// (the pre-rewrite `ctx.config_handle()` accessor is gone --
    /// see the runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use [`Self::with_config_registry`]
    /// directly.
    #[must_use]
    #[inline]
    #[cfg(test)]
    pub fn with_config_store_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config_store_handle = Some(handle);
        self
    }

    /// Attach a shared KV store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `KvRegistry` keyed under `"default"`. Handlers read it via
    /// `ctx.kv_store_default()` or the `Kv` extractor (the
    /// pre-rewrite `ctx.kv_handle()` accessor is gone -- see the
    /// runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use [`Self::with_kv_registry`]
    /// directly.
    #[must_use]
    #[inline]
    #[cfg(test)]
    pub fn with_kv_handle(mut self, handle: KvHandle) -> Self {
        self.kv_handle = Some(handle);
        self
    }

    /// Attach an id-keyed KV registry to this service.
    #[must_use]
    #[inline]
    pub fn with_kv_registry(mut self, registry: KvRegistry) -> Self {
        self.kv_registry = Some(registry);
        self
    }

    /// Attach a shared secret store to this service.
    ///
    /// Single-handle setter; the dispatcher synthesises a one-id
    /// `SecretRegistry` keyed under `"default"` (the handle is
    /// bound to the platform store name `"default"`). Handlers
    /// read it via `ctx.secret_store_default()` or the `Secrets`
    /// extractor (the pre-rewrite `ctx.secret_handle()` accessor
    /// is gone -- see the runtime-store-API hard-cutoff in
    /// docs/guide/manifest-store-migration.md). New code that
    /// declares multiple ids should use
    /// [`Self::with_secret_registry`] directly.
    #[must_use]
    #[inline]
    #[cfg(test)]
    pub fn with_secret_handle(mut self, handle: SecretHandle) -> Self {
        self.secret_handle = Some(handle);
        self
    }

    /// Attach an id-keyed secret-store registry to this service.
    #[must_use]
    #[inline]
    pub fn with_secret_registry(mut self, registry: SecretRegistry) -> Self {
        self.secret_registry = Some(registry);
        self
    }

    pub(crate) fn with_phase(mut self, phase: watch::Receiver<LifecyclePhase>) -> Self {
        self.phase = Some(phase);
        self
    }

    pub(crate) fn connection_activity(&self) -> (ConnectionActivity, ConnectionGuard) {
        self.native.connection()
    }

    pub(crate) fn for_connection(
        &self,
        remote_addr: SocketAddr,
        connection: EgressConnection,
    ) -> AxumConnectionService {
        AxumConnectionService {
            connection,
            remote_addr,
            state: self.clone(),
        }
    }

    fn resolved_store_registries(
        &self,
    ) -> (
        Option<ConfigRegistry>,
        Option<KvRegistry>,
        Option<SecretRegistry>,
    ) {
        // Legacy single-handle constructors synthesize only the conventional
        // `default` registry entry; request extensions contain registries only.
        let config_registry = self.config_registry.clone().or_else(|| {
            self.config_store_handle.clone().map(|handle| {
                ConfigRegistry::single_id(
                    "default".to_owned(),
                    ConfigStoreBinding {
                        handle,
                        default_key: "default".to_owned(),
                    },
                )
            })
        });
        let kv_registry = self.kv_registry.clone().or_else(|| {
            self.kv_handle
                .clone()
                .map(|handle| KvRegistry::single_id("default".to_owned(), handle))
        });
        let secret_registry = self.secret_registry.clone().or_else(|| {
            self.secret_handle.clone().map(|handle| {
                SecretRegistry::single_id(
                    "default".to_owned(),
                    BoundSecretStore::new(handle, "default".to_owned()),
                )
            })
        });
        (config_registry, kv_registry, secret_registry)
    }

    fn admission_open(&self) -> bool {
        self.phase
            .as_ref()
            .is_none_or(|phase| *phase.borrow() == LifecyclePhase::Ready)
    }

    async fn dispatch_prepared(
        &self,
        prepared: PreparedIngress,
        parts: RequestParts,
        native_body: AxumBody,
        connection: &EgressConnection,
        guard: RequestGuard,
    ) -> Response<AxumEgressBody> {
        let app = &self.app;
        let read_deadline = prepared.read_deadline();
        let monotonic_clock = prepared.monotonic_clock();
        let Some(outbound_transport) = self.outbound_transport.clone() else {
            let egress = app.admitted_error_egress(
                prepared,
                EdgeError::internal(anyhow::anyhow!(
                    "failed to initialize outbound HTTP transport"
                )),
            );
            return prepare_native_egress(egress, connection, guard);
        };
        let mut core_request = match into_core_request_parts(
            parts,
            native_body,
            Some((read_deadline, monotonic_clock)),
            Some(outbound_transport),
            self.json_body_limit,
        ) {
            Ok(converted) => converted,
            Err(error) => {
                let egress = app.admitted_error_egress(
                    prepared,
                    EdgeError::internal(anyhow::anyhow!(
                        "failed to convert inbound request: {error}"
                    )),
                );
                return prepare_native_egress(egress, connection, guard);
            }
        };
        if let Some(phase) = self.phase.clone() {
            core_request
                .extensions_mut()
                .insert(LifecycleReader::new(move || Some(*phase.borrow())));
        }
        let (config_registry, kv_registry, secret_registry) = self.resolved_store_registries();
        if let Some(registry) = config_registry {
            core_request.extensions_mut().insert(registry);
        }
        if let Some(registry) = kv_registry {
            core_request.extensions_mut().insert(registry);
        }
        if let Some(registry) = secret_registry {
            core_request.extensions_mut().insert(registry);
        }
        let egress = app
            .dispatch_admitted_with_framework_error_detail(
                prepared,
                core_request,
                FrameworkErrorDetail::CategoryOnly,
            )
            .await;
        prepare_native_egress(egress, connection, guard)
    }

    async fn dispatch_request(
        &self,
        mut request: Request<AxumBody>,
        connection: &EgressConnection,
        remote_addr: Option<SocketAddr>,
    ) -> Result<Response<AxumEgressBody>, AxumIngressAbort> {
        let request_start = self.app.monotonic_now();
        let mut guard = self.native.request(request.method(), request_start, self.app.monotonic_clock(), self.activity.clone())
            .map_err(|_error| {
                let _logged = catch_unwind(|| log::error!(target: "edgezero::native", "event=lifecycle reason=request_identity_exhausted"));
                AxumIngressAbort
            })?;
        // Identity and lifetime start before the gate, without admitting/body polling.
        if !self.admission_open() {
            guard.finish_ingress(IngressDisposition::Draining);
            return Err(AxumIngressAbort);
        }
        // Only the owning socket supplies native peer facts, including an absent peer.
        request.extensions_mut().remove::<AxumRequestContext>();
        if let Some(peer_addr) = remote_addr {
            request.extensions_mut().insert(ConnectInfo(peer_addr));
        } else {
            request.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
        }
        let app = Arc::clone(&self.app);
        let (mut parts, native_body) = request.into_parts();
        let request_method = parts.method.clone();
        let received =
            match validate_normalized_ingress_parts_with_summary(&parts, app.ingress_head_limits())
            {
                Ok(summary) => summary,
                Err(error) => {
                    return prepare_detached_ingress_error(
                        &app,
                        error,
                        request_method.clone(),
                        request_start,
                        connection,
                        guard,
                    );
                }
            };
        let checked = normalize(&self.trusted_proxy_policy, remote_addr, &parts);
        guard.set_forwarding_result(checked.forwarding_result());
        strip_forwarding_headers(&mut parts.headers);
        parts.extensions.insert(checked.effective_host().clone());
        parts.extensions.insert(NativeIngressMetadata::new(
            guard.request_id(),
            checked,
            received,
        ));
        let head_parts = IngressHeadParts::from_parts(
            &parts,
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        );
        let prepared = match app.begin_ingress(head_parts, request_start) {
            Ok(IngressBeginOutcome::Admitted(prepared)) => prepared,
            Ok(IngressBeginOutcome::Refused(response)) => {
                guard.refused();
                return Ok(prepare_native_egress(response, connection, guard));
            }
            Ok(IngressBeginOutcome::Aborted) => {
                guard.finish_ingress(IngressDisposition::Aborted);
                return Err(AxumIngressAbort);
            }
            Ok(_) => {
                return prepare_detached_ingress_error(
                    &app,
                    EdgeError::internal(anyhow::anyhow!("unsupported ingress admission outcome")),
                    request_method,
                    request_start,
                    connection,
                    guard,
                );
            }
            Err(error) => {
                return prepare_detached_ingress_error(
                    &app,
                    error,
                    request_method.clone(),
                    request_start,
                    connection,
                    guard,
                );
            }
        };
        Ok(self
            .dispatch_prepared(prepared, parts, native_body, connection, guard)
            .await)
    }
}

#[derive(Clone)]
pub(crate) struct AxumConnectionService {
    connection: EgressConnection,
    remote_addr: SocketAddr,
    state: AxumServiceState,
}

impl AxumConnectionService {
    pub(crate) fn with_activity(mut self, activity: ConnectionActivity) -> Self {
        self.state.activity = Some(activity);
        self
    }
}

impl HyperService<Request<Incoming>> for AxumConnectionService {
    type Error = AxumIngressAbort;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
    type Response = Response<AxumEgressBody>;

    #[inline]
    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let state = self.state.clone();
        let connection = self.connection.clone();
        let remote_addr = self.remote_addr;
        let converted_request = req.map(AxumBody::new);
        Box::pin(async move {
            state
                .dispatch_request(converted_request, &connection, Some(remote_addr))
                .await
        })
    }
}

#[cfg(test)]
impl TowerService<Request<AxumBody>> for AxumServiceState {
    type Error = AxumIngressAbort;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
    type Response = Response<AxumEgressBody>;

    fn call(&mut self, req: Request<AxumBody>) -> Self::Future {
        let state = self.clone();
        Box::pin(async move {
            state
                .dispatch_request(req, &EgressConnection::default(), None)
                .await
        })
    }

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

fn prepare_detached_ingress_error(
    app: &App,
    error: EdgeError,
    request_method: Method,
    request_start: MonotonicInstant,
    connection: &EgressConnection,
    mut guard: RequestGuard,
) -> Result<Response<AxumEgressBody>, AxumIngressAbort> {
    guard.set_ingress_error_kind(error.kind());
    if let IngressDispatchOutcome::Response(envelope) =
        app.detached_ingress_error_egress(error, request_method, request_start)
    {
        Ok(prepare_native_egress(*envelope, connection, guard))
    } else {
        guard.finish_ingress(IngressDisposition::Aborted);
        Err(AxumIngressAbort)
    }
}

fn prepare_native_egress(
    envelope: edgezero_core::ResponseEgressEnvelope,
    connection: &EgressConnection,
    mut guard: RequestGuard,
) -> Response<AxumEgressBody> {
    if let Some(failure) = envelope.failure_classification() {
        guard.set_failure(Some(failure));
    }
    let (completion, selected) = guard.completion();
    prepare_egress_response(
        envelope.with_completion(completion),
        connection,
        Some(&selected),
    )
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arbitrary_source_item_ordering,
        reason = "shared test collectors precede fixtures and behavior cases"
    )]

    use super::*;
    use crate::diagnostics::{
        NativeRequestFailure, NativeRequestObserver, NativeRequestOutcome, NativeRequestRecord,
    };
    use bytes::{Bytes, BytesMut};
    use edgezero_core::action;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{
        BoundedStoreRead, ConfigStore, ConfigStoreError, ConfigStoreHandle,
        finish_bounded_config_read,
    };
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::extractor::Json;
    use edgezero_core::http::{
        HeaderMap, HeaderValue, Response as CoreResponse, StatusCode, response_builder,
    };
    use edgezero_core::ingress::{
        AdmissionDecision, BufferedIngressResponse, IngressGrant, IngressHeadLimits,
    };
    use edgezero_core::key_value_store::KvStore;
    use edgezero_core::middleware::{Middleware, Next};
    use edgezero_core::outbound::OutboundRequest;
    use edgezero_core::response_egress::{
        DEFAULT_RESPONSE_WRITE_BUDGET, DetachedResponseEgressDecision, ResponseEgressCompletion,
        ResponseEgressFallbackDisposition, ResponseEgressObserver, ResponseEgressOutcome,
        ResponseEgressPolicy, ResponseEgressReport,
    };
    use edgezero_core::router::{RouteMetadata, RouteResolution};
    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures_util::stream::poll_fn;
    use http_body::Body as _;
    use std::future::poll_fn as poll_future;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{sleep, timeout};
    use tower::ServiceExt as _;

    async fn to_bytes(mut body: AxumEgressBody, limit: usize) -> Result<Bytes, anyhow::Error> {
        let mut buffered = BytesMut::new();
        loop {
            let next_frame = poll_future(|context| Pin::new(&mut body).poll_frame(context)).await;
            let Some(frame_result) = next_frame else {
                return Ok(buffered.freeze());
            };
            let response_frame = frame_result.map_err(anyhow::Error::new)?;
            let Ok(bytes) = response_frame.into_data() else {
                continue;
            };
            if buffered.len().saturating_add(bytes.len()) > limit {
                return Err(anyhow::anyhow!("response body exceeded test limit"));
            }
            buffered.extend_from_slice(&bytes);
        }
    }

    #[action]
    async fn json_limit_probe(Json(value): Json<String>) -> Result<String, EdgeError> {
        Ok(value.len().to_string())
    }

    #[tokio::test]
    async fn managed_json_overflow_drops_source_once_stops_polling_and_keeps_error_egress() {
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&polls);
        let guard = DropSignal(Arc::clone(&drops));
        let source = poll_fn(move |_| {
            let _retained = &guard;
            let index = observed.fetch_add(1, Ordering::SeqCst);
            let chunk = match index {
                0 => Bytes::from_static(b"\"ab"),
                1 => Bytes::from_static(b"cdef\""),
                _ => panic!("source polled after oversized chunk"),
            };
            Poll::Ready(Some(Ok::<_, io::Error>(chunk)))
        });
        let router = RouterService::builder()
            .post("/json", json_limit_probe)
            .build();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new(router);
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        app.set_error_response_renderer(|error| {
            assert_eq!(error.kind(), "payload_too_large");
            CoreResponse::new(Body::from("custom renderer"))
        });
        let mut service = AxumServiceState::from_app(app);
        service.json_body_limit = JsonBodyLimit::new(4).expect("cap");
        let request = Request::builder()
            .method("POST")
            .uri("/json")
            .header("content-type", "application/json")
            .body(AxumBody::from_stream(source))
            .expect("request");
        let response = service
            .dispatch_request(request, &EgressConnection::default(), None)
            .await
            .expect("dispatch");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.expect("egress"),
            "custom renderer"
        );
        let observed_reports = reports.lock().expect("reports");
        assert_eq!(observed_reports.len(), 1);
        assert_eq!(
            observed_reports[0].outcome,
            ResponseEgressOutcome::HostHandoff
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    struct FixedConfigStore(String);

    struct CountingMiddleware(Arc<AtomicUsize>);

    struct DropSignal(Arc<AtomicUsize>);

    #[derive(Clone)]
    struct RecordingEgressObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    impl ResponseEgressObserver for RecordingEgressObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Middleware for CountingMiddleware {
        async fn handle(
            &self,
            ctx: RequestContext,
            next: Next<'_>,
        ) -> Result<CoreResponse, EdgeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            next.run(ctx).await
        }
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for FixedConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.clone()))
        }

        async fn get_bounded(
            &self,
            key: &str,
            clock: &MonotonicClock,
            deadline: Deadline,
            max_backend_bytes: u64,
            max_value_bytes: u64,
        ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
            if deadline.is_expired_at(clock.now()) {
                return Err(ConfigStoreError::DeadlineExceeded);
            }
            finish_bounded_config_read(
                self.get(key).await,
                clock,
                deadline,
                max_backend_bytes,
                max_value_bytes,
            )
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_request_to_router() {
        let router = RouterService::builder()
            .get("/", |_ctx: RequestContext| async move {
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from("ok"))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router);

        let request = Request::builder().uri("/").body(AxumBody::empty()).unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_abort_skips_body_handler_middleware_and_egress() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_body_polls = Arc::clone(&body_polls);
        let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
            observed_body_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let observed_handler_calls = Arc::clone(&handler_calls);
        let middleware_calls = Arc::new(AtomicUsize::new(0));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let router = RouterService::builder()
            .post("/upload", move |_ctx: RequestContext| {
                let calls = Arc::clone(&observed_handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("unexpected")
                }
            })
            .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
            .build();
        let mut app = App::new(router);
        app.set_ingress_admission_policy(|_| AdmissionDecision::Abort);
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let request = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(native_body)
            .expect("request");

        let result = AxumServiceState::from_app(app).oneshot(request).await;

        assert!(result.is_err());
        assert_eq!(body_polls.load(Ordering::SeqCst), 0);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        assert!(reports.lock().expect("reports lock").is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admission_abort_closes_http1_without_response_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let mut app = App::new(RouterService::builder().build());
        app.set_ingress_admission_policy(|_| AdmissionDecision::Abort);
        let state = AxumServiceState::from_app(app);

        let server = async move {
            let (stream, remote_addr) = listener.accept().await.expect("accept client");
            let responses = EgressConnection::default();
            serve_http1(
                stream,
                state.for_connection(remote_addr, responses.clone()),
                responses,
            )
            .await
            .expect("intentional admission abort")
        };
        let client = async move {
            let mut stream = TcpStream::connect(address).await.expect("connect client");
            stream
                .write_all(
                    b"POST /missing HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\ndata",
                )
                .await
                .expect("write request");
            let mut response = Vec::new();
            let read = stream.read_to_end(&mut response).await;
            (read, response)
        };

        let (exit, (read, response)) = tokio::join!(server, client);
        assert_eq!(exit, ConnectionExit::AdmissionAborted);
        if let Err(error) = read {
            assert!(matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ));
        }
        assert!(response.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn standard_service_installs_the_exact_application_outbound_clock() {
        async fn elapsed(ctx: RequestContext) -> Result<String, EdgeError> {
            let client = ctx
                .http_client()
                .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("missing HTTP client")))?;
            let request = OutboundRequest::get("https://example.com/")?.stream_response();
            let results = client
                .send_all_until(vec![request], Deadline::after(Duration::from_secs(1)))
                .await?;
            Ok(results.slots[0]
                .as_ref()
                .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("unresolved HTTP slot")))?
                .elapsed
                .as_millis()
                .to_string())
        }

        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(7))
            .expect("completed instant");
        let observations = Arc::new(AtomicUsize::new(0));
        let clock_observations = Arc::clone(&observations);
        let router = RouterService::builder().get("/clock", elapsed).build();
        let mut app = App::new(router);
        app.set_monotonic_clock(MonotonicClock::new(move || {
            if clock_observations.fetch_add(1, Ordering::SeqCst) < 2 {
                start
            } else {
                completed
            }
        }));
        let request = Request::builder()
            .uri("/clock")
            .body(AxumBody::empty())
            .expect("request");

        let response = AxumServiceState::from_app(app)
            .oneshot(request)
            .await
            .expect("response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");

        assert_eq!(body, "7");
        assert!(observations.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_admission_refuses_before_native_body_poll() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&body_polls);
        let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
            observed_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let admission_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&admission_calls);
        let observed_during_admission = Arc::clone(&body_polls);

        let router = RouterService::builder()
            .post("/upload", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("handler must not run")
            })
            .build();
        let mut app = App::new(router);
        app.set_ingress_admission_policy(move |head| {
            observed_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(observed_during_admission.load(Ordering::SeqCst), 0);
            assert!(matches!(
                head.route_resolution(),
                RouteResolution::Matched(_)
            ));
            AdmissionDecision::Refuse {
                completion: ResponseEgressCompletion::empty(),
                response: response_builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::empty())
                    .expect("refusal"),
            }
        });
        let mut service = AxumServiceState::from_app(app);
        let request = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(native_body)
            .expect("request");

        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(admission_calls.load(Ordering::SeqCst), 1);
        assert_eq!(body_polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normalized_ingress_error_returns_typed_response_without_application_observation() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let completion_reports = Arc::new(Mutex::new(Vec::new()));
        let clock_samples = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let completion_calls = Arc::new(AtomicUsize::new(0));
        let observed_clock_samples = Arc::clone(&clock_samples);
        let now = MonotonicInstant::now();
        let mut app = App::new(RouterService::builder().build());
        app.set_monotonic_clock(MonotonicClock::new(move || {
            observed_clock_samples.fetch_add(1, Ordering::SeqCst);
            now
        }));
        app.set_ingress_head_limits(
            IngressHeadLimits::default()
                .with_max_request_target_bytes(4)
                .expect("target limit"),
        );
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let observed_factory_calls = Arc::clone(&factory_calls);
        let observed_completion_calls = Arc::clone(&completion_calls);
        let observed_completion_reports = Arc::clone(&completion_reports);
        app.set_detached_response_egress_decision_factory(move |head| {
            observed_factory_calls.fetch_add(1, Ordering::SeqCst);
            let terminal_calls = Arc::clone(&observed_completion_calls);
            let terminal_reports = Arc::clone(&observed_completion_reports);
            DetachedResponseEgressDecision::Send {
                completion: ResponseEgressCompletion::new(move |report| {
                    terminal_calls.fetch_add(1, Ordering::SeqCst);
                    terminal_reports
                        .lock()
                        .expect("completion reports lock")
                        .push(report.clone());
                }),
                deadline: head.write_deadline_after(Duration::ZERO),
            }
        });
        let request = Request::builder()
            .uri("/too-long")
            .body(AxumBody::empty())
            .expect("request");

        let response = AxumServiceState::from_app(app)
            .oneshot(request)
            .await
            .expect("typed response");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert_eq!(body, b"response write deadline exceeded".as_slice());
        assert!(reports.lock().expect("reports lock").is_empty());
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
        let completed = completion_reports.lock().expect("completion reports lock");
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].outcome,
            ResponseEgressOutcome::DeadlineExceeded
        );
        assert_eq!(
            completed[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
        assert!(clock_samples.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normalized_ingress_error_decision_abort_closes_without_response() {
        #[derive(Clone)]
        struct NativeRecords(Arc<Mutex<Vec<NativeRequestRecord>>>);
        impl NativeRequestObserver for NativeRecords {
            #[inline]
            fn observe(&self, record: &NativeRequestRecord) {
                self.0.lock().expect("native records").push(record.clone());
            }
        }
        let records = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = NativeDiagnosticsHandle::new()
            .with_request_observer(NativeRecords(Arc::clone(&records)));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observed_factory_calls = Arc::clone(&factory_calls);
        let mut app = App::new(RouterService::builder().build());
        app.set_ingress_head_limits(
            IngressHeadLimits::default()
                .with_max_request_target_bytes(4)
                .expect("target limit"),
        );
        app.set_detached_response_egress_decision_factory(move |_head| {
            observed_factory_calls.fetch_add(1, Ordering::SeqCst);
            DetachedResponseEgressDecision::Abort
        });
        let mut state = AxumServiceState::from_app(app);
        state.native = NativeSession::new(diagnostics.clone(), false).expect("test native session");

        let server = async move {
            let (stream, remote_addr) = listener.accept().await.expect("accept client");
            let responses = EgressConnection::default();
            serve_http1(
                stream,
                state.for_connection(remote_addr, responses.clone()),
                responses,
            )
            .await
            .expect("intentional detached error abort")
        };
        let client = async move {
            let mut stream = TcpStream::connect(address).await.expect("connect client");
            stream
                .write_all(b"GET /too-long HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .expect("write request");
            let mut response = Vec::new();
            let read = stream.read_to_end(&mut response).await;
            (read, response)
        };

        let (exit, (read, response)) = tokio::join!(server, client);
        assert_eq!(exit, ConnectionExit::AdmissionAborted);
        if let Err(error) = read {
            assert!(matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ));
        }
        assert!(response.is_empty());
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        let captured = records.lock().expect("native records");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].event, "request_ingress");
        assert_eq!(
            captured[0].outcome,
            NativeRequestOutcome::Ingress(IngressDisposition::Aborted)
        );
        assert_eq!(
            captured[0].failure,
            Some(NativeRequestFailure::PropagatedError {
                kind: "uri_too_long"
            })
        );
        assert!(captured[0].status.is_none());
        assert!(captured[0].bytes_written.is_none());
        assert!(captured[0].body_kind.is_none());
        assert_eq!(diagnostics.snapshot().active_requests, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_policy_error_decision_send_uses_detached_completion() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_body_polls = Arc::clone(&body_polls);
        let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
            observed_body_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let observed_handler_calls = Arc::clone(&handler_calls);
        let middleware_calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .get("/matched", move |_ctx: RequestContext| {
                let request_handler_calls = Arc::clone(&observed_handler_calls);
                async move {
                    request_handler_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("handler must not run")
                }
            })
            .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
            .build();
        let decision_completion_calls = Arc::new(AtomicUsize::new(0));
        let decision_resource_drops = Arc::new(AtomicUsize::new(0));
        let grant_drops = Arc::new(AtomicUsize::new(0));
        let mut app = App::new(router);
        let observed_decision_completion_calls = Arc::clone(&decision_completion_calls);
        let observed_decision_resource_drops = Arc::clone(&decision_resource_drops);
        let observed_grant_drops = Arc::clone(&grant_drops);
        app.set_ingress_admission_policy(move |head| {
            assert!(matches!(
                head.route_resolution(),
                RouteResolution::Matched(_)
            ));
            let decision_resource = DropSignal(Arc::clone(&observed_decision_resource_drops));
            let terminal_calls = Arc::clone(&observed_decision_completion_calls);
            AdmissionDecision::ReadBodyBeforeFallback {
                completion: ResponseEgressCompletion::new(move |_report| {
                    let _resource = decision_resource;
                    terminal_calls.fetch_add(1, Ordering::SeqCst);
                }),
                grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                max_body_bytes: 1,
                read_deadline: head.read_deadline_after(Duration::from_secs(1)),
                on_exceeded: BufferedIngressResponse::text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too large",
                ),
                on_timeout: BufferedIngressResponse::text(StatusCode::REQUEST_TIMEOUT, "timeout"),
            }
        });
        let reports = Arc::new(Mutex::new(Vec::new()));
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let completion_calls = Arc::new(AtomicUsize::new(0));
        let observed_factory_calls = Arc::clone(&factory_calls);
        let observed_completion_calls = Arc::clone(&completion_calls);
        app.set_detached_response_egress_decision_factory(move |head| {
            observed_factory_calls.fetch_add(1, Ordering::SeqCst);
            let terminal_calls = Arc::clone(&observed_completion_calls);
            DetachedResponseEgressDecision::Send {
                completion: ResponseEgressCompletion::new(move |_report| {
                    terminal_calls.fetch_add(1, Ordering::SeqCst);
                }),
                deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),
            }
        });
        let request = Request::builder()
            .uri("/matched")
            .body(native_body)
            .expect("request");

        let response = AxumServiceState::from_app(app)
            .oneshot(request)
            .await
            .expect("typed policy error response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let _body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");

        assert_eq!(body_polls.load(Ordering::SeqCst), 0);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        assert_eq!(decision_completion_calls.load(Ordering::SeqCst), 0);
        assert_eq!(decision_resource_drops.load(Ordering::SeqCst), 1);
        assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
        assert!(reports.lock().expect("reports lock").is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_policy_error_decision_abort_skips_response_and_application_work() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_body_polls = Arc::clone(&body_polls);
        let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
            observed_body_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let observed_handler_calls = Arc::clone(&handler_calls);
        let middleware_calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .get("/matched", move |_ctx: RequestContext| {
                let calls = Arc::clone(&observed_handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("handler must not run")
                }
            })
            .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
            .build();
        let decision_resource_drops = Arc::new(AtomicUsize::new(0));
        let grant_drops = Arc::new(AtomicUsize::new(0));
        let observed_decision_resource_drops = Arc::clone(&decision_resource_drops);
        let observed_grant_drops = Arc::clone(&grant_drops);
        let mut app = App::new(router);
        app.set_ingress_admission_policy(move |head| AdmissionDecision::ReadBodyBeforeFallback {
            completion: ResponseEgressCompletion::new({
                let resource = DropSignal(Arc::clone(&observed_decision_resource_drops));
                move |_report| drop(resource)
            }),
            grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
            max_body_bytes: 1,
            read_deadline: head.read_deadline_after(Duration::from_secs(1)),
            on_exceeded: BufferedIngressResponse::text(StatusCode::PAYLOAD_TOO_LARGE, "too large"),
            on_timeout: BufferedIngressResponse::text(StatusCode::REQUEST_TIMEOUT, "timeout"),
        });
        let reports = Arc::new(Mutex::new(Vec::new()));
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let observed_factory_calls = Arc::clone(&factory_calls);
        app.set_detached_response_egress_decision_factory(move |_head| {
            observed_factory_calls.fetch_add(1, Ordering::SeqCst);
            DetachedResponseEgressDecision::Abort
        });
        let request = Request::builder()
            .uri("/matched")
            .body(native_body)
            .expect("request");

        let outcome = AxumServiceState::from_app(app).oneshot(request).await;

        assert!(outcome.is_err());
        assert_eq!(body_polls.load(Ordering::SeqCst), 0);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
        assert_eq!(decision_resource_drops.load(Ordering::SeqCst), 1);
        assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert!(reports.lock().expect("reports lock").is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_admission_failure_uses_owned_error_egress() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let grant_drops = Arc::new(AtomicUsize::new(0));
        let observed_grant_drops = Arc::clone(&grant_drops);
        let router = RouterService::builder()
            .get("/owned", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("handler must not run")
            })
            .build();
        let mut app = App::new(router);
        app.set_ingress_admission_policy(move |head| AdmissionDecision::Admit {
            completion: ResponseEgressCompletion::empty(),
            grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
            read_deadline: head.read_deadline_after(Duration::from_secs(1)),
        });
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let mut service = AxumServiceState::from_app(app);
        service.outbound_transport = None;
        let request = Request::builder()
            .uri("/owned")
            .body(AxumBody::empty())
            .expect("request");

        let response = service
            .oneshot(request)
            .await
            .expect("owned error response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let _body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
        let observed = reports.lock().expect("reports lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].outcome, ResponseEgressOutcome::HostHandoff);
        assert_eq!(
            observed[0].route.as_ref().map(RouteMetadata::pattern),
            Some("/owned")
        );
    }

    fn guarded_fallback_app() -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let handler_call_count = Arc::new(AtomicUsize::new(0));
        let observed_handler_calls = Arc::clone(&handler_call_count);
        let middleware_calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .get("/known", move |_ctx: RequestContext| {
                let request_handler_calls = Arc::clone(&observed_handler_calls);
                async move {
                    request_handler_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("handler must not run")
                }
            })
            .middleware(CountingMiddleware(Arc::clone(&middleware_calls)))
            .build();
        (App::new(router), handler_call_count, middleware_calls)
    }

    fn fallback_body_app(
        max_body_bytes: usize,
        read_budget: Duration,
        grant_drop_count: &Arc<AtomicUsize>,
    ) -> (App, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
        let observed_grant_drops = Arc::clone(grant_drop_count);
        app.set_ingress_admission_policy(move |head| match head.route_resolution() {
            RouteResolution::Matched(_) => AdmissionDecision::Admit {
                completion: ResponseEgressCompletion::empty(),
                grant: IngressGrant::empty(),
                read_deadline: head.read_deadline_after(read_budget),
            },
            RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound | _ => {
                AdmissionDecision::ReadBodyBeforeFallback {
                    completion: ResponseEgressCompletion::empty(),
                    grant: IngressGrant::new(DropSignal(Arc::clone(&observed_grant_drops))),
                    max_body_bytes,
                    read_deadline: head.read_deadline_after(read_budget),
                    on_exceeded: buffered_terminal_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "exceeded",
                        b"configured overflow\0response",
                    ),
                    on_timeout: buffered_terminal_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        "timeout",
                        b"configured timeout\0response",
                    ),
                }
            }
        });
        (app, handler_calls, middleware_calls)
    }

    fn buffered_terminal_response(
        status: StatusCode,
        marker: &'static str,
        body: &'static [u8],
    ) -> BufferedIngressResponse {
        BufferedIngressResponse::new(
            status,
            terminal_response_headers(marker),
            Bytes::from_static(body),
        )
    }

    fn terminal_response_headers(marker: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-fallback-terminal", HeaderValue::from_static(marker));
        headers
    }

    fn tracked_lengthless_request(
        path: &str,
        body_chunks: Vec<Bytes>,
        grant_drops: &Arc<AtomicUsize>,
        source_drops: &Arc<AtomicUsize>,
        body_polls: &Arc<AtomicUsize>,
    ) -> Request<AxumBody> {
        let observed_grant_drops = Arc::clone(grant_drops);
        let source_drop = DropSignal(Arc::clone(source_drops));
        let observed_body_polls = Arc::clone(body_polls);
        let mut pending_chunks = body_chunks.into_iter();
        let stream = poll_fn(move |_cx| {
            let _keep_source_alive = &source_drop;
            observed_body_polls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
            Poll::Ready(pending_chunks.next().map(Ok::<Bytes, io::Error>))
        });
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .body(AxumBody::from_stream(stream))
            .expect("request");
        assert!(request.headers().get("content-length").is_none());
        request
    }

    fn assert_no_fallback_dispatch(handler_calls: &AtomicUsize, middleware_calls: &AtomicUsize) {
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
    }

    async fn assert_terminal_response(
        response: Response<AxumEgressBody>,
        status: StatusCode,
        marker: &'static str,
        body: &[u8],
    ) {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers(), &terminal_response_headers(marker));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("response body"),
            body
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_fallback_preserves_404_and_405_at_exact_cap() {
        for (path, expected) in [
            ("/missing", StatusCode::NOT_FOUND),
            ("/known", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_body_app(4, Duration::from_secs(1), &grant_drops);
            let request = tracked_lengthless_request(
                path,
                vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
                &grant_drops,
                &source_drops,
                &body_polls,
            );
            let response = AxumServiceState::from_app(app)
                .ready()
                .await
                .expect("ready")
                .call(request)
                .await
                .expect("response");
            assert_eq!(response.status(), expected);
            if expected == StatusCode::METHOD_NOT_ALLOWED {
                assert_eq!(
                    response.headers().get("allow").expect("Allow header"),
                    "GET"
                );
            }
            assert_eq!(body_polls.load(Ordering::SeqCst), 3);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_fallback_dispatch(&handler_calls, &middleware_calls);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_fallback_overflow_precedes_404_and_405() {
        for path in ["/missing", "/known"] {
            let grant_drops = Arc::new(AtomicUsize::new(0));
            let source_drops = Arc::new(AtomicUsize::new(0));
            let body_polls = Arc::new(AtomicUsize::new(0));
            let (app, handler_calls, middleware_calls) =
                fallback_body_app(4, Duration::from_secs(1), &grant_drops);
            let request = tracked_lengthless_request(
                path,
                vec![Bytes::from_static(b"abcd"), Bytes::from_static(b"e")],
                &grant_drops,
                &source_drops,
                &body_polls,
            );
            let response = AxumServiceState::from_app(app)
                .ready()
                .await
                .expect("ready")
                .call(request)
                .await
                .expect("response");
            assert_terminal_response(
                response,
                StatusCode::PAYLOAD_TOO_LARGE,
                "exceeded",
                b"configured overflow\0response",
            )
            .await;
            assert_eq!(body_polls.load(Ordering::SeqCst), 2);
            assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_fallback_dispatch(&handler_calls, &middleware_calls);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_fallback_deadline_interrupts_pending_native_body() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&body_polls);
        let grant_drops = Arc::new(AtomicUsize::new(0));
        let observed_grant_drops = Arc::clone(&grant_drops);
        let source_drops = Arc::new(AtomicUsize::new(0));
        let source_drop = DropSignal(Arc::clone(&source_drops));
        let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
            let _keep_source_alive = &source_drop;
            assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
            observed_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let (app, handler_calls, middleware_calls) =
            fallback_body_app(4_096, Duration::from_millis(500), &grant_drops);
        let mut service = AxumServiceState::from_app(app);
        let request = Request::builder()
            .method("POST")
            .uri("/missing")
            .body(native_body)
            .expect("request");

        service.ready().await.expect("ready");
        let response = timeout(Duration::from_secs(5), service.call(request))
            .await
            .expect("fallback deadline must preempt the pending native body")
            .expect("response");

        assert_terminal_response(
            response,
            StatusCode::GATEWAY_TIMEOUT,
            "timeout",
            b"configured timeout\0response",
        )
        .await;
        assert!(body_polls.load(Ordering::SeqCst) > 0);
        assert_eq!(source_drops.load(Ordering::SeqCst), 1);
        assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
        assert_no_fallback_dispatch(&handler_calls, &middleware_calls);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[expect(
        clippy::integer_division_remainder_used,
        reason = "tokio::select! expands to internal randomized branch selection arithmetic"
    )]
    async fn cancelled_service_releases_pending_fallback_drain_and_grant() {
        let body_polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&body_polls);
        let grant_drops = Arc::new(AtomicUsize::new(0));
        let observed_grant_drops = Arc::clone(&grant_drops);
        let source_drops = Arc::new(AtomicUsize::new(0));
        let source_drop = DropSignal(Arc::clone(&source_drops));
        let native_body = AxumBody::from_stream(poll_fn(move |_context| {
            let _keep_source_alive = &source_drop;
            assert_eq!(observed_grant_drops.load(Ordering::SeqCst), 0);
            observed_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        }));
        let (app, handler_calls, middleware_calls) =
            fallback_body_app(4_096, Duration::from_secs(5), &grant_drops);
        let mut service = AxumServiceState::from_app(app);
        let request = Request::builder()
            .method("POST")
            .uri("/missing")
            .body(native_body)
            .expect("request");

        service.ready().await.expect("ready");
        let mut response = Box::pin(service.call(request));
        tokio::select! {
            _result = &mut response => panic!("pending drain completed unexpectedly"),
            () = sleep(Duration::from_millis(25)) => {}
        }
        assert!(body_polls.load(Ordering::SeqCst) > 0);
        assert_eq!(grant_drops.load(Ordering::SeqCst), 0);
        assert_eq!(source_drops.load(Ordering::SeqCst), 0);
        drop(response);
        assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
        assert_eq!(source_drops.load(Ordering::SeqCst), 1);
        assert_no_fallback_dispatch(&handler_calls, &middleware_calls);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fallback_refusal_returns_503_without_polling_native_body() {
        for path in ["/missing", "/known"] {
            let body_polls = Arc::new(AtomicUsize::new(0));
            let observed_polls = Arc::clone(&body_polls);
            let source_drops = Arc::new(AtomicUsize::new(0));
            let source_drop = DropSignal(Arc::clone(&source_drops));
            let native_body = AxumBody::from_stream(poll_fn(move |_cx| {
                let _keep_source_alive = &source_drop;
                observed_polls.fetch_add(1, Ordering::SeqCst);
                Poll::<Option<Result<Bytes, io::Error>>>::Pending
            }));
            let (mut app, handler_calls, middleware_calls) = guarded_fallback_app();
            app.set_ingress_admission_policy(|head| {
                assert!(matches!(
                    head.route_resolution(),
                    RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound
                ));
                AdmissionDecision::Refuse {
                    completion: ResponseEgressCompletion::empty(),
                    response: response_builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(Body::from("fallback unavailable\n"))
                        .expect("refusal response"),
                }
            });
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .body(native_body)
                .expect("request");

            let response = AxumServiceState::from_app(app)
                .ready()
                .await
                .expect("ready")
                .call(request)
                .await
                .expect("response");

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("response body"),
                b"fallback unavailable\n".as_slice()
            );
            assert_eq!(body_polls.load(Ordering::SeqCst), 0);
            assert_eq!(source_drops.load(Ordering::SeqCst), 1);
            assert_no_fallback_dispatch(&handler_calls, &middleware_calls);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consumed_response_reports_one_host_handoff_with_accepted_bytes() {
        let router = RouterService::builder()
            .get("/observed", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("ok")
            })
            .build();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new(router);
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        let mut service = AxumServiceState::from_app(app);
        let request = Request::builder()
            .method("GET")
            .uri("/observed")
            .body(AxumBody::empty())
            .expect("request");

        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            b"ok".as_slice()
        );

        let observed = reports.lock().expect("reports lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].outcome, ResponseEgressOutcome::HostHandoff);
        assert_eq!(observed[0].bytes_written, 2);
        assert_eq!(
            observed[0].route.as_ref().map(RouteMetadata::pattern),
            Some("/observed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_clock_controls_response_write_deadline() {
        let router = RouterService::builder()
            .get("/deadline", |_ctx: RequestContext| async move {
                Ok::<_, EdgeError>("too late")
            })
            .build();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let start = MonotonicInstant::now();
        let observed_now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&observed_now);
        let policy_now = Arc::clone(&observed_now);
        let mut app = App::new(router);
        app.set_monotonic_clock(MonotonicClock::new(move || {
            *clock_now.lock().expect("clock lock")
        }));
        app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&reports)));
        app.set_response_egress_policy(move |_head, started_at| {
            let deadline = started_at
                .checked_add(Duration::from_secs(1))
                .expect("write deadline");
            *policy_now.lock().expect("clock lock") = deadline;
            ResponseEgressPolicy {
                write_deadline: Deadline::at_instant(deadline),
            }
        });
        let request = Request::builder()
            .method("GET")
            .uri("/deadline")
            .body(AxumBody::empty())
            .expect("request");

        let response = AxumServiceState::from_app(app)
            .ready()
            .await
            .expect("ready")
            .call(request)
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("fallback body");
        assert_eq!(body, b"response write deadline exceeded".as_slice());
        let observed = reports.lock().expect("reports lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(observed[0].elapsed, Duration::from_secs(1));
        assert_eq!(
            observed[0].fallback_disposition,
            Some(ResponseEgressFallbackDisposition::Completed)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admitted_deadline_reaches_body_cell_as_request_timeout() {
        let router = RouterService::builder()
            .post("/upload", |ctx: RequestContext| async move {
                let _bytes = ctx.body_bytes(1024).await?;
                Ok::<_, EdgeError>("unexpected success")
            })
            .build();
        let mut app = App::new(router);
        let request_start = MonotonicInstant::now();
        app.set_monotonic_clock(MonotonicClock::new(move || request_start));
        app.set_ingress_admission_policy(move |head| {
            assert_eq!(head.request_start(), request_start);
            AdmissionDecision::Admit {
                completion: ResponseEgressCompletion::empty(),
                grant: IngressGrant::empty(),
                read_deadline: Deadline::at_instant(head.request_start()),
            }
        });
        let mut service = AxumServiceState::from_app(app);
        let request = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(AxumBody::from("data"))
            .expect("request");

        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_config_store_handle_injects_into_request() {
        // Hard-cutoff: legacy `ctx.config_handle()` is
        // gone. The service synthesises a one-id `ConfigRegistry`
        // from the wired handle at the dispatch boundary, so
        // `ctx.config_store_default()` resolves the same store.
        let handle = ConfigStoreHandle::new(Arc::new(FixedConfigStore("injected".to_owned())));

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                let store = ctx
                    .config_store_default()
                    .expect("config store should be present");
                let val = store
                    .get("any_key")
                    .await
                    .expect("config lookup should succeed")
                    .unwrap_or_default();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router).with_config_store_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_handle_injects_into_request() {
        use crate::key_value_store::PersistentKvStore;

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store: Arc<dyn KvStore> = Arc::new(PersistentKvStore::new(db_path).unwrap());
        let handle = KvHandle::new(Arc::clone(&store));
        handle.put("test_key", &"injected").await.unwrap();

        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                // Hard-cutoff: see
                // `with_config_store_handle_injects_into_request`.
                let kv = ctx.kv_store_default().expect("kv handle should be present");
                let val: String = kv.get_or("test_key", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router).with_kv_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kv_registry_wins_over_bare_handle_when_both_wired() {
        // Documents the precedence rule baked into the dispatcher:
        // `self.kv_registry.clone().or_else(|| self.kv_handle.map(...single_id))`.
        // If a caller wires BOTH `.with_kv_registry(...)` and
        // `.with_kv_handle(...)`, the registry wins outright -- the
        // bare handle is NOT used as a fallback for ids the registry
        // doesn't define, and is NOT synthesised into a "default"
        // entry alongside the registry's ids.
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();
        let registry_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("registry.redb")).unwrap());
        let registry_handle = KvHandle::new(Arc::clone(&registry_store));
        registry_handle
            .put("marker", &"from_registry")
            .await
            .unwrap();

        let handle_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("handle.redb")).unwrap());
        let bare_handle = KvHandle::new(Arc::clone(&handle_store));
        bare_handle.put("marker", &"from_bare").await.unwrap();

        // Registry binds only `sessions` (NOT `default`). If the
        // dispatcher merged in the bare handle, `default` would
        // resolve to the bare-handle store; the test asserts it does
        // NOT.
        let by_id: BTreeMap<String, KvHandle> = [("sessions".to_owned(), registry_handle)]
            .into_iter()
            .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let router = RouterService::builder()
            .get("/probe", |ctx: RequestContext| async move {
                // Registry's id resolves to the registry's store.
                let named = ctx.kv_store("sessions").expect("registry binding");
                let from_named: String = named.get_or("marker", String::new()).await.unwrap();
                // Default ALSO resolves to the registry (registry's
                // own declared default), NOT the bare handle.
                let default = ctx.kv_store_default().expect("registry default");
                let from_default: String = default.get_or("marker", String::new()).await.unwrap();
                // The bare handle's synthesised `default` id is NOT
                // exposed -- registry wins outright.
                let bare_default_visible = ctx.kv_store("default").is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!(
                        "named={from_named} default={from_default} bare_default={bare_default_visible}"
                    )))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        // Wire BOTH: registry first, then a bare handle. The bare
        // handle would synthesise a "default" id under the legacy
        // path; the dispatcher's `or_else` precedence must skip it.
        let mut service = AxumServiceState::new(router)
            .with_kv_registry(registry)
            .with_kv_handle(bare_handle);

        let request = Request::builder()
            .uri("/probe")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &*body, b"named=from_registry default=from_registry bare_default=false",
            "registry must win: bare handle is neither merged in nor a fallback"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_handle_synthesises_one_id_registry_under_default() {
        // Verifies the one-id-registry contract for the setup API:
        // `with_kv_handle(h)` wraps `h` in a `KvRegistry` with the
        // logical id `"default"`. So in a handler:
        //   - `ctx.kv_store_default()` must resolve.
        //   - `ctx.kv_store("default")` must resolve to the same handle.
        //   - `ctx.kv_store("any-other-id")` must return None (the
        //     registry has only one id; named lookups for anything
        //     else are misses, not silent fallbacks).
        // This is the precedence guarantee that lets handlers use
        // the named-lookup path uniformly across adapters with one
        // or many declared stores.
        use crate::key_value_store::PersistentKvStore;

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let store: Arc<dyn KvStore> = Arc::new(PersistentKvStore::new(db_path).unwrap());
        let handle = KvHandle::new(Arc::clone(&store));
        handle.put("k", &"v").await.unwrap();

        let router = RouterService::builder()
            .get("/probe", |ctx: RequestContext| async move {
                let by_default = ctx.kv_store_default().is_some();
                let by_default_name = ctx.kv_store("default").is_some();
                let unknown = ctx.kv_store("custom-id").is_none();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!(
                        "default={by_default} named_default={by_default_name} unknown_is_none={unknown}"
                    )))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router).with_kv_handle(handle);

        let request = Request::builder()
            .uri("/probe")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &*body, b"default=true named_default=true unknown_is_none=true",
            "synthesised one-id registry: default + named-`default` resolve; unknown id misses"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_config_store_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-config", |ctx: RequestContext| async move {
                // Hard-cutoff: with no handle and no
                // registry wired, the registry-aware accessor
                // returns None — same observable result as the
                // legacy `config_handle().is_some()` check.
                let has_config = ctx.config_store_default().is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("has_config={has_config}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router);

        let request = Request::builder()
            .uri("/no-config")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"has_config=false");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_secret_handle_injects_into_request() {
        use bytes::Bytes;
        use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
        use std::sync::Arc;

        // Hard-cutoff: the service synthesises a one-id
        // `SecretRegistry` from `with_secret_handle`, binding the
        // handle under the platform store name `"default"`. The
        // fixture keys mirror that bound name (`"default/<key>"`)
        // so the registry-aware lookup resolves.
        let handle = SecretHandle::new(Arc::new(InMemorySecretStore::new([(
            "default/__EDGEZERO_SERVICE_TEST_SECRET__",
            Bytes::from("injected_value"),
        )])));
        let router = RouterService::builder()
            .get("/check", |ctx: RequestContext| async move {
                // `BoundSecretStore::get_bytes(key)` is single-arg —
                // the platform store name is bound by the
                // dispatcher's synthesis.
                let secrets = ctx
                    .secret_store_default()
                    .expect("secret store should be present");
                let val = secrets
                    .get_bytes("__EDGEZERO_SERVICE_TEST_SECRET__")
                    .await
                    .unwrap()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(val))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router).with_secret_handle(handle);

        let request = Request::builder()
            .uri("/check")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"injected_value");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn service_without_kv_handle_still_works() {
        let router = RouterService::builder()
            .get("/no-kv", |ctx: RequestContext| async move {
                // Hard-cutoff: see
                // `service_without_config_store_handle_still_works`.
                let has_kv = ctx.kv_store_default().is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("has_kv={has_kv}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let mut service = AxumServiceState::new(router);

        let request = Request::builder()
            .uri("/no-kv")
            .body(AxumBody::empty())
            .unwrap();
        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&*body, b"has_kv=false");
    }

    /// Two-id KV registry: `ctx.kv_store("sessions")` and
    /// `ctx.kv_store("cache")` must each resolve to their own backing store.
    /// `ctx.kv_store_default()` must resolve to the registered default id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_kv_registry_resolves_named_and_default() {
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();

        let sessions_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("sessions.redb")).unwrap());
        let sessions_handle = KvHandle::new(Arc::clone(&sessions_store));
        sessions_handle
            .put("greeting", &"hello-from-sessions")
            .await
            .unwrap();

        let cache_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("cache.redb")).unwrap());
        let cache_handle = KvHandle::new(Arc::clone(&cache_store));
        cache_handle
            .put("greeting", &"hello-from-cache")
            .await
            .unwrap();

        let by_id: BTreeMap<String, KvHandle> = [
            ("sessions".to_owned(), sessions_handle),
            ("cache".to_owned(), cache_handle),
        ]
        .into_iter()
        .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());

        let router = RouterService::builder()
            .get("/named/{id}", |ctx: RequestContext| async move {
                let id = ctx
                    .path_params()
                    .get("id")
                    .map(ToOwned::to_owned)
                    .unwrap_or_default();
                let store = ctx
                    .kv_store(&id)
                    .ok_or_else(|| EdgeError::not_found(format!("kv id `{id}` not registered")))?;
                let value: String = store.get_or("greeting", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(value))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .get("/default", |ctx: RequestContext| async move {
                let store = ctx
                    .kv_store_default()
                    .expect("default kv store is registered");
                let value: String = store.get_or("greeting", String::new()).await.unwrap();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(value))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let service = AxumServiceState::new(router).with_kv_registry(registry);

        assert_eq!(
            body_at(&service, "/named/sessions").await,
            "hello-from-sessions"
        );
        assert_eq!(body_at(&service, "/named/cache").await, "hello-from-cache");
        assert_eq!(body_at(&service, "/default").await, "hello-from-sessions");
    }

    /// Unknown ids on a wired registry yield `None` — strict lookup, no
    /// fallback to the default. The handler returns 404 in that case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kv_registry_lookup_is_strict_for_unknown_ids() {
        use crate::key_value_store::PersistentKvStore;
        use edgezero_core::store_registry::{KvRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let temp_dir = tempfile::tempdir().unwrap();
        let only_store: Arc<dyn KvStore> =
            Arc::new(PersistentKvStore::new(temp_dir.path().join("only.redb")).unwrap());
        let only_handle = KvHandle::new(Arc::clone(&only_store));

        let by_id: BTreeMap<String, KvHandle> =
            [("only".to_owned(), only_handle)].into_iter().collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "only".to_owned());

        let router = RouterService::builder()
            .get("/lookup/{id}", |ctx: RequestContext| async move {
                let id = ctx
                    .path_params()
                    .get("id")
                    .map(ToOwned::to_owned)
                    .unwrap_or_default();
                let present = ctx.kv_store(&id).is_some();
                let response = response_builder()
                    .status(StatusCode::OK)
                    .body(Body::from(format!("present={present}")))
                    .expect("response");
                Ok::<_, EdgeError>(response)
            })
            .build();
        let service = AxumServiceState::new(router).with_kv_registry(registry);

        assert_eq!(body_at(&service, "/lookup/only").await, "present=true");
        assert_eq!(body_at(&service, "/lookup/missing").await, "present=false");
    }

    mod managed_native {
        use std::future::pending;
        use std::net::{IpAddr, SocketAddr};

        use http_body::Body as _;
        use serde_json::{Value, json};

        use crate::context::AxumRequestContext;
        use crate::diagnostics::{
            NativeDiagnosticsSnapshot, NativeRequestFailure, NativeRequestObserver,
            NativeRequestOutcome, NativeRequestRecord,
        };
        use crate::proxy::{ForwardingHeaderFamily, ForwardingResult, Scheme};
        use edgezero_core::extractor::ForwardedHost;
        use edgezero_core::http::{Authority, Uri};
        use edgezero_core::ingress::{CheckedEffectiveHost, CheckedHostSource, IngressHead};

        use super::*;

        #[derive(Clone, Default)]
        struct NativeProbe {
            handle: NativeDiagnosticsHandle,
            records: Arc<Mutex<Vec<NativeRequestRecord>>>,
            snapshots: Arc<Mutex<Vec<NativeDiagnosticsSnapshot>>>,
            events: Arc<Mutex<Vec<&'static str>>>,
            panics: bool,
        }

        impl NativeRequestObserver for NativeProbe {
            fn observe(&self, record: &NativeRequestRecord) {
                self.records
                    .lock()
                    .expect("native records")
                    .push(record.clone());
                self.snapshots
                    .lock()
                    .expect("native snapshots")
                    .push(self.handle.snapshot());
                self.events.lock().expect("hook events").push("native");
                assert!(!self.panics, "native observer fixture panic");
            }
        }

        struct OrderedAppObserver {
            events: Arc<Mutex<Vec<&'static str>>>,
            reports: Arc<Mutex<Vec<ResponseEgressReport>>>,
        }

        impl ResponseEgressObserver for OrderedAppObserver {
            fn complete(&self, report: &ResponseEgressReport) {
                self.events.lock().expect("hook events").push("observer");
                self.reports
                    .lock()
                    .expect("app reports")
                    .push(report.clone());
            }
        }

        #[derive(Clone)]
        struct PendingCalls(Arc<AtomicUsize>);

        struct MetadataMiddleware(Arc<Mutex<Option<Value>>>);

        #[async_trait::async_trait(?Send)]
        impl Middleware for MetadataMiddleware {
            async fn handle(
                &self,
                ctx: RequestContext,
                next: Next<'_>,
            ) -> Result<CoreResponse, EdgeError> {
                *self.0.lock().expect("middleware facts") = Some(context_facts(&ctx));
                next.run(ctx).await
            }
        }

        fn observed_service(
            app: App,
            policy: TrustedProxyPolicy,
            probe: &NativeProbe,
        ) -> AxumServiceState {
            let handle = probe.handle.clone().with_request_observer(probe.clone());
            AxumServiceState::with_transport(
                app,
                AxumOutboundClient::try_transport().expect("fixture transport"),
                JsonBodyLimit::DEFAULT,
                NativeSession::new(handle, false).expect("fixture identity"),
                policy,
            )
        }

        fn tracked_connection(service: &mut AxumServiceState) -> ConnectionGuard {
            let (activity, guard) = service.connection_activity();
            service.activity = Some(activity);
            guard
        }

        fn source_name(source: CheckedHostSource) -> &'static str {
            match source {
                CheckedHostSource::Direct => "direct",
                CheckedHostSource::TrustedForwarded => "trusted_forwarded",
                CheckedHostSource::TrustedXForwarded => "trusted_x_forwarded",
                CheckedHostSource::Unavailable => "unavailable",
                _ => "unknown",
            }
        }

        fn metadata_facts(
            metadata: &NativeIngressMetadata,
            headers: &HeaderMap,
            uri: &Uri,
        ) -> Value {
            let summary = metadata.received_normalized_head();
            json!({
                "request_id": metadata.request_id().to_string(),
                "peer": metadata.direct_peer().map(|peer| peer.to_string()),
                "client": metadata.effective_client().map(|client| client.to_string()),
                "host": metadata.effective_host().authority(),
                "scheme": metadata.effective_scheme().as_str(),
                "client_source": source_name(metadata.client_source()),
                "host_source": source_name(metadata.host_source()),
                "scheme_source": source_name(metadata.scheme_source()),
                "forwarding_result": metadata.forwarding_result().as_str(),
                "host_header": headers.get("host").and_then(|host| host.to_str().ok()),
                "uri": uri.to_string(),
                "raw_forwarding_absent": headers.keys().all(|name| name.as_str() != "forwarded" && !name.as_str().starts_with("x-forwarded-")),
                "normalized": {"target_bytes": summary.target_bytes(), "header_bytes": summary.header_bytes(), "header_count": summary.header_count()},
            })
        }

        fn context_facts(ctx: &RequestContext) -> Value {
            metadata_facts(
                ctx.extensions()
                    .get::<NativeIngressMetadata>()
                    .expect("managed metadata"),
                ctx.headers(),
                ctx.uri(),
            )
        }

        #[action]
        async fn metadata_handler(
            ctx: RequestContext,
            ForwardedHost(host): ForwardedHost,
        ) -> Result<CoreResponse, EdgeError> {
            let peer = ctx
                .extensions()
                .get::<AxumRequestContext>()
                .and_then(|native| native.remote_addr);
            Ok(CoreResponse::new(Body::from(
                json!({
                    "facts": context_facts(&ctx), "forwarded_host": host,
                    "direct_context_peer": peer.map(|address| address.to_string()),
                })
                .to_string(),
            )))
        }

        #[action]
        async fn mutate_metadata_handler(mut ctx: RequestContext) -> Result<String, EdgeError> {
            let original = ctx
                .extensions_mut()
                .remove::<NativeIngressMetadata>()
                .expect("original metadata");
            ctx.extensions_mut()
                .insert(CheckedEffectiveHost::from_ingress(
                    Some(
                        "application-mutated.example"
                            .parse::<Authority>()
                            .expect("authority"),
                    ),
                    CheckedHostSource::Direct,
                ));
            Ok(original.request_id().to_string())
        }

        #[action]
        async fn ordinary_status_handler(ctx: RequestContext) -> Result<CoreResponse, EdgeError> {
            let status = if ctx.uri().path().ends_with("/413") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            Ok(response_builder()
                .status(status)
                .body(Body::from("application status"))
                .expect("status response"))
        }

        #[action]
        async fn success_handler() -> Result<&'static str, EdgeError> {
            Ok("ok")
        }

        #[action]
        async fn internal_error_handler() -> Result<&'static str, EdgeError> {
            Err(EdgeError::internal(anyhow::anyhow!(
                "PRIVATE_SERVICE_ERROR_SENTINEL"
            )))
        }

        #[action]
        async fn upstream_error_handler() -> Result<&'static str, EdgeError> {
            Err(EdgeError::bad_gateway("PRIVATE_UPSTREAM_SENTINEL"))
        }

        #[action]
        async fn terminal_stream_handler(ctx: RequestContext) -> Result<CoreResponse, EdgeError> {
            let source_failure = ctx.uri().path().ends_with("/source");
            let mut sent_prefix = false;
            let stream = poll_fn(move |_context| {
                if !sent_prefix {
                    sent_prefix = true;
                    return Poll::Ready(Some(Ok::<_, io::Error>(Bytes::from_static(
                        b"terminal-prefix",
                    ))));
                }
                if source_failure {
                    Poll::Ready(Some(Err(io::Error::other("PRIVATE_SOURCE_SENTINEL"))))
                } else {
                    Poll::Pending
                }
            });
            Ok(CoreResponse::new(Body::from_external_stream(stream)))
        }

        #[action]
        async fn bad_framing_handler() -> Result<CoreResponse, EdgeError> {
            Ok(response_builder()
                .header("content-length", "1")
                .body(Body::from("mismatched"))
                .expect("framing response"))
        }

        #[action]
        async fn handler_not_found() -> Result<&'static str, EdgeError> {
            Err(EdgeError::not_found("APPLICATION_CHOSEN_SENTINEL"))
        }

        #[action]
        async fn read_body_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            ctx.extensions()
                .get::<PendingCalls>()
                .expect("handler calls")
                .0
                .fetch_add(1, Ordering::SeqCst);
            Ok(ctx.body_bytes(1024).await?.len().to_string())
        }

        #[action]
        async fn pending_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            ctx.extensions()
                .get::<PendingCalls>()
                .expect("handler calls")
                .0
                .fetch_add(1, Ordering::SeqCst);
            pending().await
        }

        fn assert_one_released(probe: &NativeProbe) -> NativeRequestRecord {
            let records = probe.records.lock().expect("native records");
            assert_eq!(records.len(), 1);
            assert_eq!(
                probe.snapshots.lock().expect("native snapshots")[0].active_requests,
                0
            );
            assert_eq!(probe.handle.snapshot().active_requests, 0);
            records[0].clone()
        }

        #[tokio::test]
        #[expect(
            clippy::too_many_lines,
            reason = "The seven-case trust matrix keeps forged inputs, one managed dispatch, and all consumer-witness assertions together so each boundary proof is visible"
        )]
        async fn owned_peer_checked_metadata_precedes_all_managed_consumers() {
            let trusted: SocketAddr = "192.0.2.2:4000".parse().expect("trusted peer");
            let untrusted: SocketAddr = "192.0.2.250:5000".parse().expect("untrusted peer");
            for (
                family,
                peer,
                enabled,
                forwarded,
                expected_host,
                expected_scheme,
                expected_client,
                expected_result,
                expected_source,
            ) in [
                (
                    ForwardingHeaderFamily::Forwarded,
                    Some(trusted),
                    false,
                    "for=203.0.113.7;host=public.example;proto=https",
                    "direct.example",
                    Scheme::Http,
                    Some(trusted.ip()),
                    ForwardingResult::UntrustedPeer,
                    CheckedHostSource::Direct,
                ),
                (
                    ForwardingHeaderFamily::Forwarded,
                    Some(trusted),
                    true,
                    "for=203.0.113.7;host=public.example;proto=https",
                    "public.example",
                    Scheme::Https,
                    Some("203.0.113.7".parse::<IpAddr>().expect("client")),
                    ForwardingResult::Accepted,
                    CheckedHostSource::TrustedForwarded,
                ),
                (
                    ForwardingHeaderFamily::Forwarded,
                    None,
                    true,
                    "for=203.0.113.7;host=public.example;proto=https",
                    "direct.example",
                    Scheme::Http,
                    None,
                    ForwardingResult::UntrustedPeer,
                    CheckedHostSource::Direct,
                ),
                (
                    ForwardingHeaderFamily::Forwarded,
                    Some(untrusted),
                    true,
                    "for=203.0.113.7;host=public.example;proto=https",
                    "direct.example",
                    Scheme::Http,
                    Some(untrusted.ip()),
                    ForwardingResult::UntrustedPeer,
                    CheckedHostSource::Direct,
                ),
                (
                    ForwardingHeaderFamily::XForwarded,
                    Some(trusted),
                    true,
                    "for=203.0.113.7;host=public.example;proto=https",
                    "x-public.example",
                    Scheme::Https,
                    Some("203.0.113.9".parse::<IpAddr>().expect("client")),
                    ForwardingResult::Accepted,
                    CheckedHostSource::TrustedXForwarded,
                ),
                (
                    ForwardingHeaderFamily::Forwarded,
                    Some(trusted),
                    true,
                    "for=bad-address;host=public.example;proto=https",
                    "direct.example",
                    Scheme::Http,
                    Some(trusted.ip()),
                    ForwardingResult::Malformed,
                    CheckedHostSource::Direct,
                ),
                (
                    ForwardingHeaderFamily::Forwarded,
                    Some(trusted),
                    true,
                    "for=unknown;host=public.example;proto=https",
                    "public.example",
                    Scheme::Https,
                    Some(trusted.ip()),
                    ForwardingResult::UnresolvedChain,
                    CheckedHostSource::TrustedForwarded,
                ),
            ] {
                let probe = NativeProbe::default();
                let admission = Arc::new(Mutex::new(None));
                let middleware = Arc::new(Mutex::new(None));
                let captured_admission = Arc::clone(&admission);
                let admission_handle = probe.handle.clone();
                let router = RouterService::builder()
                    .get("/native/{id}", metadata_handler)
                    .middleware(MetadataMiddleware(Arc::clone(&middleware)))
                    .build();
                let mut app = App::new(router);
                app.set_ingress_admission_policy(move |head| {
                    assert!(matches!(
                        head.head_accounting(),
                        IngressHeadAccounting::HostManaged
                    ));
                    assert!(matches!(head.framing(), IngressFraming::HostManaged));
                    assert_eq!(admission_handle.snapshot().active_requests, 1);
                    let metadata = head
                        .extension::<NativeIngressMetadata>()
                        .expect("admission metadata");
                    assert_eq!(
                        head.extension::<CheckedEffectiveHost>()
                            .expect("checked host"),
                        metadata.effective_host()
                    );
                    *captured_admission.lock().expect("admission facts") =
                        Some(metadata_facts(metadata, head.headers(), head.target()));
                    AdmissionDecision::Admit {
                        completion: ResponseEgressCompletion::empty(),
                        grant: IngressGrant::empty(),
                        read_deadline: head.read_deadline_after(Duration::from_secs(30)),
                    }
                });
                let policy = if enabled {
                    TrustedProxyPolicy::new(family, ["192.0.2.2"]).expect("policy")
                } else {
                    TrustedProxyPolicy::no_trust()
                };
                let mut service = observed_service(app, policy, &probe);
                let owned_connection = tracked_connection(&mut service);
                let incoming = Request::builder()
                    .uri("https://direct.example/native/SECRET-PATH?token=SECRET-QUERY")
                    .header("host", "direct.example")
                    .header("forwarded", forwarded)
                    .header("x-forwarded-for", "203.0.113.9")
                    .header("x-forwarded-host", "x-public.example")
                    .header("x-forwarded-proto", "https")
                    .header("X-FoRwArDeD-Private-Secret", "SECRET-HEADER")
                    .header("x-request-id", "SECRET-VISITOR-ID")
                    .body(AxumBody::empty())
                    .expect("request");
                let (mut parts, body) = incoming.into_parts();
                let received = validate_normalized_ingress_parts_with_summary(
                    &parts,
                    IngressHeadLimits::default(),
                )
                .expect("original summary");
                let foreign = NativeSession::new(NativeDiagnosticsHandle::new(), false)
                    .expect("foreign session");
                let foreign_guard = foreign
                    .request(
                        &Method::GET,
                        MonotonicInstant::now(),
                        MonotonicClock::default(),
                        None,
                    )
                    .expect("foreign request");
                let foreign_id = foreign_guard.request_id();
                let claimed = normalize(
                    &TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, ["192.0.2.2"])
                        .expect("forged policy"),
                    Some(trusted),
                    &parts,
                );
                parts
                    .extensions
                    .insert(NativeIngressMetadata::new(foreign_id, claimed, received));
                parts.extensions.insert(CheckedEffectiveHost::from_ingress(
                    Some("FORGED-HOST.example".parse().expect("forged authority")),
                    CheckedHostSource::TrustedForwarded,
                ));
                parts
                    .extensions
                    .insert(ConnectInfo(if peer == Some(trusted) {
                        untrusted
                    } else {
                        trusted
                    }));
                parts.extensions.insert(AxumRequestContext {
                    remote_addr: Some(if peer == Some(trusted) {
                        untrusted
                    } else {
                        trusted
                    }),
                });
                drop(foreign_guard);
                let response = service
                    .dispatch_request(
                        Request::from_parts(parts, body),
                        &EgressConnection::default(),
                        peer,
                    )
                    .await
                    .expect("managed response");
                assert!(!response.headers().contains_key("x-request-id"));
                assert!(!response.headers().contains_key("request-id"));
                assert_eq!(probe.handle.snapshot().active_requests, 1);
                let result: Value = serde_json::from_slice(
                    &to_bytes(response.into_body(), 8192)
                        .await
                        .expect("metadata body"),
                )
                .expect("metadata JSON");
                let facts = &result["facts"];
                assert_eq!(
                    facts,
                    admission
                        .lock()
                        .expect("admission facts")
                        .as_ref()
                        .expect("observed admission")
                );
                assert_eq!(
                    facts,
                    middleware
                        .lock()
                        .expect("middleware facts")
                        .as_ref()
                        .expect("observed middleware")
                );
                assert_eq!(
                    facts["peer"],
                    json!(peer.map(|address| address.to_string()))
                );
                assert_eq!(
                    facts["client"],
                    json!(expected_client.map(|address| address.to_string()))
                );
                assert_eq!(facts["host"], expected_host);
                assert_eq!(facts["scheme"], expected_scheme.as_str());
                assert_eq!(facts["host_source"], source_name(expected_source));
                assert_eq!(facts["scheme_source"], source_name(expected_source));
                let client_source = if peer.is_none() {
                    CheckedHostSource::Unavailable
                } else if expected_result == ForwardingResult::Accepted {
                    expected_source
                } else {
                    CheckedHostSource::Direct
                };
                assert_eq!(facts["client_source"], source_name(client_source));
                assert_eq!(facts["forwarding_result"], expected_result.as_str());
                assert_eq!(facts["host_header"], "direct.example");
                assert_eq!(
                    facts["uri"],
                    "https://direct.example/native/SECRET-PATH?token=SECRET-QUERY"
                );
                assert_eq!(facts["raw_forwarding_absent"], true);
                assert_eq!(
                    facts["normalized"],
                    json!({"target_bytes":received.target_bytes(), "header_bytes":received.header_bytes(), "header_count":received.header_count()})
                );
                assert_eq!(result["forwarded_host"], expected_host);
                assert_eq!(
                    result["direct_context_peer"],
                    json!(peer.map(|address| address.to_string()))
                );
                assert_ne!(facts["request_id"], foreign_id.to_string());
                assert_ne!(facts["request_id"], "SECRET-VISITOR-ID");
                let record = assert_one_released(&probe);
                assert_eq!(facts["request_id"], record.request_id.to_string());
                assert_eq!(record.route, "/native/{id}");
                assert_eq!(record.forwarding_result, expected_result);
                assert_eq!(record.status, Some(StatusCode::OK));
                assert_eq!(
                    record.level(),
                    if expected_result == ForwardingResult::Malformed {
                        log::Level::Warn
                    } else {
                        log::Level::Info
                    }
                );
                assert_eq!(probe.handle.snapshot().idle_connections, 1);
                drop(owned_connection);
                assert_eq!(
                    probe.handle.snapshot(),
                    NativeDiagnosticsSnapshot::default()
                );
            }
        }

        #[tokio::test]
        async fn original_forwarding_head_overages_reject_before_stripping_or_body_poll() {
            for limits in [
                IngressHeadLimits::default()
                    .with_max_request_header_count(1)
                    .expect("field limit"),
                IngressHeadLimits::default()
                    .with_max_request_header_bytes(32)
                    .expect("byte limit"),
            ] {
                let probe = NativeProbe::default();
                let body_polls = Arc::new(AtomicUsize::new(0));
                let source_drops = Arc::new(AtomicUsize::new(0));
                let source_guard = DropSignal(Arc::clone(&source_drops));
                let counted_polls = Arc::clone(&body_polls);
                let body = AxumBody::from_stream(poll_fn(move |_context| {
                    let _retained = &source_guard;
                    counted_polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<Option<Result<Bytes, io::Error>>>::Pending
                }));
                let admission_calls = Arc::new(AtomicUsize::new(0));
                let counted_admission = Arc::clone(&admission_calls);
                let app_reports = Arc::new(Mutex::new(Vec::new()));
                let mut app = App::new(
                    RouterService::builder()
                        .post("/head", success_handler)
                        .build(),
                );
                app.set_ingress_head_limits(limits);
                app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
                app.set_ingress_admission_policy(move |_head| {
                    counted_admission.fetch_add(1, Ordering::SeqCst);
                    AdmissionDecision::Abort
                });
                let service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let incoming = Request::builder()
                    .method("POST")
                    .uri("/head")
                    .header("host", "d.test")
                    .header(
                        "forwarded",
                        "for=203.0.113.8;host=PUBLIC-SECRET.example;proto=https",
                    )
                    .header("x-forwarded-private", "SECRET-HEADER")
                    .body(body)
                    .expect("head request");
                let (mut parts, native_body) = incoming.into_parts();
                let saved = parts.headers.clone();
                strip_forwarding_headers(&mut parts.headers);
                validate_normalized_ingress_parts_with_summary(&parts, limits)
                    .expect("stripped head alone would fit");
                parts.headers = saved;
                let response = service
                    .dispatch_request(
                        Request::from_parts(parts, native_body),
                        &EgressConnection::default(),
                        None,
                    )
                    .await
                    .expect("detached head response");
                assert_eq!(
                    response.status(),
                    StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
                );
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(admission_calls.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                let rendered = to_bytes(response.into_body(), 1024)
                    .await
                    .expect("head error body");
                assert!(!String::from_utf8_lossy(&rendered).contains("SECRET"));
                let record = assert_one_released(&probe);
                assert_eq!(
                    record.failure,
                    Some(NativeRequestFailure::PropagatedError {
                        kind: "request_header_fields_too_large"
                    })
                );
                assert_eq!(
                    record.status,
                    Some(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE)
                );
                assert_eq!(record.forwarding_result, ForwardingResult::Absent);
                assert!(app_reports.lock().expect("detached app reports").is_empty());
            }
        }

        #[tokio::test]
        async fn public_metadata_mutation_cannot_change_private_record_identity_or_add_response_id()
        {
            let probe = NativeProbe::default();
            let router = RouterService::builder()
                .get("/mutate", mutate_metadata_handler)
                .build();
            let service =
                observed_service(App::new(router), TrustedProxyPolicy::no_trust(), &probe);
            let request = Request::builder()
                .uri("/mutate")
                .header("x-request-id", "VISITOR-SENTINEL")
                .body(AxumBody::empty())
                .expect("request");
            let response = service
                .dispatch_request(request, &EgressConnection::default(), None)
                .await
                .expect("response");
            assert!(!response.headers().contains_key("x-request-id"));
            assert!(!response.headers().contains_key("request-id"));
            let identity = to_bytes(response.into_body(), 1024)
                .await
                .expect("identity body");
            let record = assert_one_released(&probe);
            assert_eq!(identity.as_ref(), record.request_id.to_string().as_bytes());
            assert_eq!(identity.len(), 64);
            assert_ne!(identity, "VISITOR-SENTINEL");
        }

        #[tokio::test]
        #[expect(
            clippy::too_many_lines,
            reason = "The abort, drain and refusal matrix shares lifetime fixtures while checking their distinct no-response versus detached-completion contracts"
        )]
        async fn native_abort_drain_and_refusal_release_once_without_inventing_egress() {
            for mode in ["abort", "drain", "refuse"] {
                let probe = NativeProbe::default();
                let body_polls = Arc::new(AtomicUsize::new(0));
                let observed_polls = Arc::clone(&body_polls);
                let source_drops = Arc::new(AtomicUsize::new(0));
                let source_guard = DropSignal(Arc::clone(&source_drops));
                let body = AxumBody::from_stream(poll_fn(move |_context| {
                    let _retained = &source_guard;
                    observed_polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<Option<Result<Bytes, io::Error>>>::Pending
                }));
                let app_reports = Arc::new(Mutex::new(Vec::new()));
                let completion_calls = Arc::new(AtomicUsize::new(0));
                let counted_completion = Arc::clone(&completion_calls);
                let admission_calls = Arc::new(AtomicUsize::new(0));
                let counted_admission = Arc::clone(&admission_calls);
                let admission_handle = probe.handle.clone();
                let mut app = App::new(
                    RouterService::builder()
                        .post("/gate", success_handler)
                        .build(),
                );
                app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
                app.set_ingress_admission_policy(move |_head| {
                    counted_admission.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(admission_handle.snapshot().active_requests, 1);
                    if mode == "refuse" {
                        let completed = Arc::clone(&counted_completion);
                        AdmissionDecision::Refuse {
                            completion: ResponseEgressCompletion::new(move |_report| {
                                completed.fetch_add(1, Ordering::SeqCst);
                            }),
                            response: response_builder()
                                .status(StatusCode::SERVICE_UNAVAILABLE)
                                .body(Body::from("admission refusal"))
                                .expect("refusal"),
                        }
                    } else {
                        AdmissionDecision::Abort
                    }
                });
                let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let (_phase_sender, phase) = watch::channel(if mode == "drain" {
                    LifecyclePhase::Draining
                } else {
                    LifecyclePhase::Ready
                });
                service.phase = Some(phase);
                let owned_connection = tracked_connection(&mut service);
                let request = Request::builder()
                    .method("POST")
                    .uri("/gate")
                    .body(body)
                    .expect("request");
                let outcome = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await;
                if mode == "refuse" {
                    let response = outcome.expect("refusal response");
                    assert_eq!(probe.handle.snapshot().active_requests, 1);
                    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                    assert_eq!(
                        to_bytes(response.into_body(), 1024)
                            .await
                            .expect("refusal body"),
                        "admission refusal"
                    );
                    let record = assert_one_released(&probe);
                    assert_eq!(record.status, Some(StatusCode::SERVICE_UNAVAILABLE));
                    assert_eq!(record.failure, Some(NativeRequestFailure::AdmissionRefused));
                    assert_eq!(
                        record.outcome,
                        NativeRequestOutcome::Egress(ResponseEgressOutcome::HostHandoff)
                    );
                    assert_eq!(completion_calls.load(Ordering::SeqCst), 1);
                    // Refusal retains its completion, not the admitted-request observer.
                    assert!(app_reports.lock().expect("app reports").is_empty());
                } else {
                    let Err(_aborted) = outcome else {
                        panic!("no response expected on abort or drain");
                    };
                    let record = assert_one_released(&probe);
                    assert_eq!(record.event, "request_ingress");
                    assert_eq!(record.status, None);
                    assert_eq!(record.bytes_written, None);
                    assert_eq!(record.body_kind, None);
                    assert_eq!(
                        record.outcome,
                        NativeRequestOutcome::Ingress(if mode == "drain" {
                            IngressDisposition::Draining
                        } else {
                            IngressDisposition::Aborted
                        })
                    );
                    assert_eq!(completion_calls.load(Ordering::SeqCst), 0);
                    assert!(app_reports.lock().expect("app reports").is_empty());
                }
                assert_eq!(
                    admission_calls.load(Ordering::SeqCst),
                    usize::from(mode != "drain")
                );
                assert_eq!(body_polls.load(Ordering::SeqCst), 0);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_eq!(probe.handle.snapshot().idle_connections, 1);
                drop(owned_connection);
                assert_eq!(
                    probe.handle.snapshot(),
                    NativeDiagnosticsSnapshot::default()
                );
            }
        }

        #[tokio::test]
        #[expect(
            clippy::too_many_lines,
            reason = "Three cancellation stages retain one fixture and its before-drop and after-drop assertions for native, body, grant and completion ownership"
        )]
        async fn cancelled_body_handler_and_fallback_futures_release_native_and_app_resources() {
            for stage in ["body", "handler", "fallback"] {
                let probe = NativeProbe::default();
                let body_polls = Arc::new(AtomicUsize::new(0));
                let observed_polls = Arc::clone(&body_polls);
                let source_drops = Arc::new(AtomicUsize::new(0));
                let source_guard = DropSignal(Arc::clone(&source_drops));
                let body_handle = probe.handle.clone();
                let body = AxumBody::from_stream(poll_fn(move |_context| {
                    let _retained = &source_guard;
                    assert_eq!(body_handle.snapshot().active_requests, 1);
                    observed_polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<Option<Result<Bytes, io::Error>>>::Pending
                }));
                let handler_calls = Arc::new(AtomicUsize::new(0));
                let router = match stage {
                    "body" => RouterService::builder()
                        .post("/pending", read_body_handler)
                        .build(),
                    "handler" => RouterService::builder()
                        .post("/pending", pending_handler)
                        .build(),
                    _ => RouterService::builder().build(),
                };
                let app_reports = Arc::new(Mutex::new(Vec::new()));
                let completion_calls = Arc::new(AtomicUsize::new(0));
                let retained_drops = Arc::new(AtomicUsize::new(0));
                let grant_drops = Arc::new(AtomicUsize::new(0));
                let captured_completions = Arc::clone(&completion_calls);
                let captured_retained = Arc::clone(&retained_drops);
                let captured_grants = Arc::clone(&grant_drops);
                let mut app = App::new(router);
                app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
                app.set_ingress_admission_policy(move |head| {
                    let calls = Arc::clone(&captured_completions);
                    let retained = DropSignal(Arc::clone(&captured_retained));
                    let completion = ResponseEgressCompletion::new(move |_report| {
                        let _resource = retained;
                        calls.fetch_add(1, Ordering::SeqCst);
                    });
                    let grant = IngressGrant::new(DropSignal(Arc::clone(&captured_grants)));
                    if stage == "fallback" {
                        AdmissionDecision::ReadBodyBeforeFallback {
                            completion,
                            grant,
                            max_body_bytes: 1024,
                            read_deadline: head.read_deadline_after(Duration::from_secs(30)),
                            on_exceeded: BufferedIngressResponse::text(StatusCode::OK, "overflow"),
                            on_timeout: BufferedIngressResponse::text(StatusCode::OK, "timeout"),
                        }
                    } else {
                        AdmissionDecision::Admit {
                            completion,
                            grant,
                            read_deadline: head.read_deadline_after(Duration::from_secs(30)),
                        }
                    }
                });
                let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let owned_connection = tracked_connection(&mut service);
                let mut request = Request::builder()
                    .method("POST")
                    .uri("/pending")
                    .body(body)
                    .expect("request");
                request
                    .extensions_mut()
                    .insert(PendingCalls(Arc::clone(&handler_calls)));
                let connection = EgressConnection::default();
                let mut response = Box::pin(service.dispatch_request(request, &connection, None));
                assert!(matches!(
                    futures_util::poll!(response.as_mut()),
                    Poll::Pending
                ));
                assert_eq!(
                    probe.handle.snapshot(),
                    NativeDiagnosticsSnapshot {
                        active_requests: 1,
                        active_connections: 1,
                        idle_connections: 0
                    }
                );
                assert_eq!(source_drops.load(Ordering::SeqCst), 0);
                assert_eq!(retained_drops.load(Ordering::SeqCst), 0);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 0);
                assert_eq!(
                    handler_calls.load(Ordering::SeqCst),
                    usize::from(stage != "fallback")
                );
                assert_eq!(body_polls.load(Ordering::SeqCst) > 0, stage != "handler");
                drop(response);
                assert_eq!(source_drops.load(Ordering::SeqCst), 1);
                assert_eq!(retained_drops.load(Ordering::SeqCst), 1);
                assert_eq!(grant_drops.load(Ordering::SeqCst), 1);
                assert_eq!(completion_calls.load(Ordering::SeqCst), 0);
                assert!(app_reports.lock().expect("app reports").is_empty());
                let record = assert_one_released(&probe);
                assert_eq!(
                    record.outcome,
                    NativeRequestOutcome::Ingress(IngressDisposition::Abandoned)
                );
                assert_eq!(record.status, None);
                assert_eq!(record.bytes_written, None);
                assert_eq!(probe.handle.snapshot().idle_connections, 1);
                drop(owned_connection);
                assert_eq!(
                    probe.handle.snapshot(),
                    NativeDiagnosticsSnapshot::default()
                );
            }
        }

        #[tokio::test]
        async fn prepared_response_drop_is_one_failing_egress_record_with_original_status() {
            let probe = NativeProbe::default();
            let app_reports = Arc::new(Mutex::new(Vec::new()));
            let mut app = App::new(
                RouterService::builder()
                    .get("/drop", success_handler)
                    .build(),
            );
            app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
            let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
            let owned_connection = tracked_connection(&mut service);
            let response = service
                .dispatch_request(
                    Request::builder()
                        .uri("/drop")
                        .body(AxumBody::empty())
                        .expect("request"),
                    &EgressConnection::default(),
                    None,
                )
                .await
                .expect("response");
            assert_eq!(probe.handle.snapshot().active_requests, 1);
            drop(response);
            let record = assert_one_released(&probe);
            assert_eq!(record.event, "request_terminal");
            assert_eq!(record.status, Some(StatusCode::OK));
            assert_eq!(
                record.outcome,
                NativeRequestOutcome::Egress(ResponseEgressOutcome::TransportError)
            );
            assert_eq!(record.level(), log::Level::Warn);
            assert_eq!(app_reports.lock().expect("app reports").len(), 1);
            drop(owned_connection);
            assert_eq!(
                probe.handle.snapshot(),
                NativeDiagnosticsSnapshot::default()
            );
        }

        #[tokio::test]
        async fn managed_native_observer_panic_preserves_release_and_application_observer() {
            for abort in [false, true] {
                let probe = NativeProbe {
                    panics: true,
                    ..NativeProbe::default()
                };
                let app_reports = Arc::new(Mutex::new(Vec::new()));
                let mut app = App::new(
                    RouterService::builder()
                        .get("/observer", success_handler)
                        .build(),
                );
                app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
                if abort {
                    app.set_ingress_admission_policy(|_head| AdmissionDecision::Abort);
                }
                let service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let request = Request::builder()
                    .uri("/observer")
                    .body(AxumBody::empty())
                    .expect("request");
                let outcome = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await;
                if abort {
                    let Err(_aborted) = outcome else {
                        panic!("abort produced a response");
                    };
                    assert_eq!(assert_one_released(&probe).status, None);
                    assert!(app_reports.lock().expect("app reports").is_empty());
                } else {
                    let response = outcome.expect("response despite observer panic");
                    assert_eq!(
                        to_bytes(response.into_body(), 1024)
                            .await
                            .expect("body despite observer panic"),
                        "ok"
                    );
                    assert_eq!(assert_one_released(&probe).status, Some(StatusCode::OK));
                    assert_eq!(app_reports.lock().expect("app reports").len(), 1);
                }
            }
        }

        #[tokio::test]
        async fn configured_fallback_200_has_owner_failure_but_deliberate_413_and_503_do_not() {
            for timed_out in [false, true] {
                let probe = NativeProbe::default();
                let mut app = App::new(RouterService::builder().build());
                app.set_ingress_admission_policy(move |head| {
                    AdmissionDecision::ReadBodyBeforeFallback {
                        completion: ResponseEgressCompletion::empty(),
                        grant: IngressGrant::empty(),
                        max_body_bytes: 1,
                        read_deadline: if timed_out {
                            Deadline::at_instant(head.request_start())
                        } else {
                            head.read_deadline_after(Duration::from_secs(30))
                        },
                        on_exceeded: BufferedIngressResponse::text(
                            StatusCode::OK,
                            "configured overflow",
                        ),
                        on_timeout: BufferedIngressResponse::text(
                            StatusCode::OK,
                            "configured timeout",
                        ),
                    }
                });
                let service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let request = Request::builder()
                    .method("POST")
                    .uri("/missing")
                    .body(AxumBody::from("ab"))
                    .expect("fallback request");
                let response = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await
                    .expect("configured response");
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    to_bytes(response.into_body(), 1024)
                        .await
                        .expect("configured body"),
                    if timed_out {
                        "configured timeout"
                    } else {
                        "configured overflow"
                    }
                );
                let record = assert_one_released(&probe);
                assert_eq!(
                    record.failure,
                    Some(if timed_out {
                        NativeRequestFailure::FallbackReadTimedOut
                    } else {
                        NativeRequestFailure::FallbackBodyExceeded
                    })
                );
                assert_eq!(record.status, Some(StatusCode::OK));
                assert_eq!(record.level(), log::Level::Warn);
            }
            for (path, status) in [
                ("/ordinary/413", StatusCode::PAYLOAD_TOO_LARGE),
                ("/ordinary/503", StatusCode::SERVICE_UNAVAILABLE),
            ] {
                let probe = NativeProbe::default();
                let service = observed_service(
                    App::new(
                        RouterService::builder()
                            .get("/ordinary/{status}", ordinary_status_handler)
                            .build(),
                    ),
                    TrustedProxyPolicy::no_trust(),
                    &probe,
                );
                let request = Request::builder()
                    .uri(path)
                    .body(AxumBody::empty())
                    .expect("ordinary request");
                let response = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await
                    .expect("ordinary response");
                assert_eq!(response.status(), status);
                to_bytes(response.into_body(), 1024)
                    .await
                    .expect("ordinary body");
                let record = assert_one_released(&probe);
                assert_eq!(record.failure, None);
                assert_eq!(record.status, Some(status));
                assert_eq!(record.level(), log::Level::Info);
            }
        }

        #[tokio::test]
        async fn native_json_overflow_observes_owning_413_even_with_custom_error_body() {
            let probe = NativeProbe::default();
            let mut app = App::new(
                RouterService::builder()
                    .post("/json", json_limit_probe)
                    .build(),
            );
            app.set_error_response_renderer(|_error| {
                CoreResponse::new(Body::from("custom JSON body"))
            });
            let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
            service.json_body_limit = JsonBodyLimit::new(4).expect("JSON limit");
            let request = Request::builder()
                .method("POST")
                .uri("/json")
                .header("content-type", "application/json")
                .body(AxumBody::from("\"abcdef\""))
                .expect("JSON request");
            let response = service
                .dispatch_request(request, &EgressConnection::default(), None)
                .await
                .expect("JSON response");
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(
                to_bytes(response.into_body(), 1024)
                    .await
                    .expect("JSON body"),
                "custom JSON body"
            );
            let record = assert_one_released(&probe);
            assert_eq!(record.status, Some(StatusCode::PAYLOAD_TOO_LARGE));
            assert_eq!(
                record.failure,
                Some(NativeRequestFailure::PropagatedError {
                    kind: "payload_too_large"
                })
            );
            assert_eq!(record.route, "/json");
            assert_eq!(record.level(), log::Level::Warn);
        }

        #[tokio::test]
        #[expect(
            clippy::too_many_lines,
            reason = "Framework routing, handler errors and custom rendering are paired negative controls that keep privacy and provenance assertions in one matrix"
        )]
        async fn native_routing_privacy_preserves_router_provenance_allow_and_application_control()
        {
            for (path, method, status, framework, kind, public_sentinel) in [
                (
                    "/missing/FRAMEWORK_PATH_SENTINEL",
                    "GET",
                    StatusCode::NOT_FOUND,
                    true,
                    "not_found",
                    false,
                ),
                (
                    "/known",
                    "PRIVATE-METHOD-SENTINEL",
                    StatusCode::METHOD_NOT_ALLOWED,
                    true,
                    "method_not_allowed",
                    false,
                ),
                (
                    "/handler404",
                    "GET",
                    StatusCode::NOT_FOUND,
                    false,
                    "not_found",
                    true,
                ),
                (
                    "/internal",
                    "GET",
                    StatusCode::INTERNAL_SERVER_ERROR,
                    false,
                    "internal",
                    false,
                ),
                (
                    "/upstream",
                    "GET",
                    StatusCode::BAD_GATEWAY,
                    false,
                    "bad_gateway",
                    false,
                ),
            ] {
                let probe = NativeProbe::default();
                let router = RouterService::builder()
                    .get("/known", success_handler)
                    .get("/handler404", handler_not_found)
                    .get("/internal", internal_error_handler)
                    .get("/upstream", upstream_error_handler)
                    .build();
                let service =
                    observed_service(App::new(router), TrustedProxyPolicy::no_trust(), &probe);
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .body(AxumBody::empty())
                    .expect("error request");
                let response = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await
                    .expect("error response");
                assert_eq!(response.status(), status);
                if status == StatusCode::METHOD_NOT_ALLOWED {
                    assert_eq!(response.headers().get("allow").expect("Allow"), "GET");
                }
                let body = to_bytes(response.into_body(), 2048)
                    .await
                    .expect("error body");
                assert_eq!(
                    String::from_utf8_lossy(&body).contains("SENTINEL"),
                    public_sentinel
                );
                let record = assert_one_released(&probe);
                assert_eq!(
                    record.failure,
                    Some(if framework {
                        NativeRequestFailure::FrameworkRouting { kind }
                    } else {
                        NativeRequestFailure::PropagatedError { kind }
                    })
                );
                assert_eq!(
                    record.level(),
                    if framework {
                        log::Level::Info
                    } else {
                        log::Level::Warn
                    }
                );
                assert_eq!(record.method, if method == "GET" { "GET" } else { "other" });
            }
            let probe = NativeProbe::default();
            let mut app = App::new(RouterService::builder().build());
            app.set_error_response_renderer(|error| {
                let chosen_body = error.to_string();
                assert!(chosen_body.contains("CUSTOM_ROUTE_SENTINEL"));
                CoreResponse::new(Body::from(chosen_body))
            });
            let service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
            let request = Request::builder()
                .uri("/CUSTOM_ROUTE_SENTINEL")
                .body(AxumBody::empty())
                .expect("custom request");
            let response = service
                .dispatch_request(request, &EgressConnection::default(), None)
                .await
                .expect("custom response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(
                String::from_utf8_lossy(
                    &to_bytes(response.into_body(), 1024)
                        .await
                        .expect("custom body")
                )
                .contains("CUSTOM_ROUTE_SENTINEL")
            );
            assert_eq!(
                assert_one_released(&probe).failure,
                Some(NativeRequestFailure::FrameworkRouting { kind: "not_found" })
            );
        }

        #[tokio::test]
        async fn managed_source_and_deadline_failures_keep_selected_200_prefix_accounting_and_one_record()
         {
            for failure in [
                ResponseEgressOutcome::SourceError,
                ResponseEgressOutcome::DeadlineExceeded,
            ] {
                let probe = NativeProbe::default();
                let app_reports = Arc::new(Mutex::new(Vec::new()));
                let start = MonotonicInstant::now();
                let now = Arc::new(Mutex::new(start));
                let captured_clock = Arc::clone(&now);
                let mut app = App::new(
                    RouterService::builder()
                        .get("/stream/{failure}", terminal_stream_handler)
                        .build(),
                );
                app.set_monotonic_clock(MonotonicClock::new(move || {
                    *captured_clock.lock().expect("clock")
                }));
                app.set_response_egress_observer(RecordingEgressObserver(Arc::clone(&app_reports)));
                app.set_response_egress_policy(|_head, egress_started_at| ResponseEgressPolicy {
                    write_deadline: Deadline::at_instant(
                        egress_started_at
                            .checked_add(Duration::from_secs(1))
                            .expect("write deadline"),
                    ),
                });
                let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
                let owned_connection = tracked_connection(&mut service);
                let path = if failure == ResponseEgressOutcome::SourceError {
                    "/stream/source"
                } else {
                    "/stream/deadline"
                };
                let request = Request::builder()
                    .uri(path)
                    .body(AxumBody::empty())
                    .expect("stream request");
                let response = service
                    .dispatch_request(request, &EgressConnection::default(), None)
                    .await
                    .expect("stream response");
                assert_eq!(response.status(), StatusCode::OK);
                let mut body = response.into_body();
                let prefix = poll_future(|context| Pin::new(&mut body).poll_frame(context))
                    .await
                    .expect("prefix frame")
                    .expect("prefix data")
                    .into_data()
                    .expect("data frame");
                assert_eq!(prefix, "terminal-prefix");
                assert_eq!(probe.handle.snapshot().active_requests, 1);
                assert!(probe.records.lock().expect("native records").is_empty());
                if failure == ResponseEgressOutcome::DeadlineExceeded {
                    *now.lock().expect("clock") = start
                        .checked_add(Duration::from_secs(1))
                        .expect("expired instant");
                }
                let terminal_error = poll_future(|context| Pin::new(&mut body).poll_frame(context))
                    .await
                    .expect("terminal frame")
                    .expect_err("terminal failure");
                assert!(!terminal_error.to_string().contains("SENTINEL"));
                // Hyper stops after a body error; an ended body need not be repolled.
                assert!(body.is_end_stream());
                drop(body);
                let record = assert_one_released(&probe);
                assert_eq!(record.status, Some(StatusCode::OK));
                assert_eq!(record.outcome, NativeRequestOutcome::Egress(failure));
                assert_eq!(record.bytes_written, Some(15));
                assert_eq!(record.fallback_disposition, None);
                assert_eq!(record.route, "/stream/{failure}");
                assert_eq!(record.level(), log::Level::Warn);
                assert_eq!(app_reports.lock().expect("app reports").len(), 1);
                assert_eq!(app_reports.lock().expect("app reports")[0].outcome, failure);
                drop(owned_connection);
                assert_eq!(
                    probe.handle.snapshot(),
                    NativeDiagnosticsSnapshot::default()
                );
            }
        }

        #[tokio::test]
        async fn managed_precommit_framing_fallback_records_selected_500_and_original_cause() {
            let probe = NativeProbe::default();
            let service = observed_service(
                App::new(
                    RouterService::builder()
                        .get("/framing", bad_framing_handler)
                        .build(),
                ),
                TrustedProxyPolicy::no_trust(),
                &probe,
            );
            let request = Request::builder()
                .uri("/framing")
                .body(AxumBody::empty())
                .expect("request");
            let response = service
                .dispatch_request(request, &EgressConnection::default(), None)
                .await
                .expect("fallback response");
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = to_bytes(response.into_body(), 1024)
                .await
                .expect("fallback body");
            assert!(!String::from_utf8_lossy(&body).contains("mismatched"));
            let record = assert_one_released(&probe);
            assert_eq!(record.status, Some(StatusCode::INTERNAL_SERVER_ERROR));
            assert_eq!(
                record.outcome,
                NativeRequestOutcome::Egress(ResponseEgressOutcome::ConversionError)
            );
            assert_eq!(
                record.body_kind,
                Some(edgezero_core::ResponseEgressBodyKind::Fallback)
            );
            assert_eq!(
                record.fallback_disposition,
                Some(ResponseEgressFallbackDisposition::Completed)
            );
        }

        #[tokio::test]
        async fn managed_hooks_keep_app_native_observer_order_and_authoritative_total_duration() {
            let probe = NativeProbe::default();
            let start = MonotonicInstant::now();
            let now = Arc::new(Mutex::new(start));
            let source = Arc::clone(&now);
            let admission_now = Arc::clone(&now);
            let completion_now = Arc::clone(&now);
            let completion_events = Arc::clone(&probe.events);
            let reports = Arc::new(Mutex::new(Vec::new()));
            let mut app = App::new(
                RouterService::builder()
                    .get("/timed", success_handler)
                    .build(),
            );
            app.set_monotonic_clock(MonotonicClock::new(move || *source.lock().expect("clock")));
            app.set_ingress_admission_policy(move |head: &IngressHead| {
                assert_eq!(head.request_start(), start);
                *admission_now.lock().expect("clock") = start
                    .checked_add(Duration::from_secs(3))
                    .expect("pre-egress instant");
                let events = Arc::clone(&completion_events);
                let observed_clock = Arc::clone(&completion_now);
                AdmissionDecision::Admit {
                    completion: ResponseEgressCompletion::new(move |_report| {
                        events.lock().expect("hook events").push("app");
                        *observed_clock.lock().expect("clock") = start
                            .checked_add(Duration::from_secs(80))
                            .expect("callback instant");
                    }),
                    grant: IngressGrant::empty(),
                    read_deadline: head.read_deadline_after(Duration::from_secs(30)),
                }
            });
            app.set_response_egress_observer(OrderedAppObserver {
                events: Arc::clone(&probe.events),
                reports: Arc::clone(&reports),
            });
            let mut service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
            let owned_connection = tracked_connection(&mut service);
            let response = service
                .dispatch_request(
                    Request::builder()
                        .uri("/timed")
                        .body(AxumBody::empty())
                        .expect("request"),
                    &EgressConnection::default(),
                    None,
                )
                .await
                .expect("response");
            assert_eq!(probe.handle.snapshot().active_requests, 1);
            *now.lock().expect("clock") = start
                .checked_add(Duration::from_secs(8))
                .expect("terminal instant");
            to_bytes(response.into_body(), 1024)
                .await
                .expect("response body");
            let record = assert_one_released(&probe);
            assert_eq!(record.duration, Some(Duration::from_secs(8)));
            assert_eq!(
                reports.lock().expect("app reports")[0].elapsed,
                Duration::from_secs(5)
            );
            assert_eq!(
                probe.events.lock().expect("hook events").as_slice(),
                ["app", "native", "observer"]
            );
            assert_eq!(probe.handle.snapshot().idle_connections, 1);
            drop(owned_connection);
            assert_eq!(
                probe.handle.snapshot(),
                NativeDiagnosticsSnapshot::default()
            );
        }

        #[tokio::test]
        async fn managed_backwards_request_clock_is_safe_native_fault_without_fake_duration() {
            let probe = NativeProbe::default();
            let now = MonotonicInstant::now();
            let request_start = now
                .checked_add(Duration::from_secs(1))
                .expect("request start");
            let samples = Arc::new(AtomicUsize::new(0));
            let captured_samples = Arc::clone(&samples);
            let mut app = App::new(
                RouterService::builder()
                    .get("/backwards", success_handler)
                    .build(),
            );
            app.set_monotonic_clock(MonotonicClock::new(move || {
                if captured_samples.fetch_add(1, Ordering::SeqCst) == 0 {
                    request_start
                } else {
                    now
                }
            }));
            let service = observed_service(app, TrustedProxyPolicy::no_trust(), &probe);
            let request = Request::builder()
                .uri("/backwards")
                .body(AxumBody::empty())
                .expect("request");
            let response = service
                .dispatch_request(request, &EgressConnection::default(), None)
                .await
                .expect("response");
            to_bytes(response.into_body(), 1024)
                .await
                .expect("response body");
            let record = assert_one_released(&probe);
            assert_eq!(record.status, Some(StatusCode::OK));
            assert_eq!(record.duration, None);
            assert_eq!(record.level(), log::Level::Error);
        }
    }

    /// Send a GET request through `service` and return the response body as a UTF-8 string.
    /// Lifted out of the registry-aware tests so each can stay flat (clippy
    /// `items_after_statements` rejects nested `async fn` definitions).
    async fn body_at(service: &AxumServiceState, path: &str) -> String {
        let request = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        let mut svc = service.clone();
        let response = svc.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }
}
