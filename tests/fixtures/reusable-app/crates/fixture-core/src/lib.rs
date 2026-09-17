#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::Hooks;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;
    use futures::executor::block_on;

    #[test]
    fn retained_probe_echoes_each_request_and_preserves_cookies() {
        let app = FixtureApp::build_app();
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
        let app = FixtureApp::build_app();
        let other = OtherApp::build_app();
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

use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::http::{Method, Response, response_builder};
use edgezero_core::proxy::ProxyRequest;
use edgezero_core::router::RouterService;
use edgezero_core::{action, body::Body, context::RequestContext, error::EdgeError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

static INSTANCE: OnceLock<String> = OnceLock::new();
static BUILDS: AtomicUsize = AtomicUsize::new(0);
static CONFIGURES: AtomicUsize = AtomicUsize::new(0);
static ORDINAL: AtomicUsize = AtomicUsize::new(0);
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static MAX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

pub fn instance_id() -> Option<&'static str> {
    INSTANCE.get().map(String::as_str)
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
    fn build_app() -> App {
        BUILDS.fetch_add(1, Ordering::SeqCst);
        let mut app = App::new(Self::routes());
        Self::configure(&mut app);
        app
    }
    fn configure(app: &mut App) {
        CONFIGURES.fetch_add(1, Ordering::SeqCst);
        app.set_name("fixture");
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
impl Hooks for OtherApp {
    fn routes() -> RouterService {
        RouterService::builder().get("/other", other).build()
    }
}

#[action]
async fn other(_ctx: RequestContext) -> Result<String, EdgeError> {
    Ok("other-app".into())
}

fn record(ctx: &RequestContext) -> serde_json::Value {
    let req = ctx.request();
    let token = req
        .headers()
        .get("x-request-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing");
    let observation = req
        .extensions()
        .get::<Observation>()
        .cloned()
        .unwrap_or_else(|| observe(token, None));
    let shared = req
        .extensions()
        .get::<Arc<AtomicUsize>>()
        .unwrap()
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    serde_json::json!({
        "instance": observation.instance, "ordinal": observation.ordinal,
        "correlation": observation.correlation, "native_id": observation.native_id,
        "builds": BUILDS.load(Ordering::SeqCst), "configures": CONFIGURES.load(Ordering::SeqCst),
        "path": ctx.path_params().get("id"), "token": token,
        "mutated": req.headers().get("x-fixture-mutated").is_some(),
        "body": String::from_utf8_lossy(ctx.body().as_bytes().unwrap_or_default()),
        "shared": shared, "max_inflight": MAX_INFLIGHT.load(Ordering::SeqCst),
    })
}

#[action]
async fn probe(mut ctx: RequestContext) -> Result<String, EdgeError> {
    use futures::StreamExt;
    let body = std::mem::replace(ctx.request_mut().body_mut(), Body::empty());
    let bytes = match body {
        Body::Once(bytes) => bytes.to_vec(),
        Body::Stream(mut stream) => {
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk.map_err(EdgeError::internal)?);
            }
            bytes
        }
    };
    *ctx.request_mut().body_mut() = Body::from(bytes);
    Ok(record(&ctx).to_string())
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

async fn fetch_backend(ctx: &RequestContext) -> Result<Response, EdgeError> {
    let uri = ctx
        .request()
        .headers()
        .get("x-fixture-backend")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| EdgeError::bad_request("missing fixture backend"))?;
    let uri: edgezero_core::http::Uri = uri
        .parse()
        .map_err(|_| EdgeError::bad_request("invalid backend URI"))?;
    // Fixtures accept only harness-controlled local origins, never arbitrary Internet targets.
    if uri.scheme_str() != Some("http") || !matches!(uri.host(), Some("127.0.0.1" | "localhost")) {
        return Err(EdgeError::bad_request("backend must be loopback HTTP"));
    }
    ctx.proxy_handle()
        .ok_or_else(|| EdgeError::bad_request("missing proxy"))?
        .forward(ProxyRequest::new(Method::GET, uri))
        .await
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
    let count = INFLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = InFlight;
    MAX_INFLIGHT.fetch_max(count, Ordering::SeqCst);
    let before = record(&ctx);
    let _response = fetch_backend(&ctx).await?;
    let mut after = record(&ctx);
    after["before"] = before;
    Ok(after.to_string())
}

#[action]
async fn bindings(ctx: RequestContext) -> Result<String, EdgeError> {
    let config = ctx
        .config_store("fixture_config")
        .ok_or_else(|| EdgeError::bad_request("config handle absent"))?;
    let marker = config.get("marker").await.map_err(EdgeError::internal)?;
    let default_marker = ctx
        .config_store_default()
        .unwrap()
        .get("marker")
        .await
        .map_err(EdgeError::internal)?;
    let kv = ctx
        .kv_store("fixture_kv")
        .ok_or_else(|| EdgeError::bad_request("KV handle absent"))?;
    kv.put("fixture-marker", &"local-fixture")
        .await
        .map_err(EdgeError::internal)?;
    let kv_marker: Option<String> = ctx
        .kv_store_default()
        .unwrap()
        .get("fixture-marker")
        .await
        .map_err(EdgeError::internal)?;
    let secret = ctx
        .secret_store("fixture_secrets")
        .ok_or_else(|| EdgeError::bad_request("secret handle absent"))?;
    let named_secret = secret
        .get_bytes("fixture_marker")
        .await
        .map_err(EdgeError::internal)?;
    let default_secret = ctx
        .secret_store_default()
        .unwrap()
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
        if ctx.request().extensions().get::<Observation>().is_none() {
            let candidate = ctx
                .request()
                .headers()
                .get("x-request-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing");
            let observation = observe(candidate, None);
            ctx.request_mut().extensions_mut().insert(observation);
        }
        let lifetime = ctx
            .request()
            .extensions()
            .get::<ResponseLifetime>()
            .cloned();
        let finalization = Finalize(
            ctx.request()
                .headers()
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
