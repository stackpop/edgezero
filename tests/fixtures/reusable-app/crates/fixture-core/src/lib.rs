use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::http::{Method, Response, response_builder};
use edgezero_core::outbound::OutboundRequest;
use edgezero_core::router::RouterService;
use edgezero_core::{action, body::Body, context::RequestContext, error::EdgeError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

static INSTANCE: OnceLock<String> = OnceLock::new();
static BUILDS: AtomicUsize = AtomicUsize::new(0);
static ORDINAL: AtomicUsize = AtomicUsize::new(0);
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub fn instance_id() -> Option<&'static str> {
    INSTANCE.get().map(String::as_str)
}

pub fn retained_app<A: Hooks>(
    cell: &OnceLock<App>,
    platform: edgezero_core::PlatformMetadata,
) -> Result<&App, EdgeError> {
    if let Some(app) = cell.get() {
        return Ok(app);
    }
    let app = App::build::<A>(platform)?;
    // Keep an already-installed winner; configuration errors never enter the cell.
    let _ = cell.set(app);
    cell.get()
        .ok_or_else(|| EdgeError::internal(std::io::Error::other("application retention failed")))
}

/// Carry a fixture-owned lifetime observer through request and response conversion.
#[derive(Clone)]
pub struct ResponseLifetime(
    pub Arc<dyn Send + Sync>,
    pub Arc<std::sync::atomic::AtomicBool>,
);

/// Request-specific finalization data produced inside the router.
#[derive(Clone)]
pub struct Finalize(pub String);

#[derive(Clone)]
pub struct Observation {
    pub instance: String,
    pub ordinal: usize,
    pub correlation: String,
    pub native_id: bool,
}

pub fn observe(candidate: &str, correlation: Option<&str>) -> Observation {
    Observation {
        instance: INSTANCE.get_or_init(|| candidate.to_owned()).clone(),
        ordinal: ORDINAL.fetch_add(1, Ordering::SeqCst) + 1,
        correlation: correlation.unwrap_or(candidate).to_owned(),
        native_id: correlation.is_some(),
    }
}

pub struct FixtureApp;
pub struct OtherApp;

impl Hooks for FixtureApp {
    fn stores() -> StoresMetadata {
        StoresMetadata {
            config: Some(StoreMetadata {
                default: "fixture_config",
                ids: &["fixture_config"],
            }),
            kv: Some(StoreMetadata {
                default: "fixture_kv",
                ids: &["fixture_kv"],
            }),
            secrets: Some(StoreMetadata {
                default: "fixture_secrets",
                ids: &["fixture_secrets"],
            }),
        }
    }
    fn configure(app: &mut App) -> Result<(), EdgeError> {
        BUILDS.fetch_add(1, Ordering::SeqCst);
        app.set_name("fixture");
        Ok(())
    }
    fn routes() -> RouterService {
        RouterService::builder()
            .middleware(ObserveRequest)
            .with_state(Arc::new(AtomicUsize::new(0)))
            .get("/probe/{id}", probe)
            .post("/probe/{id}", probe)
            .get("/bindings", bindings)
            .get("/cookies", cookies)
            .get("/rendered-error", rendered_error)
            .get("/stream", backend)
            .get("/stream-error", backend)
            .get("/overlap/{id}", overlap)
            .get("/origin/{id}", backend)
            .build()
    }
}
/// Swaps only the KV binding metadata to inject a required-binding failure.
///
/// Build and dispatch this app independently. Its default configuration leaves
/// the healthy fixture's build counter unchanged.
pub struct MissingBindingApp;
impl Hooks for MissingBindingApp {
    fn routes() -> RouterService {
        FixtureApp::routes()
    }
    fn stores() -> StoresMetadata {
        let mut stores = FixtureApp::stores();
        stores.kv = Some(StoreMetadata {
            default: "missing_fixture_kv",
            ids: &["missing_fixture_kv"],
        });
        stores
    }
}
impl Hooks for OtherApp {
    fn routes() -> RouterService {
        RouterService::builder().get("/other", other).build()
    }
}

#[action]
async fn other(_ctx: RequestContext) -> Result<String, EdgeError> {
    Ok("other-app".into())
}

async fn record(ctx: &RequestContext) -> Result<serde_json::Value, EdgeError> {
    let body = ctx.body_bytes(MAX_BODY_BYTES).await?;
    let token = ctx
        .headers()
        .get("x-request-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing");
    let observation = ctx
        .extensions()
        .get::<Observation>()
        .cloned()
        .ok_or_else(|| {
            EdgeError::internal(std::io::Error::other("observation middleware absent"))
        })?;
    let shared = ctx
        .extensions()
        .get::<Arc<AtomicUsize>>()
        .unwrap()
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    Ok(serde_json::json!({
        "instance": observation.instance, "ordinal": observation.ordinal,
        "correlation": observation.correlation, "native_id": observation.native_id,
        "builds": BUILDS.load(Ordering::SeqCst),
        "path": ctx.path_params().get("id"), "token": token,
        "mutated": ctx.headers().get("x-fixture-mutated").is_some(),
        "body": String::from_utf8_lossy(&body),
        "shared": shared, "inflight": INFLIGHT.load(Ordering::SeqCst),
    }))
}

#[action]
async fn probe(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(record(&ctx).await?.to_string())
}

#[action]
async fn cookies(_ctx: RequestContext) -> Result<Response, EdgeError> {
    let mut response = response_builder().body(Body::from("cookies")).unwrap();
    response
        .headers_mut()
        .append("set-cookie", "first=1; Path=/".parse().unwrap());
    response
        .headers_mut()
        .append("set-cookie", "second=2; Path=/".parse().unwrap());
    Ok(response)
}

#[action]
async fn rendered_error(_ctx: RequestContext) -> Result<Response, EdgeError> {
    Err(EdgeError::bad_request("fixture error"))
}

/// Fixtures accept only harness-controlled local origins, never arbitrary Internet targets.
pub fn is_loopback_http(uri: &edgezero_core::http::Uri) -> bool {
    uri.scheme_str() == Some("http") && matches!(uri.host(), Some("127.0.0.1" | "localhost"))
}

async fn fetch_backend(ctx: &RequestContext) -> Result<Response, EdgeError> {
    let uri = ctx
        .headers()
        .get("x-fixture-backend")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| EdgeError::bad_request("missing fixture backend"))?;
    let uri: edgezero_core::http::Uri = uri
        .parse()
        .map_err(|_| EdgeError::bad_request("invalid backend URI"))?;
    if !is_loopback_http(&uri) {
        return Err(EdgeError::bad_request("backend must be loopback HTTP"));
    }
    let mut request = OutboundRequest::new(Method::GET, uri)?;
    if matches!(ctx.uri().path(), "/stream" | "/stream-error") {
        request = request.stream_response();
    }
    ctx.http_client()
        .ok_or_else(|| EdgeError::internal(std::io::Error::other("missing HTTP client")))?
        .send(request)
        .await?
        .into_response()
}

#[action]
async fn backend(ctx: RequestContext) -> Result<Response, EdgeError> {
    fetch_backend(&ctx).await
}

struct InFlight;
impl Drop for InFlight {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}
#[action]
async fn overlap(ctx: RequestContext) -> Result<String, EdgeError> {
    INFLIGHT.fetch_add(1, Ordering::SeqCst);
    let _guard = InFlight;
    let before = record(&ctx).await?;
    let _response = fetch_backend(&ctx).await?;
    let mut after = record(&ctx).await?;
    after["before"] = before;
    Ok(after.to_string())
}

/// A missing server-side binding is a deployment fault, not a client error.
fn missing_binding(message: &'static str) -> EdgeError {
    EdgeError::internal(std::io::Error::other(message))
}

#[action]
async fn bindings(ctx: RequestContext) -> Result<String, EdgeError> {
    let config = ctx
        .config_store("fixture_config")
        .ok_or_else(|| missing_binding("config handle absent"))?;
    let marker = config.get("marker").await.map_err(EdgeError::internal)?;
    let default_marker = ctx
        .config_store_default()
        .ok_or_else(|| missing_binding("default config handle absent"))?
        .get("marker")
        .await
        .map_err(EdgeError::internal)?;
    let kv = ctx
        .kv_store("fixture_kv")
        .ok_or_else(|| missing_binding("KV handle absent"))?;
    kv.put("fixture-marker", &"local-fixture")
        .await
        .map_err(EdgeError::internal)?;
    let kv_marker: Option<String> = ctx
        .kv_store_default()
        .ok_or_else(|| missing_binding("default kv handle absent"))?
        .get("fixture-marker")
        .await
        .map_err(EdgeError::internal)?;
    let secret = ctx
        .secret_store("fixture_secrets")
        .ok_or_else(|| missing_binding("secret handle absent"))?;
    let named_secret = secret
        .get_bytes("fixture_marker")
        .await
        .map_err(EdgeError::internal)?;
    let default_secret = ctx
        .secret_store_default()
        .ok_or_else(|| missing_binding("default secret handle absent"))?
        .get_bytes("fixture_marker")
        .await
        .map_err(EdgeError::internal)?;
    Ok(serde_json::json!({
        "config": marker.as_deref() == Some("local-fixture") && marker == default_marker,
        "kv": kv_marker.as_deref() == Some("local-fixture"),
        "secrets": named_secret.is_some() && named_secret == default_secret,
        "unknown": ctx.kv_store("unknown").is_some(),
    })
    .to_string())
}

struct ObserveRequest;
#[async_trait::async_trait(?Send)]
impl edgezero_core::middleware::Middleware for ObserveRequest {
    async fn handle(
        &self,
        mut ctx: RequestContext,
        next: edgezero_core::middleware::Next<'_>,
    ) -> Result<Response, EdgeError> {
        if ctx.extensions().get::<Observation>().is_none() {
            let candidate = ctx
                .headers()
                .get("x-request-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing");
            let observation = observe(candidate, None);
            ctx.extensions_mut().insert(observation);
        }
        let lifetime = ctx.extensions().get::<ResponseLifetime>().cloned();
        let finalization = Finalize(
            ctx.headers()
                .get("x-request-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing")
                .to_owned(),
        );
        let mut response = next.run(ctx).await?;
        response.extensions_mut().insert(finalization);
        if let Some(lifetime) = lifetime {
            lifetime.1.store(true, Ordering::SeqCst);
            response.extensions_mut().insert(lifetime);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::Hooks;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;
    use futures::executor::block_on;

    #[test]
    fn retained_app_retries_failed_configuration_and_reuses_success() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        struct RetryApp;
        impl Hooks for RetryApp {
            fn configure(_app: &mut App) -> Result<(), EdgeError> {
                if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(EdgeError::service_unavailable(
                        "injected configuration failure",
                    ))
                } else {
                    Ok(())
                }
            }

            fn routes() -> RouterService {
                RouterService::builder().build()
            }
        }

        let cell = OnceLock::new();
        let platform = edgezero_core::PlatformMetadata::default();
        assert!(retained_app::<RetryApp>(&cell, platform).is_err());
        assert!(cell.get().is_none());
        let first = retained_app::<RetryApp>(&cell, platform).unwrap();
        let second = retained_app::<RetryApp>(&cell, platform).unwrap();
        assert!(std::ptr::eq(first, second));
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retained_app_returns_an_already_installed_winner() {
        static CELL: OnceLock<App> = OnceLock::new();
        struct RacingApp;
        impl Hooks for RacingApp {
            fn configure(app: &mut App) -> Result<(), EdgeError> {
                let winner = App::with_name(RouterService::builder().build(), "winner");
                assert!(CELL.set(winner).is_ok());
                app.set_name("discarded");
                Ok(())
            }

            fn routes() -> RouterService {
                RouterService::builder().build()
            }
        }

        let app =
            retained_app::<RacingApp>(&CELL, edgezero_core::PlatformMetadata::default()).unwrap();
        assert_eq!(app.name(), "winner");
    }

    #[test]
    fn probe_accepts_exact_body_bound_and_rejects_overflow() {
        let app = App::build::<FixtureApp>(edgezero_core::PlatformMetadata::default()).unwrap();
        for (size, status) in [(1024 * 1024, 200), (1024 * 1024 + 1, 400)] {
            let req = request_builder()
                .uri("/probe/bounded")
                .header("x-request-token", "bounded")
                .body(Body::from(vec![b'x'; size]))
                .unwrap();
            let response = block_on(app.router().oneshot(req)).unwrap();
            assert_eq!(response.status().as_u16(), status);
        }
    }

    #[test]
    fn retained_probe_echoes_each_request_and_preserves_cookies() {
        let app = App::build::<FixtureApp>(edgezero_core::PlatformMetadata::default()).unwrap();
        for (index, token) in ["one", "two"].into_iter().enumerate() {
            let req = request_builder()
                .uri(format!("/probe/{token}"))
                .header("x-request-token", token)
                .body(Body::from(token))
                .unwrap();
            let response = block_on(app.router().oneshot(req)).unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(response.body().as_bytes().unwrap()).unwrap();
            assert_eq!(value["token"], token);
            assert_eq!(value["path"], token);
            assert_eq!(value["body"], token);
            assert_eq!(value["shared"], index + 1);
        }
        let req = request_builder()
            .uri("/cookies")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app.router().oneshot(req)).unwrap();
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    }

    #[test]
    fn rejects_remote_origins_and_keeps_concrete_apps_separate() {
        let app = App::build::<FixtureApp>(edgezero_core::PlatformMetadata::default()).unwrap();
        let other = App::build::<OtherApp>(edgezero_core::PlatformMetadata::default()).unwrap();
        let req = request_builder()
            .uri("/origin/x")
            .header("x-fixture-backend", "https://example.com/")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app.router().oneshot(req)).unwrap();
        assert_eq!(response.status().as_u16(), 400);
        let req = request_builder().uri("/other").body(Body::empty()).unwrap();
        let response = block_on(other.router().oneshot(req)).unwrap();
        assert_eq!(response.body().as_bytes().unwrap(), b"other-app");
    }
}
