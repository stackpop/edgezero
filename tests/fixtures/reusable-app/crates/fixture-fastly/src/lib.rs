use edgezero_adapter_fastly::{
    FastlyLogging, Serve, ServeSummary, init_logger, runtime_env_config,
};
use edgezero_core::app::{App, Hooks};
use edgezero_core::body::Body;
use edgezero_core::http::Extensions;
use fastly::{Error, Request, Response};
use fixture_core::{FixtureApp, Observation, observe};
use futures::{StreamExt, executor::block_on};
use std::io::Write;
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
pub fn extend(req: &Request, extensions: &mut Extensions) {
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
pub fn rebuilt() -> Result<(), Error> {
    let mut initialized = false;
    finish(serving().run(move |req| -> Result<Response, Error> {
        let stores = MeasuredApp::stores();
        let env = runtime_env_config(stores);
        if !initialized {
            let logging = FastlyLogging::from(&env);
            if logging.use_fastly_logger {
                init_logger(
                    logging.endpoint.as_deref().unwrap(),
                    logging.level,
                    logging.echo_stdout,
                )?;
            }
            initialized = true;
        }
        let app = MeasuredApp::build_app();
        edgezero_adapter_fastly::request::dispatch_with_registries(&app, req, stores, &env, extend)
    }))
}

fn custom_dispatch(
    mut req: Request,
    sandbox: &mut edgezero_adapter_fastly::lifecycle::Sandbox<App>,
    reuse_app: bool,
) -> Result<(), Error> {
    let token = req
        .get_header_str("x-request-token")
        .unwrap_or("missing")
        .to_owned();
    let observation = observation(&req);
    assert_eq!(sandbox.requests(), observation.ordinal as u64);
    if req.get_path() == "/panic" {
        panic!("injected callback panic");
    }
    if req.get_path() == "/health" {
        Response::from_status(200)
            .with_header(
                "x-fixture-attempts",
                sandbox.initialization_attempts().to_string(),
            )
            .with_body_json(&serde_json::json!({"instance":observation.instance,
                "ordinal":observation.ordinal,"retained":sandbox.state().is_some()}))?
            .send_to_client();
        return Ok(());
    }
    let post_send = req
        .get_header_str("x-fixture-backend")
        .map(|uri| uri.replace("/chunks", "/post-send"));
    req.set_header("x-fixture-mutated", "true");
    if req.get_path() == "/constructor-panic" {
        PANIC_IN_CONSTRUCTOR.store(true, Ordering::SeqCst);
    }
    // Arm B deliberately scopes application state to the current callback.
    // The panic fixture also needs a fresh constructor even after initialization.
    let mut per_request = edgezero_adapter_fastly::lifecycle::Sandbox::default();
    let state = if reuse_app && req.get_path() != "/constructor-panic" {
        sandbox
    } else {
        &mut per_request
    };
    let result = state.initialize(|| {
        if req.get_path() == "/initialization-error" {
            Err(Error::msg("injected initialization failure"))
        } else {
            Ok(MeasuredApp::build_app())
        }
    });
    if result.is_err() {
        Response::from_status(503)
            .with_header(
                "x-fixture-attempts",
                state.initialization_attempts().to_string(),
            )
            .with_body_json(&serde_json::json!({"instance":observation.instance,
                        "ordinal":observation.ordinal,"retained":state.state().is_some()}))?
            .send_to_client();
        return Ok(());
    }
    initialized(&observation, &token);
    let mut core = edgezero_adapter_fastly::request::into_core_request(req)?;
    core.extensions_mut().insert(observation.clone());
    let proxy = core
        .extensions()
        .get::<edgezero_core::proxy::ProxyHandle>()
        .cloned();
    let mut response = block_on(state.state().unwrap().router().oneshot(core))?;
    if let Some(fixture_core::Finalize(value)) =
        response.extensions_mut().remove::<fixture_core::Finalize>()
    {
        response
            .headers_mut()
            .append("x-fixture-finalized", value.parse().unwrap());
    }
    let (parts, body) = response.into_parts();
    let mut native = Response::from_status(parts.status.as_u16());
    native.set_header(
        "x-fixture-attempts",
        state.initialization_attempts().to_string(),
    );
    for (name, value) in &parts.headers {
        native.append_header(name.as_str(), value.as_bytes());
    }
    match body {
        Body::Once(bytes) => {
            native.set_body(bytes.to_vec());
            native.send_to_client();
            lifecycle("response_committed", &observation, &token);
        }
        Body::Stream(mut stream) => {
            let mut writer = native.stream_to_client();
            lifecycle("response_committed", &observation, &token);
            while let Some(chunk) = block_on(stream.next()) {
                match chunk {
                    Ok(bytes) => {
                        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                            println!(
                                "{}",
                                serde_json::json!({"event":"post_commit_error","instance":observation.instance,"ordinal":observation.ordinal,"token":token,"error":error.to_string()})
                            );
                            drop(writer);
                            lifecycle("guest_completed", &observation, &token);
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        println!(
                            "{}",
                            serde_json::json!({"event":"post_commit_error","instance":observation.instance,"ordinal":observation.ordinal,"token":token,"error":error.to_string()})
                        );
                        drop(writer);
                        lifecycle("guest_completed", &observation, &token);
                        return Ok(());
                    }
                }
            }
            if let Err(error) = writer.finish() {
                println!(
                    "{}",
                    serde_json::json!({"event":"post_commit_error","instance":observation.instance,"ordinal":observation.ordinal,"token":token,"error":error.to_string()})
                );
            }
        }
    }
    if let (Some(uri), Some(proxy)) = (post_send, proxy) {
        let Ok(uri) = uri.parse::<edgezero_core::http::Uri>() else {
            return Ok(());
        };
        if uri.host() == Some("127.0.0.1") && uri.scheme_str() == Some("http") {
            let outcome = block_on(proxy.forward(edgezero_core::proxy::ProxyRequest::new(
                edgezero_core::http::Method::GET,
                uri,
            )));
            // All failures here are after commitment: record them without a second send.
            match outcome {
                Ok(response) => {
                    if let Body::Stream(mut stream) = response.into_body() {
                        while let Some(chunk) = block_on(stream.next()) {
                            if let Err(error) = chunk {
                                println!("post-send body failure: {error}");
                                break;
                            }
                        }
                    }
                }
                Err(error) => println!("post-send failure: {error}"),
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"event":"guest_completed", "instance":observation.instance,"token":token,
        "ordinal":observation.ordinal,"cpu_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),
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
    fn build_app() -> App {
        if PANIC_IN_CONSTRUCTOR.swap(false, Ordering::SeqCst) {
            println!(
                "{}",
                serde_json::json!({"event":"constructor_panic","instance":fixture_core::instance_id(),"source":"injected_inside_build_app"})
            );
            panic!("injected failure inside MeasuredApp::build_app");
        }
        let cpu_before = fastly::compute_runtime::elapsed_vcpu_ms().ok();
        let heap_before = fastly::compute_runtime::heap_memory_snapshot_mib().ok();
        let start = std::time::Instant::now();
        let rounds: usize = option_env!("FIXTURE_BUILD_ROUNDS")
            .unwrap_or("0")
            .parse()
            .unwrap();
        for _ in 0..rounds {
            let parsed: serde_json::Value =
                serde_json::from_str(r#"{"routes":["one","two"],"cache":{"capacity":128}}"#)
                    .unwrap();
            std::hint::black_box(parsed);
        }
        let app = FixtureApp::build_app();
        INITIALIZATIONS.lock().unwrap().push(serde_json::json!({"event":"initialization", "wall_ns":start.elapsed().as_nanos(),
            "cpu_before_ms":cpu_before,"cpu_after_ms":fastly::compute_runtime::elapsed_vcpu_ms().ok(),
            "heap_before_mib":heap_before,"heap_after_mib":fastly::compute_runtime::heap_memory_snapshot_mib().ok(),
            "construction_rounds":rounds}));

        app
    }
}

/// Controlled fixture inputs exercise real adapter boundaries, not provider outages.
pub fn faults() -> Result<(), Error> {
    use edgezero_core::env_config::EnvConfig;
    use edgezero_core::http::response_builder;
    let mut retained = None;
    finish(serving().run(move |mut req: Request| -> Result<Response, Error> {
        let observation = observation(&req);
        let token = req.get_header_str("x-request-token").unwrap_or("missing").to_owned();
        let path = req.get_path().to_owned();
        if path == "/constructor-panic" {
            PANIC_IN_CONSTRUCTOR.store(true, Ordering::SeqCst);
            retained = None;
        }
        let app = retained.get_or_insert_with(MeasuredApp::build_app);
        initialized(&observation, &token);
        let result = match path.as_str() {
            "/collect-error" => {
                let body = Body::from_stream(futures::stream::iter([
                    Err(std::io::Error::other("injected core stream failure"))
                ]));
                edgezero_adapter_fastly::response::from_core_response(response_builder().body(body).unwrap()).map_err(Error::from)
            }
            "/inbound-error" => {
                let mut upstream = Request::get("http://fault-origin/abort").send("fault-origin")?;
                req.set_body(upstream.take_body());
                edgezero_adapter_fastly::request::into_core_request(req)
                    .map(|_| Response::from_status(200)).map_err(Error::from)
            }
            _ => {
                let env = if path == "/recoverable-store-error" || path == "/terminal-store-error" {
                    EnvConfig::from_vars([("EDGEZERO__STORES__KV__FIXTURE_KV__NAME", "missing_fixture_store")])
                } else { runtime_env_config(MeasuredApp::stores()) };
                edgezero_adapter_fastly::request::dispatch_with_registries(app, req, MeasuredApp::stores(), &env, |_, extensions| { extensions.insert(observation.clone()); })
            }
        };
        match result {
            Ok(response) => { lifecycle("conversion_completed", &observation, &token); Ok(response) }
            Err(error) => {
                println!("{}", serde_json::json!({"event":"adapter_error","instance":observation.instance,"ordinal":observation.ordinal,"token":token,"path":path,"error":error.to_string(),"source":"controlled_fixture_input"}));
                if path == "/recoverable-store-error" {
                    // Explicit custom policy permits another callback; standard helpers propagate.
                    Ok(Response::from_status(503).with_body("injected selector failed"))
                } else {
                    lifecycle("terminal_error", &observation, &token);
                    Err(error)
                }
            }
        }
    }))
}
