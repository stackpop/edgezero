use edgezero_adapter_fastly::lifecycle::{Sandbox, serve_custom};
use edgezero_adapter_fastly::outbound::FastlyOutboundClient;
use edgezero_adapter_fastly::request::send_request_with_registries_and_hooks;
use edgezero_adapter_fastly::{
    FASTLY_PLATFORM, FastlyLogging, Serve, ServeSummary, init_logger, runtime_env_config,
};
use edgezero_core::app::{App, Hooks};
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Extensions, Response as CoreResponse, StatusCode, Uri};
use edgezero_core::outbound::{HttpClient, OutboundRequest};
use edgezero_core::{
    AdmissionDecision, DEFAULT_INBOUND_READ_BUDGET, IngressGrant, ResponseEgressCompletion,
};
use fastly::{Error, Request, Response};
use fixture_core::{FixtureApp, Observation, observe};
use futures::executor::block_on;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
static INITIALIZATIONS: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());
static PANIC_IN_CONSTRUCTOR: AtomicBool = AtomicBool::new(false);

fn lifecycle(event: &str, observation: &Observation, token: &str) {
    println!(
        "{}",
        serde_json::json!({"event":event,"instance":observation.instance,"ordinal":observation.ordinal,"token":token,"cpu_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),"heap_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok()})
    );
}
fn initialized(observation: &Observation, token: &str) {
    for mut event in INITIALIZATIONS.lock().unwrap().drain(..) {
        event["instance"] = observation.instance.clone().into();
        event["ordinal"] = observation.ordinal.into();
        event["token"] = token.into();
        println!("{event}");
    }
}
struct ConversionCompletion {
    armed: Arc<AtomicBool>,
    observation: Observation,
    token: String,
}
impl Drop for ConversionCompletion {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            lifecycle("conversion_completed", &self.observation, &self.token);
        }
    }
}

pub fn serving() -> Serve {
    Serve::new()
        .with_max_requests(
            option_env!("FIXTURE_MAX_REQUESTS")
                .unwrap_or("10")
                .parse()
                .unwrap(),
        )
        .with_timeout(Duration::from_millis(500))
}
pub fn observation(req: &Request) -> Observation {
    let token = req.get_header_str("x-request-token").unwrap_or("missing");
    let value = observe(token, req.get_client_request_id());
    println!(
        "{}",
        serde_json::json!({"event":"request_start", "instance":value.instance,"token":token,
        "service_id":fastly::compute_runtime::service_id(),"ordinal":value.ordinal,"correlation":value.correlation,"native_id":value.native_id,
        "cpu_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),
        "heap_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok()})
    );
    value
}
pub fn extend(req: &mut Request, extensions: &mut Extensions) {
    let value = observation(req);
    let token = req.get_header_str("x-request-token").unwrap_or("missing");
    initialized(&value, token);
    let armed = Arc::new(AtomicBool::new(false));
    extensions.insert(fixture_core::ResponseLifetime(
        Arc::new(ConversionCompletion {
            armed: armed.clone(),
            observation: value.clone(),
            token: token.to_owned(),
        }),
        armed,
    ));
    log::info!("fixture request {}", value.correlation);
    extensions.insert(value);
}
pub fn finish(summary: ServeSummary<Error>) -> Result<(), Error> {
    println!(
        "{}",
        serde_json::json!({"event":"sdk_summary", "instance":fixture_core::instance_id(), "attempted":summary.requests(),
        "handler_wall_ns":summary.time_handler().as_nanos(), "wait_wall_ns":summary.time_waited().as_nanos()})
    );
    summary.into_result()
}
fn install_logger(env: &edgezero_core::env_config::EnvConfig) -> Result<(), Error> {
    let logging = FastlyLogging::from(env);
    if logging.use_fastly_logger {
        init_logger(
            logging.endpoint.as_deref().unwrap(),
            logging.level,
            logging.echo_stdout,
        )?;
    }
    Ok(())
}
fn post_commit_error(obs: &Observation, token: &str, error: &dyn std::fmt::Display) {
    println!(
        "{}",
        serde_json::json!({"event":"post_commit_error","instance":obs.instance,"ordinal":obs.ordinal,"token":token,"error":error.to_string()})
    );
}
pub fn rebuilt() -> Result<(), Error> {
    finish(serve_custom(serving(), |req, setup: &mut Sandbox<()>| {
        let stores = MeasuredApp::stores();
        let mut runtime = runtime_env_config(stores);
        setup.setup_once(|| install_logger(&runtime.env))?;
        runtime.emit_boot_diagnostics();
        let app = App::build::<MeasuredApp>(FASTLY_PLATFORM)?;
        send_request_with_registries_and_hooks(
            &app,
            stores,
            req,
            &runtime.env,
            extend,
            |_response| (),
        )
        .map(|_state| ())
    }))
}

pub fn logger_negative() -> Result<(), Error> {
    finish(serve_custom(serving(), |req, _state: &mut Sandbox<()>| {
        let obs = observation(&req);
        let stores = MeasuredApp::stores();
        let mut runtime = runtime_env_config(stores);
        if let Err(error) = install_logger(&runtime.env) {
            Response::from_status(500)
                .with_body(error.to_string())
                .send_to_client();
            return Err(error);
        }
        runtime.emit_boot_diagnostics();
        let app = App::build::<MeasuredApp>(FASTLY_PLATFORM)?;
        initialized(
            &obs,
            req.get_header_str("x-request-token").unwrap_or("missing"),
        );
        send_request_with_registries_and_hooks(
            &app,
            stores,
            req,
            &runtime.env,
            |_, extensions| {
                extensions.insert(obs);
            },
            |_response| (),
        )
        .map(|_state| ())
    }))
}

fn finalize_response(response: &mut CoreResponse, attempts: u64) {
    if let Some(fixture_core::Finalize(value)) =
        response.extensions_mut().remove::<fixture_core::Finalize>()
    {
        response
            .headers_mut()
            .append("x-fixture-finalized", value.parse().unwrap());
    }
    response
        .headers_mut()
        .insert("x-fixture-attempts", attempts.to_string().parse().unwrap());
}

fn custom_dispatch(req: Request, sandbox: &mut Sandbox<App>, reuse_app: bool) -> Result<(), Error> {
    let token = req
        .get_header_str("x-request-token")
        .unwrap_or("missing")
        .to_owned();
    let obs = observation(&req);
    assert_eq!(sandbox.requests(), obs.ordinal as u64);
    if req.get_path() == "/panic" {
        panic!("injected callback panic");
    }
    if req.get_path() == "/health" {
        Response::from_status(200)
            .with_header(
                "x-fixture-attempts",
                sandbox.initialization_attempts().to_string(),
            )
            .with_body_json(&serde_json::json!({"instance":obs.instance,
                "ordinal":obs.ordinal,"retained":sandbox.state().is_some()}))?
            .send_to_client();
        return Ok(());
    }
    let stores = MeasuredApp::stores();
    let mut runtime = runtime_env_config(stores);
    if let Err(error) = sandbox.setup_once(|| install_logger(&runtime.env)) {
        eprintln!("logger setup failed: {error}");
        Response::from_status(503).send_to_client();
        return Ok(());
    }
    runtime.emit_boot_diagnostics();
    log::info!("fixture request {}", obs.correlation);
    // Only the streaming route schedules post-send work.
    let post_send = req
        .get_header_str("x-fixture-backend")
        .filter(|uri| uri.contains("/chunks"))
        .map(|uri| uri.replace("/chunks", "/post-send"));
    // Per-request mode scopes application state to the current callback.
    let mut per_request = Sandbox::default();
    let state = if reuse_app { sandbox } else { &mut per_request };
    let result = state.initialize(|| {
        if req.get_path() == "/initialization-error" {
            Err(Error::msg("injected initialization failure"))
        } else {
            App::build::<MeasuredApp>(FASTLY_PLATFORM).map_err(Error::from)
        }
    });
    if result.is_err() {
        Response::from_status(503)
            .with_header(
                "x-fixture-attempts",
                state.initialization_attempts().to_string(),
            )
            .with_body_json(&serde_json::json!({"instance":obs.instance,
                        "ordinal":obs.ordinal,"retained":state.state().is_some()}))?
            .send_to_client();
        return Ok(());
    }
    initialized(&obs, &token);
    let app = state.state().unwrap();
    let attempts = state.initialization_attempts();
    let delivered = send_request_with_registries_and_hooks(
        app,
        stores,
        req,
        &runtime.env,
        |raw, extensions| {
            raw.set_header("x-fixture-mutated", "true");
            extensions.insert(obs.clone());
        },
        |response| finalize_response(response, attempts),
    );
    match delivered {
        Err(error) => {
            post_commit_error(&obs, &token, &error);
            lifecycle("guest_completed", &obs, &token);
            return Ok(());
        }
        Ok(None) => {
            lifecycle("guest_completed", &obs, &token);
            return Ok(());
        }
        Ok(Some(())) => lifecycle("delivery_completed", &obs, &token),
    }
    if let Some(uri) = post_send {
        let outcome = block_on(async {
            let uri: Uri = uri.parse().map_err(EdgeError::internal)?;
            if !fixture_core::is_loopback_http(&uri) {
                return Err(EdgeError::bad_request("backend must be loopback HTTP"));
            }
            let client =
                HttpClient::with_client(FastlyOutboundClient::with_clock(app.monotonic_clock()));
            client
                .send(OutboundRequest::get(uri.to_string())?.stream_response())
                .await?
                .into_bytes_bounded(1024 * 1024)
                .await?;
            Ok::<(), EdgeError>(())
        });
        if let Err(error) = outcome {
            post_commit_error(&obs, &token, &error);
        }
    }
    println!(
        "{}",
        serde_json::json!({"event":"guest_completed", "instance":obs.instance,"token":token,
        "ordinal":obs.ordinal,"cpu_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),
        "heap_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok()})
    );
    Ok(())
}
pub fn custom(reuse_sandbox: bool, reuse_app: bool) -> Result<(), Error> {
    if reuse_sandbox {
        finish(edgezero_adapter_fastly::lifecycle::serve_custom(
            serving(),
            move |req, state| custom_dispatch(req, state, reuse_app),
        ))
    } else {
        fastly::init();
        edgezero_adapter_fastly::lifecycle::run_custom(Request::from_client(), |req, state| {
            custom_dispatch(req, state, false)
        })
    }
}

pub struct MeasuredApp;
impl Hooks for MeasuredApp {
    fn routes() -> edgezero_core::router::RouterService {
        FixtureApp::routes()
    }
    fn stores() -> edgezero_core::app::StoresMetadata {
        FixtureApp::stores()
    }
    fn configure(app: &mut App) -> Result<(), EdgeError> {
        if PANIC_IN_CONSTRUCTOR.swap(false, Ordering::SeqCst) {
            println!(
                "{}",
                serde_json::json!({"event":"constructor_panic","instance":fixture_core::instance_id(),"source":"injected_inside_configure"})
            );
            panic!("injected failure inside MeasuredApp::configure");
        }
        let cpu_before = fastly::compute_runtime::elapsed_vcpu_ms().ok();
        let heap_before = fastly::compute_runtime::heap_memory_snapshot_mib().ok();
        let start = std::time::Instant::now();
        let rounds: usize = option_env!("FIXTURE_BUILD_ROUNDS")
            .unwrap_or("0")
            .parse()
            .unwrap();
        let source =
            serde_json::json!({"routes": ["one", "two"], "cache": {"capacity": 128}}).to_string();
        for _ in 0..rounds {
            let parsed: serde_json::Value = serde_json::from_str(&source).unwrap();
            std::hint::black_box(parsed);
        }
        FixtureApp::configure(app)?;
        app.set_ingress_admission_policy(|head| {
            let token = head
                .headers()
                .get("x-request-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing")
                .to_owned();
            AdmissionDecision::Admit {
                completion: ResponseEgressCompletion::new(move |report| {
                    println!(
                        "{}",
                        serde_json::json!({"event":"egress_terminal","instance":fixture_core::instance_id(),
                            "token":token,"outcome":format!("{:?}",report.outcome),
                            "body_kind":format!("{:?}",report.body_kind),"bytes_written":report.bytes_written,
                            "fallback":report.fallback_disposition.map(|value|format!("{value:?}"))})
                    );
                }),
                grant: IngressGrant::empty(),
                read_deadline: head.read_deadline_after(DEFAULT_INBOUND_READ_BUDGET),
            }
        });
        INITIALIZATIONS.lock().unwrap().push(serde_json::json!({"event":"initialization", "wall_ns":start.elapsed().as_nanos(),
            "cpu_before_ms":cpu_before,"cpu_after_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),
            "heap_before_mib":heap_before,"heap_after_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok(),
            "construction_rounds":rounds}));

        Ok(())
    }
}

/// Controlled fixture inputs exercise real adapter boundaries, not provider outages.
pub fn faults() -> Result<(), Error> {
    use edgezero_core::env_config::EnvConfig;
    finish(serve_custom(
        serving(),
        |mut req: Request, retained: &mut Sandbox<App>| {
            let obs = observation(&req);
            let token = req
                .get_header_str("x-request-token")
                .unwrap_or("missing")
                .to_owned();
            let path = req.get_path().to_owned();
            if path == "/constructor-panic" {
                PANIC_IN_CONSTRUCTOR.store(true, Ordering::SeqCst);
            }
            retained.initialize(|| App::build::<MeasuredApp>(FASTLY_PLATFORM))?;
            let app = retained.state().unwrap();
            initialized(&obs, &token);
            if path == "/inbound-error" {
                let mut upstream =
                    Request::get("http://fault-origin/abort").send("fault-origin")?;
                req.set_body(upstream.take_body());
            }
            let stores = MeasuredApp::stores();
            let mut runtime = runtime_env_config(stores);
            runtime.emit_boot_diagnostics();
            let env = if path == "/recoverable-store-error" || path == "/terminal-store-error" {
                EnvConfig::from_vars([(
                    "EDGEZERO__STORES__KV__FIXTURE_KV__NAME",
                    "missing_fixture_store",
                )])
            } else {
                runtime.env
            };
            let result = send_request_with_registries_and_hooks(
                app,
                stores,
                req,
                &env,
                |raw, extensions| {
                    if path == "/collect-error" || path == "/inbound-error" {
                        raw.set_url(format!("http://example.com/probe{}", path));
                    }
                    extensions.insert(obs.clone());
                },
                |response| {
                    if path == "/collect-error" {
                        *response.status_mut() = StatusCode::OK;
                        *response.body_mut() = Body::from_stream(futures::stream::once(async {
                            Err(EdgeError::internal(std::io::Error::other(
                                "injected core stream failure",
                            )))
                        }));
                    }
                    response.status()
                },
            );
            match result {
                Ok(status) => {
                    if path == "/inbound-error" {
                        assert_eq!(status, Some(StatusCode::INTERNAL_SERVER_ERROR));
                        lifecycle("ingress_error_response", &obs, &token);
                    }
                    lifecycle("delivery_completed", &obs, &token);
                    Ok(())
                }
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::json!({"event":"adapter_error","instance":obs.instance,"ordinal":obs.ordinal,"token":token,"path":path,"error":error.to_string(),"source":"controlled_fixture_input"})
                    );
                    if path == "/recoverable-store-error" {
                        // Explicit custom policy permits another callback; standard helpers propagate.
                        Response::from_status(503)
                            .with_body("injected selector failed")
                            .send_to_client();
                        Ok(())
                    } else {
                        if path == "/terminal-store-error" {
                            Response::from_status(500)
                                .with_body("injected selector failed")
                                .send_to_client();
                        } else if path == "/collect-error" {
                            post_commit_error(&obs, &token, &error);
                        }
                        lifecycle("terminal_error", &obs, &token);
                        Err(error)
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::http::Response as CoreResponse;

    #[test]
    fn finalization_preserves_streaming_body_and_request_local_headers() {
        for (attempts, token) in [(1, "first"), (2, "second")] {
            let mut response = CoreResponse::new(Body::stream(futures::stream::once(async {
                Body::from("chunk").into_bytes().unwrap()
            })));
            response
                .extensions_mut()
                .insert(fixture_core::Finalize(token.to_owned()));

            finalize_response(&mut response, attempts);

            assert_eq!(response.headers()["x-fixture-finalized"], token);
            assert_eq!(
                response.headers()["x-fixture-attempts"],
                attempts.to_string()
            );
            assert!(
                response
                    .extensions()
                    .get::<fixture_core::Finalize>()
                    .is_none()
            );
            assert!(matches!(response.body(), Body::Stream(_)));
        }
    }
}
