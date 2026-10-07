//! Test-only executable for real startup, signal, drain and storage acceptance.
#![expect(clippy::print_stdout, reason = "bounded subprocess IPC markers")]

#[path = "../../../edgezero-core/tests/fixtures/production_handlers.rs"]
mod fixture;

use std::cell::Cell;
use std::env;
use std::future;
use std::net::SocketAddr;
use std::panic::set_hook;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use edgezero_adapter_axum::context::AxumRequestContext;
use edgezero_adapter_axum::dev_server::{
    AxumRunOptions, run_app, run_app_with_options_and_initializer, run_production_app,
    run_production_app_with_initializer,
};
use edgezero_adapter_axum::diagnostics::{
    IngressDisposition, NativeDiagnosticsHandle, NativeDiagnosticsSnapshot, NativeIngressMetadata,
    NativeRequestFailure, NativeRequestObserver, NativeRequestOutcome, NativeRequestRecord,
};
use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::context::RequestContext;
use edgezero_core::extractor::{AppConfig, ForwardedHost, FromRequest as _, Json, State};
use edgezero_core::http::{HeaderMap, HeaderValue, Response, StatusCode, Uri};
use edgezero_core::ingress::{
    AdmissionDecision, CheckedEffectiveHost, CheckedHostSource, IngressGrant,
};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::middleware::{Middleware, Next};
use edgezero_core::params::PathParams;
use edgezero_core::probe::{self, LifecyclePhase, LifecycleReader};
use edgezero_core::response::response_with_body;
use edgezero_core::response_egress::{
    ResponseEgressBodyKind, ResponseEgressCompletion, ResponseEgressDeadline,
    ResponseEgressFallbackDisposition, ResponseEgressObserver, ResponseEgressOutcome,
    ResponseEgressReport,
};
use edgezero_core::router::RouterService;
use edgezero_core::{BadGatewayReason, Body, EdgeError, MonotonicClock, MonotonicInstant, action};
use futures::stream;
use log::{LevelFilter, Log, Metadata, Record};
use serde_json::{Value, json};
use tokio::signal;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio::sync::oneshot;
use tokio::time::sleep;

static ADMISSION_FACTS: Mutex<Option<Value>> = Mutex::new(None);
static MIDDLEWARE_FACTS: Mutex<Option<Value>> = Mutex::new(None);
static PANIC_COMPLETION: AtomicBool = AtomicBool::new(false);
static LOGGER_FAULT_REPORTED: AtomicBool = AtomicBool::new(false);
static NATIVE_LOGGER: NativeFixtureLogger = NativeFixtureLogger;

#[derive(Clone, Default)]
struct NativeFailureInputs {
    configuration: String,
    secret: String,
}

struct NativeFixtureLogger;

impl Log for NativeFixtureLogger {
    #[inline]
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("edgezero")
    }

    #[inline]
    fn flush(&self) {}

    #[inline]
    #[expect(
        clippy::panic,
        reason = "regression intentionally panics during the core completion-fault log attempt to prove the joined native completion and app observer still run"
    )]
    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        println!(
            "FRAMEWORK_LOG {}",
            json!({"level": record.level().as_str(), "target": record.target(), "message": message})
        );
        if PANIC_COMPLETION.load(Ordering::Relaxed)
            && message == "response-egress completion panicked after terminal transition"
            && !LOGGER_FAULT_REPORTED.swap(true, Ordering::Relaxed)
        {
            println!("LOGGER_FAULT");
            panic!("fixture completion logger fault");
        }
    }
}

struct NativeFixtureObserver(NativeDiagnosticsHandle);

impl NativeRequestObserver for NativeFixtureObserver {
    #[inline]
    fn observe(&self, record: &NativeRequestRecord) {
        println!(
            "NATIVE_OBSERVATION {}",
            json!({
                "id": record.request_id.to_string(),
                "event": record.event,
                "method": record.method,
                "route": record.route,
                "status": record.status.map(|status| status.as_u16()),
                "duration_us": record.duration.map(|duration| duration.as_micros()),
                "outcome": native_outcome(record.outcome),
                "failure": native_failure(record.failure),
                "forwarding_result": record.forwarding_result.as_str(),
                "bytes_written": record.bytes_written,
                "body_kind": record.body_kind.map(|kind| match kind {
                    ResponseEgressBodyKind::Application => "application",
                    ResponseEgressBodyKind::Fallback => "fallback",
                    _ => "unknown",
                }),
                "fallback": record.fallback_disposition.map(|disposition| match disposition {
                    ResponseEgressFallbackDisposition::Aborted => "aborted",
                    ResponseEgressFallbackDisposition::Completed => "completed",
                    _ => "unknown",
                }),
                "level": record.level().as_str(),
                "counts": snapshot_json(self.0.snapshot()),
            })
        );
    }
}

struct FixtureEgressObserver;

impl ResponseEgressObserver for FixtureEgressObserver {
    #[inline]
    fn complete(&self, report: &ResponseEgressReport) {
        println!("APP_OBSERVER {}", egress_outcome(report.outcome));
    }
}

struct NativeFactsMiddleware;

#[async_trait(?Send)]
impl Middleware for NativeFactsMiddleware {
    #[inline]
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        if ctx.uri().path() == "/native-metadata" {
            let metadata = ingress_metadata(&ctx)?;
            *MIDDLEWARE_FACTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some(metadata_facts(metadata, ctx.headers(), ctx.uri()));
        }
        next.run(ctx).await
    }
}

struct FixtureApp;

struct WorkGuard(&'static str);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        println!("{}", self.0);
    }
}

struct StopState(Mutex<Option<oneshot::Sender<()>>>);

#[expect(
    clippy::missing_trait_methods,
    reason = "fixture retains unused Hooks defaults"
)]
impl Hooks for FixtureApp {
    fn configure(app: &mut App) -> Result<(), EdgeError> {
        println!("HOOK_ENTERED");
        if mode() == "hook-error" {
            return Err(EdgeError::internal(anyhow::anyhow!("HOOK_SENTINEL")));
        }
        let selected = mode();
        if selected.starts_with("diagnostics") {
            configure_native_fixture(app, &selected);
        }
        if env::var_os("FIXTURE_FREEZE_CLOCK").is_some() {
            let frozen = MonotonicInstant::now();
            app.set_monotonic_clock(MonotonicClock::new(move || frozen));
        }
        Ok(())
    }

    fn owns_logging() -> bool {
        true
    }

    fn routes() -> RouterService {
        RouterService::builder()
            .post("/json", buffered_json)
            .post("/json-explicit", explicit_json)
            .post("/json-small", small_json)
            .post("/generic", generic_body)
            .post("/taken", taken_body)
            .get("/ready", probe::readiness)
            .get("/live", probe::liveness)
            .get("/state", fixture::state)
            .get("/typed", fixture::typed)
            .get("/finite", finite)
            .get("/pending", pending)
            .get("/stream", endless_stream)
            .get("/flood", flood)
            .get("/after", after)
            .get("/write", write)
            .get("/read", read)
            .get("/alias", alias)
            .get("/distinct", distinct)
            .get("/snapshot", snapshot)
            .get("/stop", stop)
            .get("/native-metadata", native_metadata)
            .get("/native-error", native_error)
            .get("/native-upstream", native_upstream)
            .get("/native-status503", native_status503)
            .get("/native-handler404", native_handler404)
            .get("/native-mutated", native_mutated)
            .get("/native-source", native_source)
            .get("/native-deadline", native_deadline)
            .get("/native-expired", native_expired)
            .get("/native-conversion", native_conversion)
            .post("/native-json-number", native_json_number)
            .get("/native-counts", native_counts)
            .get("/native-refuse", native_status503)
            .get("/native-abort", native_status503)
            .middleware(NativeFactsMiddleware)
            .build()
    }

    fn stores() -> StoresMetadata {
        if matches!(
            mode().as_str(),
            "bare"
                | "bare-dev"
                | "hook-error"
                | "initializer-error"
                | "starting"
                | "embedding-bare"
                | "diagnostics"
                | "diagnostics-panic"
                | "diagnostics-optout"
                | "diagnostics-custom"
        ) {
            return StoresMetadata::default();
        }
        StoresMetadata {
            config: Some(StoreMetadata {
                ids: &["settings"],
                default: "settings",
            }),
            kv: Some(StoreMetadata {
                ids: &["first", "second", "third"],
                default: "first",
            }),
            secrets: Some(StoreMetadata {
                ids: &["vault"],
                default: "vault",
            }),
        }
    }
}

fn configure_native_fixture(app: &mut App, selected: &str) {
    let panic_completion = selected == "diagnostics-panic";
    app.set_response_egress_observer(FixtureEgressObserver);
    app.set_ingress_admission_policy(move |head| {
        let metadata = head.extension::<NativeIngressMetadata>();
        if head.target().path() == "/native-metadata"
            && let Some(checked) = metadata
        {
            *ADMISSION_FACTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some(metadata_facts(checked, head.headers(), head.target()));
        }
        if head.target().path() == "/native-abort" {
            return AdmissionDecision::Abort;
        }
        let id = metadata.map_or_else(
            || "unavailable".to_owned(),
            |checked| checked.request_id().to_string(),
        );
        let completion = ResponseEgressCompletion::new(move |_report| {
            println!("APP_COMPLETION {id}");
            assert!(!panic_completion, "fixture application completion fault");
        });
        if head.target().path() == "/native-refuse" {
            let mut response = Response::new(Body::text("fixture admission refused"));
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return AdmissionDecision::Refuse {
                completion,
                response,
            };
        }
        AdmissionDecision::Admit {
            completion,
            grant: IngressGrant::empty(),
            read_deadline: head.read_deadline_after(Duration::from_secs(30)),
        }
    });
    if selected == "diagnostics-custom" {
        app.set_error_response_renderer(|error| {
            let mut response = Response::new(Body::text(error.message()));
            *response.status_mut() = error.status();
            response
        });
    }
}

fn metadata_facts(metadata: &NativeIngressMetadata, headers: &HeaderMap, uri: &Uri) -> Value {
    json!({
        "request_id": metadata.request_id().to_string(),
        "peer": metadata.direct_peer().map(|peer| peer.to_string()),
        "client": metadata.effective_client().map(|client| client.to_string()),
        "host": metadata.effective_host().authority(),
        "scheme": metadata.effective_scheme().as_str(),
        "client_source": host_source(metadata.client_source()),
        "host_source": host_source(metadata.host_source()),
        "scheme_source": host_source(metadata.scheme_source()),
        "forwarding_result": metadata.forwarding_result().as_str(),
        "host_header": headers.get("host").and_then(|value| value.to_str().ok()),
        "uri": uri.to_string(),
        "raw_forwarding_absent": !headers.keys().any(|name| name.as_str() == "forwarded" || name.as_str().starts_with("x-forwarded-")),
    })
}

fn ingress_metadata(ctx: &RequestContext) -> Result<&NativeIngressMetadata, EdgeError> {
    ctx.extensions()
        .get::<NativeIngressMetadata>()
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("fixture ingress metadata missing")))
}

fn host_source(source: CheckedHostSource) -> &'static str {
    match source {
        CheckedHostSource::Direct => "direct",
        CheckedHostSource::TrustedForwarded => "trusted_forwarded",
        CheckedHostSource::TrustedXForwarded => "trusted_x_forwarded",
        CheckedHostSource::Unavailable => "unavailable",
        _ => "unknown",
    }
}

fn egress_outcome(outcome: ResponseEgressOutcome) -> &'static str {
    match outcome {
        ResponseEgressOutcome::ClientDisconnected => "client_disconnected",
        ResponseEgressOutcome::Completed => "completed",
        ResponseEgressOutcome::ConversionError => "conversion_error",
        ResponseEgressOutcome::DeadlineExceeded => "deadline_exceeded",
        ResponseEgressOutcome::HostHandoff => "host_handoff",
        ResponseEgressOutcome::RequestCancelled => "request_cancelled",
        ResponseEgressOutcome::SourceError => "source_error",
        ResponseEgressOutcome::TransportError => "transport_error",
        ResponseEgressOutcome::Unspecified => "unspecified",
        _ => "unknown",
    }
}

fn native_outcome(outcome: NativeRequestOutcome) -> &'static str {
    match outcome {
        NativeRequestOutcome::Egress(egress) => egress_outcome(egress),
        NativeRequestOutcome::Ingress(IngressDisposition::Aborted) => "aborted",
        NativeRequestOutcome::Ingress(IngressDisposition::Draining) => "draining",
        NativeRequestOutcome::Ingress(IngressDisposition::Abandoned) => "abandoned",
    }
}

fn native_failure(failure: Option<NativeRequestFailure>) -> Option<&'static str> {
    failure.map(|category| match category {
        NativeRequestFailure::FrameworkRouting { kind }
        | NativeRequestFailure::PropagatedError { kind } => kind,
        NativeRequestFailure::FallbackBodyExceeded => "fallback_body_exceeded",
        NativeRequestFailure::FallbackReadTimedOut => "fallback_read_timed_out",
        NativeRequestFailure::AdmissionRefused => "admission_refused",
    })
}

fn json_response(value: &Value) -> Result<Response, EdgeError> {
    let mut response = response_with_body(
        StatusCode::OK,
        Body::json(value).map_err(EdgeError::internal)?,
    )?;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    Ok(response)
}

fn snapshot_json(snapshot: NativeDiagnosticsSnapshot) -> Value {
    json!({
        "active_requests": snapshot.active_requests,
        "active_connections": snapshot.active_connections,
        "idle_connections": snapshot.idle_connections,
    })
}

#[expect(
    clippy::expect_used,
    reason = "fixture-owned SIGTERM registration is fatal setup: without that owner this child cannot exercise graceful stop"
)]
#[expect(
    clippy::integer_division_remainder_used,
    reason = "tokio::select uses internal bounded remainder to randomize branch polling; the fixture itself introduces no integer division"
)]
async fn native_stop() {
    #[cfg(unix)]
    {
        let mut terminate = unix_signal(SignalKind::terminate()).expect("fixture SIGTERM owner");
        tokio::select! {
            _interrupted = signal::ctrl_c() => {},
            _terminated = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _interrupted = signal::ctrl_c().await;
    }
}

fn run_native_fixture(selected: &str) -> anyhow::Result<()> {
    PANIC_COMPLETION.store(selected == "diagnostics-panic", Ordering::Relaxed);
    set_hook(Box::new(|_info| println!("FIXTURE_PANIC")));
    log::set_logger(&NATIVE_LOGGER)?;
    log::set_max_level(LevelFilter::Trace);
    println!("APP_LOGGER_INSTALLED");
    let base = NativeDiagnosticsHandle::new();
    // Capture a counter-only clone, not the observer-bearing handle, to avoid a cycle.
    let observed = base
        .clone()
        .with_request_observer(NativeFixtureObserver(base));
    let options = AxumRunOptions::from_env()?.with_diagnostics(observed.clone());
    let state = observed.clone();
    let private_inputs = NativeFailureInputs {
        configuration: env::var("PRIVATE_CONFIG_SENTINEL").unwrap_or_default(),
        secret: env::var("fixture_token").unwrap_or_default(),
    };
    let result = run_app_with_options_and_initializer::<FixtureApp, _, _>(
        options,
        native_stop(),
        move |app, _stores| {
            Box::pin(async move {
                app.insert_state(state);
                app.insert_state(private_inputs);
                println!("INITIALIZED");
                Ok(())
            })
        },
    );
    println!("FINAL_SNAPSHOT {}", snapshot_json(observed.snapshot()));
    result
}

#[action]
async fn buffered_json(Json(value): Json<String>) -> Result<String, EdgeError> {
    println!("JSON_HANDLER_ENTERED");
    Ok(value.len().to_string())
}

#[action]
async fn explicit_json(ctx: RequestContext) -> Result<String, EdgeError> {
    let value: String = ctx.json_within(16 * 1024 * 1024).await?;
    Ok(value.len().to_string())
}

#[action]
async fn small_json(ctx: RequestContext) -> Result<String, EdgeError> {
    let value: String = ctx.json_within(32).await?;
    Ok(value.len().to_string())
}

#[action]
async fn generic_body(mut ctx: RequestContext) -> Result<String, EdgeError> {
    ctx.headers_mut().clear();
    ctx.extensions_mut().clear();
    let request = ctx.into_request()?;
    let reconstructed = RequestContext::new(request, PathParams::default());
    Ok(reconstructed
        .body_bytes(16 * 1024 * 1024)
        .await?
        .len()
        .to_string())
}

#[action]
async fn taken_body(ctx: RequestContext) -> Result<String, EdgeError> {
    use futures::StreamExt as _;
    let mut source = ctx
        .take_body()?
        .into_stream()
        .ok_or_else(|| EdgeError::bad_request("expected stream"))?;
    let mut bytes = 0_usize;
    while let Some(chunk) = source.next().await {
        bytes = bytes
            .checked_add(chunk?.len())
            .ok_or_else(|| EdgeError::bad_request("fixture accounting overflow"))?;
    }
    Ok(bytes.to_string())
}

#[action]
async fn native_metadata(
    ctx: RequestContext,
    ForwardedHost(forwarded_host): ForwardedHost,
) -> Result<Response, EdgeError> {
    let metadata = ingress_metadata(&ctx)?;
    let summary = metadata.received_normalized_head();
    let admission = ADMISSION_FACTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("fixture admission facts missing")))?;
    let middleware = MIDDLEWARE_FACTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("fixture middleware facts missing")))?;
    json_response(&json!({
        "admission": admission,
        "middleware": middleware,
        "handler": metadata_facts(metadata, ctx.headers(), ctx.uri()),
        "forwarded_host": forwarded_host,
        "direct_context_peer": ctx.extensions().get::<AxumRequestContext>().and_then(|context| context.remote_addr).map(|peer| peer.to_string()),
        "normalized": {
            "target_bytes": summary.target_bytes(),
            "header_bytes": summary.header_bytes(),
            "header_count": summary.header_count(),
        },
    }))
}

#[action]
async fn native_error(
    State(inputs): State<NativeFailureInputs>,
) -> Result<&'static str, EdgeError> {
    println!("PRIVATE_INPUTS_USED");
    Err(EdgeError::internal(anyhow::anyhow!(
        "PRIVATE_ERROR_SENTINEL {} {}",
        inputs.configuration,
        inputs.secret,
    )))
}

#[action]
async fn native_upstream(
    State(inputs): State<NativeFailureInputs>,
) -> Result<&'static str, EdgeError> {
    println!("PRIVATE_INPUTS_USED");
    Err(EdgeError::bad_gateway_with_reason(
        format!(
            "PRIVATE_ORIGIN_SENTINEL {} {}",
            inputs.configuration, inputs.secret
        ),
        BadGatewayReason::Transport,
    ))
}

#[action]
async fn native_status503() -> Result<Response, EdgeError> {
    response_with_body(
        StatusCode::SERVICE_UNAVAILABLE,
        Body::text("application-selected 503"),
    )
}

#[action]
async fn native_handler404() -> Result<&'static str, EdgeError> {
    Err(EdgeError::not_found("HANDLER_404_SENTINEL"))
}

#[action]
async fn native_mutated(mut ctx: RequestContext) -> Result<Response, EdgeError> {
    let removed = ctx
        .extensions_mut()
        .remove::<NativeIngressMetadata>()
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("fixture ingress metadata missing")))?;
    let removed_id = removed.request_id().to_string();
    ctx.extensions_mut()
        .insert(CheckedEffectiveHost::from_ingress(
            Some("replacement.example".parse().map_err(EdgeError::internal)?),
            CheckedHostSource::Direct,
        ));
    let ForwardedHost(after_host) = ForwardedHost::from_request(&ctx).await?;
    json_response(&json!({
        "removed_id": removed_id,
        "metadata_absent": ctx.extensions().get::<NativeIngressMetadata>().is_none(),
        "forwarded_host_after": after_host,
    }))
}

#[action]
async fn native_source() -> Result<Body, EdgeError> {
    let guard = Rc::new(WorkGuard("SOURCE_DROPPED"));
    Ok(Body::from_stream(async_stream::stream! {
        let _retained = guard;
        yield Ok(Bytes::from_static(b"source-prefix"));
        sleep(Duration::from_millis(100)).await;
        yield Err(EdgeError::internal(anyhow::anyhow!("SOURCE_SENTINEL")));
    }))
}

#[action]
async fn native_deadline() -> Result<Response, EdgeError> {
    let guard = Rc::new(WorkGuard("DEADLINE_DROPPED"));
    let body = Body::from_stream(async_stream::stream! {
        let _retained = guard;
        yield Ok(Bytes::from_static(b"deadline-prefix"));
        future::pending::<()>().await;
    });
    let mut response = Response::new(body);
    response
        .extensions_mut()
        .insert(ResponseEgressDeadline::after(Duration::from_millis(150)));
    Ok(response)
}

#[action]
async fn native_expired() -> Result<Response, EdgeError> {
    let mut response = Response::new(Body::text("expired-private-body"));
    response
        .extensions_mut()
        .insert(ResponseEgressDeadline::after(Duration::ZERO));
    Ok(response)
}

#[action]
async fn native_conversion() -> Result<Response, EdgeError> {
    let mut response = Response::new(Body::text("CONVERSION_PAYLOAD_SENTINEL"));
    response
        .headers_mut()
        .insert("content-length", HeaderValue::from_static("1"));
    Ok(response)
}

#[action]
async fn native_json_number(Json(value): Json<u64>) -> Result<String, EdgeError> {
    Ok(value.to_string())
}

#[action]
async fn native_counts(
    ctx: RequestContext,
    State(handle): State<NativeDiagnosticsHandle>,
) -> Result<Response, EdgeError> {
    let snapshot = handle.snapshot();
    let phase = match reader(&ctx)?.phase() {
        Some(LifecyclePhase::Starting) => "starting",
        Some(LifecyclePhase::Ready) => "ready",
        Some(LifecyclePhase::Draining) => "draining",
        Some(LifecyclePhase::Stopped) => "stopped",
        None => "unavailable",
    };
    json_response(&json!({
        "active_requests": snapshot.active_requests,
        "active_connections": snapshot.active_connections,
        "idle_connections": snapshot.idle_connections,
        "phase": phase,
    }))
}

fn mode() -> String {
    env::var("FIXTURE_MODE").unwrap_or_else(|_| "initialized".to_owned())
}

fn reader(ctx: &RequestContext) -> Result<LifecycleReader, EdgeError> {
    ctx.extensions()
        .get::<LifecycleReader>()
        .cloned()
        .ok_or_else(|| EdgeError::service_unavailable("fixture reader missing"))
}

#[action]
async fn after() -> Result<&'static str, EdgeError> {
    println!("FORBIDDEN_DISPATCH");
    Ok("forbidden dispatch")
}

#[action]
async fn finite(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    let observed = reader(&ctx)?;
    let _guard = WorkGuard("FINITE_DROPPED");
    println!("DISPATCH");
    while observed.phase() == Some(LifecyclePhase::Ready) {
        sleep(Duration::from_millis(5)).await;
    }
    println!("DRAINING");
    sleep(Duration::from_millis(80)).await;
    Ok("finite-complete")
}

#[action]
async fn pending(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    let observed = reader(&ctx)?;
    let _guard = WorkGuard("WORK_DROPPED");
    println!("DISPATCH");
    let mut reported = false;
    #[expect(
        clippy::infinite_loop,
        reason = "fixture proves process shutdown cancels a never-ending handler"
    )]
    loop {
        if !reported && observed.phase() == Some(LifecyclePhase::Draining) {
            println!("DRAINING");
            reported = true;
        }
        sleep(Duration::from_millis(5)).await;
    }
}

#[action]
async fn endless_stream(ctx: RequestContext) -> Result<Body, EdgeError> {
    let observed = reader(&ctx)?;
    let guard = Rc::new(WorkGuard("STREAM_DROPPED"));
    println!("STREAM_READY");
    Ok(Body::from_stream(async_stream::stream! {
        let _retained = guard;
        yield Ok(Bytes::from_static(b"stream-prefix"));
        let mut reported = false;
        loop {
            if !reported && observed.phase() == Some(LifecyclePhase::Draining) {
                println!("DRAINING");
                reported = true;
            }
            sleep(Duration::from_millis(5)).await;
        }
    }))
}

#[action]
async fn flood() -> Result<Body, EdgeError> {
    static BLOCK: [u8; 0x0001_0000] = [b'x'; 0x0001_0000];
    let guard = Rc::new(WorkGuard("STREAM_DROPPED"));
    println!("STREAM_READY");
    Ok(Body::from_stream(stream::repeat_with(move || {
        let _retained = &guard;
        Ok(Bytes::from_static(&BLOCK))
    })))
}

fn kv(ctx: &RequestContext, id: &str) -> Result<KvHandle, EdgeError> {
    ctx.kv_store(id)
        .ok_or_else(|| EdgeError::service_unavailable("fixture KV missing"))
}

#[action]
async fn write(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    kv(&ctx, "first")?
        .put("persisted", &"durable-value")
        .await?;
    Ok("written")
}

#[action]
async fn read(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "first")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn alias(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "second")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn distinct(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "third")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn snapshot(ctx: RequestContext) -> Result<String, EdgeError> {
    let binding = ctx
        .config_store_default_binding()
        .ok_or_else(|| EdgeError::service_unavailable("fixture config missing"))?;
    Ok(binding
        .handle
        .get(&binding.default_key)
        .await?
        .unwrap_or_default())
}

#[action]
async fn stop(State(state): State<Arc<StopState>>) -> Result<&'static str, EdgeError> {
    if let Some(sender) = state
        .0
        .lock()
        .map_err(|_error| EdgeError::internal(anyhow::anyhow!("fixture stop lock poisoned")))?
        .take()
    {
        let _sent = sender.send(());
    }
    Ok("stopping")
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "test-only executable asserts initializer ownership and call counts"
)]
fn main() -> anyhow::Result<()> {
    let selected = mode();
    if selected.starts_with("diagnostics") {
        return run_native_fixture(&selected);
    }
    if matches!(selected.as_str(), "development" | "bare-dev") {
        return run_app::<FixtureApp>();
    }
    if matches!(selected.as_str(), "framework" | "bare" | "hook-error") {
        return run_production_app::<FixtureApp>();
    }
    if selected == "starting" {
        return run_production_app_with_initializer::<FixtureApp, _>(|_app, _stores| {
            Box::pin(async {
                let _guard = WorkGuard("STARTUP_DROPPED");
                println!("INITIALIZING");
                future::pending::<Result<(), EdgeError>>().await
            })
        });
    }
    if selected == "initializer-error" {
        return run_production_app_with_initializer::<FixtureApp, _>(|_app, _stores| {
            Box::pin(async { Err(EdgeError::internal(anyhow::anyhow!("CALLBACK_SENTINEL"))) })
        });
    }
    let calls = Rc::new(Cell::new(0_u32));
    let initialized = Rc::clone(&calls);
    if selected.starts_with("embedding") {
        let address: SocketAddr = env::var("FIXTURE_BIND")?.parse()?;
        let mut options = AxumRunOptions::new(address)?;
        if let Ok(raw) = env::var("FIXTURE_JSON_LIMIT") {
            options = options.with_json_body_limit_bytes(raw.parse()?)?;
        }
        if selected == "embedding" {
            options = options
                .with_config_dir(env::var("FIXTURE_CONFIG_ROOT")?)?
                .with_data_dir(env::var("FIXTURE_DATA_ROOT")?)?;
        }
        let (sender, receiver) = oneshot::channel();
        let stop_capture = Rc::clone(&calls);
        return run_app_with_options_and_initializer::<FixtureApp, _, _>(
            options,
            async move {
                let _closed = receiver.await;
                assert_eq!(
                    stop_capture.get(),
                    1,
                    "initializer ran once before caller stop"
                );
            },
            move |app, stores| {
                Box::pin(async move {
                    if let Some(binding) =
                        stores.config().and_then(|registry| registry.default_ref())
                    {
                        let config = AppConfig::<fixture::FixtureConfig>::from_binding(
                            binding,
                            None,
                            stores.secrets(),
                            app.config_extraction_limits(),
                            app.monotonic_clock(),
                        )
                        .await?;
                        app.insert_state(Arc::new(fixture::HostState {
                            greeting: config.greeting,
                        }));
                    }
                    app.insert_state(Arc::new(StopState(Mutex::new(Some(sender)))));
                    initialized.set(initialized.get().saturating_add(1));
                    println!("INITIALIZED");
                    Ok(())
                })
            },
        );
    }
    run_production_app_with_initializer::<FixtureApp, _>(move |app, stores| {
        Box::pin(async move {
            let binding = stores
                .config()
                .and_then(|registry| registry.default_ref())
                .ok_or_else(|| {
                    EdgeError::service_unavailable("required fixture binding missing")
                })?;
            let config = AppConfig::<fixture::FixtureConfig>::from_binding(
                binding,
                None,
                stores.secrets(),
                app.config_extraction_limits(),
                app.monotonic_clock(),
            )
            .await?;
            app.insert_state(Arc::new(fixture::HostState {
                greeting: config.greeting,
            }));
            initialized.set(initialized.get().saturating_add(1));
            assert_eq!(initialized.get(), 1, "initializer runs exactly once");
            println!("INITIALIZED");
            Ok(())
        })
    })
}
