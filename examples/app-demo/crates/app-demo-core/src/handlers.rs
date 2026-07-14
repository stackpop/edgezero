use std::env;
use std::sync::Arc;

use bytes::Bytes;
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::{
    AppConfig, Headers, Json, Kv, Path, Query, Secrets, State, ValidatedPath,
};
use edgezero_core::http::{self, Response, StatusCode, Uri};
use edgezero_core::proxy::ProxyRequest;
use edgezero_core::response::Text;
use futures::{stream, StreamExt as _};

use crate::config::AppDemoConfig;

const ALLOWED_CONFIG_KEYS: &[&str] = &["greeting", "feature.new_checkout", "service.timeout_ms"];
const DEFAULT_PROXY_BASE: &str = "https://httpbin.org";
/// Maximum request body size (25 MB, matches KV value limit).
const MAX_BODY_SIZE: usize = 25 * 1024 * 1024;
// 512 (KV key limit) - 5 (len of "note:") = 507
const MAX_NOTE_ID_LEN: u64 = 507;
const SMOKE_SECRET_MISSING_NAME: &str = "SMOKE_SECRET_MISSING";
const SMOKE_SECRET_NAME: &str = "SMOKE_SECRET";

#[derive(serde::Deserialize)]
struct ConfigParams {
    name: String,
}

#[derive(serde::Deserialize)]
pub struct EchoBody {
    pub name: String,
}

#[derive(serde::Deserialize)]
pub struct EchoParams {
    pub name: String,
}

#[derive(serde::Deserialize, validator::Validate)]
pub struct NoteIdPath {
    #[validate(length(
        min = 1_u64,
        max = "MAX_NOTE_ID_LEN",
        message = "note id must be 1–507 bytes"
    ))]
    pub id: String,
}

#[derive(serde::Deserialize)]
struct ProxyPath {
    #[serde(default)]
    rest: String,
}

#[action]
pub async fn root() -> Text<&'static str> {
    Text::new("app-demo app")
}

#[action]
pub async fn echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("Hello, {}!", params.name))
}

#[action]
pub async fn headers(Headers(headers): Headers) -> Text<String> {
    let ua = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(unknown)");
    Text::new(format!("ua={ua}"))
}

#[action]
pub async fn stream() -> Result<Response, EdgeError> {
    let body = Body::stream(
        stream::iter(0_i32..3_i32).map(|index| Bytes::from(format!("chunk {index}\n"))),
    );

    http::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .map_err(EdgeError::internal)
}

#[action]
pub async fn echo_json(Json(body): Json<EchoBody>) -> Text<String> {
    Text::new(format!("Hello, {}!", body.name))
}

#[action]
pub async fn proxy_demo(RequestContext(ctx): RequestContext) -> Result<Response, EdgeError> {
    let params: ProxyPath = ctx.path()?;
    let proxy_handle = ctx.proxy_handle();
    let request = ctx.into_request();
    let base = env::var("API_BASE_URL").unwrap_or_else(|_| DEFAULT_PROXY_BASE.to_owned());
    let target = build_proxy_target(&base, &params.rest, request.uri())?;
    let proxy_request = ProxyRequest::from_request(request, target);

    if let Some(handle) = proxy_handle {
        handle.forward(proxy_request).await
    } else {
        proxy_not_available_response()
    }
}

fn build_proxy_target(base: &str, rest: &str, original_uri: &Uri) -> Result<Uri, EdgeError> {
    let mut target = base.trim_end_matches('/').to_owned();
    let trimmed_rest = rest.trim_start_matches('/');
    if !trimmed_rest.is_empty() {
        target.push('/');
        target.push_str(trimmed_rest);
    }

    if let Some(query) = original_uri.query() {
        if !query.is_empty() {
            target.push('?');
            target.push_str(query);
        }
    }

    target
        .parse::<Uri>()
        .map_err(|err| EdgeError::bad_request(format!("invalid proxy target URI: {err}")))
}

fn proxy_not_available_response() -> Result<Response, EdgeError> {
    let body = Body::text(
        "proxy example is not enabled for this adapter build; enable a proxy-capable adapter",
    );
    http::response_builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .map_err(EdgeError::internal)
}

fn text_response(status: StatusCode, message: impl Into<String>) -> Result<Response, EdgeError> {
    http::response_builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::text(message.into()))
        .map_err(EdgeError::internal)
}

#[action]
pub async fn config_get(RequestContext(ctx): RequestContext) -> Result<Response, EdgeError> {
    let params: ConfigParams = ctx.path()?;
    if !ALLOWED_CONFIG_KEYS.contains(&params.name.as_str()) {
        return text_response(
            StatusCode::NOT_FOUND,
            format!("config key '{}' is not exposed by the demo", params.name),
        );
    }

    // The registry-aware accessor reads the wired `ConfigRegistry`
    // (adapter dispatchers synthesise one from any legacy bare handle
    // before reaching the router).
    let Some(store) = ctx.config_store_default() else {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "config store is unavailable for this adapter",
        );
    };

    match store.get(&params.name).await? {
        Some(value) => text_response(StatusCode::OK, value),
        None => text_response(
            StatusCode::NOT_FOUND,
            format!("config key '{}' not found", params.name),
        ),
    }
}

/// Return the greeting from the typed blob config.
///
/// Demonstrates the blob-model extractor (`AppConfig<AppDemoConfig>`): the
/// adapter pushes a signed JSON envelope into the config store via
/// `app-demo-cli config push`, and the extractor deserialises + secret-walks
/// it on every request. No manual `config_store_default()` or
/// `secret_store.require_str()` calls needed.
#[action]
pub async fn config_typed(AppConfig(cfg): AppConfig<AppDemoConfig>) -> Result<Response, EdgeError> {
    text_response(StatusCode::OK, cfg.greeting)
}

/// Increment and return a visit counter stored in the `sessions`
/// KV store. The `[stores.kv]` manifest declares both `sessions`
/// and `cache` ids; the counter lives in `sessions` because it
/// tracks per-deployment session-flavoured state — the demo
/// exercises [`Kv::named`] (not `.default()`) so the multi-store
/// surface is visible end-to-end.
#[action]
pub async fn kv_counter(kv: Kv) -> Result<Response, EdgeError> {
    let store = kv
        .named("sessions")
        .ok_or_else(|| EdgeError::service_unavailable("KV store `sessions` is not registered"))?;
    let count: i64 = store
        .read_modify_write("demo:counter", 0_i64, |n| n.wrapping_add(1))
        .await?;
    let body = serde_json::json!({ "count": count }).to_string();
    http::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::text(body))
        .map_err(EdgeError::internal)
}

/// Store a note by id (body = note text) in the `cache` KV store.
/// Notes are short-lived, cache-flavoured payloads — separate
/// from the `sessions` store the counter writes to, so the demo
/// exercises both named ids declared in `[stores.kv]`.
#[action]
pub async fn kv_note_put(
    kv: Kv,
    ValidatedPath(path): ValidatedPath<NoteIdPath>,
    RequestContext(ctx): RequestContext,
) -> Result<Response, EdgeError> {
    let store = kv
        .named("cache")
        .ok_or_else(|| EdgeError::service_unavailable("KV store `cache` is not registered"))?;
    let body = ctx.into_request().into_body();
    let body_bytes = body.into_bytes_bounded(MAX_BODY_SIZE).await?;
    store
        .put_bytes(&format!("note:{}", path.id), body_bytes)
        .await?;
    http::response_builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .map_err(EdgeError::internal)
}

/// Read a note by id from the `cache` KV store.
#[action]
pub async fn kv_note_get(
    kv: Kv,
    ValidatedPath(path): ValidatedPath<NoteIdPath>,
) -> Result<Response, EdgeError> {
    let store = kv
        .named("cache")
        .ok_or_else(|| EdgeError::service_unavailable("KV store `cache` is not registered"))?;
    match store.get_bytes(&format!("note:{}", path.id)).await? {
        Some(data) => http::response_builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(data.to_vec()))
            .map_err(EdgeError::internal),
        None => Err(EdgeError::not_found(format!("note:{}", path.id))),
    }
}

/// Delete a note by id from the `cache` KV store.
#[action]
pub async fn kv_note_delete(
    kv: Kv,
    ValidatedPath(path): ValidatedPath<NoteIdPath>,
) -> Result<Response, EdgeError> {
    let store = kv
        .named("cache")
        .ok_or_else(|| EdgeError::service_unavailable("KV store `cache` is not registered"))?;
    store.delete(&format!("note:{}", path.id)).await?;
    http::response_builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .map_err(EdgeError::internal)
}

// ---------------------------------------------------------------------------
// Secrets demo handler — illustrates platform-neutral secret access.
// WARNING: This handler returns the raw secret value in the response body.
//          It exists solely for smoke-testing. Never do this in production.
//          Only fixed smoke-test key names are accepted.
// ---------------------------------------------------------------------------

/// Echo the value of an allowlisted smoke-test secret from the configured store.
///
/// Usage: `GET /secrets/echo?name=SMOKE_SECRET`
#[action]
pub async fn secrets_echo(
    secrets: Secrets,
    Query(params): Query<EchoParams>,
) -> Result<Text<String>, EdgeError> {
    match params.name.as_str() {
        SMOKE_SECRET_NAME | SMOKE_SECRET_MISSING_NAME => {}
        _ => {
            return Err(EdgeError::bad_request(
                "only smoke-test secret names are allowed",
            ))
        }
    }

    // `BoundSecretStore` is pre-bound to a platform store name by the
    // adapter (`EDGEZERO__STORES__SECRETS__<ID>__NAME` or the logical id);
    // the handler passes only the key.
    let store = secrets
        .default()
        .ok_or_else(|| EdgeError::service_unavailable("no default secret store registered"))?;
    let value = store
        .require_str(&params.name)
        .await
        .map_err(EdgeError::from)?;
    Ok(Text::new(value))
}

/// Demonstrates app-owned shared state injected via `app!(..., state = ...)`:
/// the `State<Arc<DemoState>>` extractor resolves the value the macro-generated
/// router registered with `RouterBuilder::with_state`.
#[action]
pub async fn state_demo(
    State(state): State<Arc<crate::DemoState>>,
) -> Result<Text<String>, EdgeError> {
    Ok(Text::new(state.greeting.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use edgezero_core::blob_envelope::BlobEnvelope;
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::context::RequestContext;
    use edgezero_core::http::header::{HeaderName, HeaderValue};
    use edgezero_core::http::{request_builder, Method, StatusCode, Uri};
    use edgezero_core::key_value_store::{KvError, KvHandle, KvPage, KvStore};
    use edgezero_core::params::PathParams;
    use edgezero_core::proxy::{ProxyClient, ProxyHandle, ProxyResponse};
    use edgezero_core::response::IntoResponse as _;
    use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
    use edgezero_core::store_registry::{
        ConfigRegistry, ConfigStoreBinding, KvRegistry, StoreRegistry,
    };
    use futures::executor::block_on;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct MapConfigStore(HashMap<String, String>);

    struct MockKv {
        data: Mutex<BTreeMap<String, Bytes>>,
    }

    struct TestProxyClient;

    struct UnavailableConfigStore;

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for MapConfigStore {
        async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(self.0.get(key).cloned())
        }
    }

    impl MockKv {
        fn new() -> Self {
            Self {
                data: Mutex::new(BTreeMap::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl KvStore for MockKv {
        async fn delete(&self, key: &str) -> Result<(), KvError> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }

        async fn exists(&self, key: &str) -> Result<bool, KvError> {
            Ok(self.data.lock().unwrap().contains_key(key))
        }

        async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }

        async fn list_keys_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: usize,
        ) -> Result<KvPage, KvError> {
            let data = self.data.lock().unwrap();
            let mut keys = data
                .keys()
                .filter(|key| {
                    key.starts_with(prefix) && cursor.is_none_or(|cur| key.as_str() > cur)
                })
                .cloned()
                .collect::<Vec<_>>();
            let has_more = keys.len() > limit;
            keys.truncate(limit);

            Ok(KvPage {
                cursor: has_more.then(|| keys.last().cloned()).flatten(),
                keys,
            })
        }

        async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
            self.data.lock().unwrap().insert(key.to_owned(), value);
            Ok(())
        }

        async fn put_bytes_with_ttl(
            &self,
            key: &str,
            value: Bytes,
            _ttl: Duration,
        ) -> Result<(), KvError> {
            self.data.lock().unwrap().insert(key.to_owned(), value);
            Ok(())
        }
    }

    #[async_trait(?Send)]
    impl ProxyClient for TestProxyClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let (_method, uri, _headers, _body, _) = request.into_parts();
            assert!(uri.to_string().contains("status/201"));
            Ok(ProxyResponse::new(StatusCode::CREATED, Body::empty()))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl ConfigStore for UnavailableConfigStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Err(ConfigStoreError::unavailable("backend offline"))
        }
    }

    struct FixedStore(String);

    #[async_trait(?Send)]
    impl ConfigStore for FixedStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.clone()))
        }
    }

    #[test]
    fn introspection_routes_are_wired() {
        let router = crate::build_router();

        // manifest: 200 + JSON body whose [app].name is "app-demo".
        let manifest_req = request_builder()
            .method(Method::GET)
            .uri("/_app-demo/manifest")
            .body(Body::empty())
            .unwrap();
        let manifest_resp = block_on(router.oneshot(manifest_req)).unwrap();
        assert_eq!(manifest_resp.status(), StatusCode::OK);
        assert_eq!(
            manifest_resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let manifest_body: serde_json::Value = manifest_resp.into_body().to_json().unwrap();
        assert_eq!(manifest_body["app"]["name"], "app-demo");

        // routes: 200 + [{method,path}] including the root route.
        let routes_req = request_builder()
            .method(Method::GET)
            .uri("/_app-demo/routes")
            .body(Body::empty())
            .unwrap();
        let routes_resp = block_on(router.oneshot(routes_req)).unwrap();
        assert_eq!(routes_resp.status(), StatusCode::OK);
        let routes_body: serde_json::Value = routes_resp.into_body().to_json().unwrap();
        let arr = routes_body.as_array().expect("routes array");
        assert!(arr
            .iter()
            .any(|entry| entry["method"] == "GET" && entry["path"] == "/"));

        // /config: seed a default config store with a valid envelope so a wired
        // route returns 200 (a routing miss would be 404, proving nothing).
        let data = serde_json::json!({ "greeting": "hi", "api_token": "demo_api_token" });
        let blob =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned()))
                .unwrap();
        let registry: ConfigRegistry = StoreRegistry::new(
            [(
                "app_config".to_owned(),
                ConfigStoreBinding {
                    handle: ConfigStoreHandle::new(Arc::new(FixedStore(blob))),
                    default_key: "app_config".to_owned(),
                },
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "app_config".to_owned(),
        );
        let mut config_req = request_builder()
            .method(Method::GET)
            .uri("/_app-demo/config")
            .body(Body::empty())
            .unwrap();
        config_req.extensions_mut().insert(registry);
        let config_resp = block_on(router.oneshot(config_req)).unwrap();
        assert_eq!(
            config_resp.status(),
            StatusCode::OK,
            "/config should be wired and 200 with a store"
        );
        // Raw envelope `data`: secret field holds the KEY NAME, not a resolved value.
        let config_body: serde_json::Value = config_resp.into_body().to_json().unwrap();
        assert_eq!(config_body["api_token"], "demo_api_token");
        assert_eq!(config_body["greeting"], "hi");
    }

    #[test]
    fn build_proxy_target_merges_segments_and_query() {
        let original = Uri::from_static("/proxy/status?foo=bar");
        let target = build_proxy_target("https://example.com/api", "status/200", &original)
            .expect("target uri");
        assert_eq!(
            target.to_string(),
            "https://example.com/api/status/200?foo=bar"
        );
    }

    #[test]
    fn config_get_returns_404_for_keys_outside_demo_allowlist() {
        let ctx = context_with_config_key("missing.key", &[("missing.key", "value")]);
        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_get_returns_404_when_key_not_in_allowlist() {
        let ctx = context_with_config_key("missing.key", &[("other.key", "value")]);
        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_get_returns_404_when_key_not_in_store() {
        let ctx = context_with_config_key("greeting", &[("other_key", "value")]);
        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_get_returns_503_when_no_store_injected() {
        let ctx = context_with_params("/config/greeting", &[("name", "greeting")]);
        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn config_get_returns_503_when_store_lookup_fails() {
        let ctx = context_with_unavailable_config_store("greeting");
        let err = block_on(config_get(ctx)).expect_err("expected store error");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn config_get_returns_value_when_key_exists() {
        let ctx = context_with_config_key("greeting", &[("greeting", "hello from config store")]);
        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .into_body()
                .into_bytes()
                .expect("buffered")
                .as_ref(),
            b"hello from config store"
        );
    }

    #[test]
    fn config_get_resolves_through_registry_when_wired() {
        // Adapters wire `ConfigRegistry` into request extensions; the
        // handler must read it via `ctx.config_store_default()` instead of
        // the legacy single-handle accessor (which would 503).
        use edgezero_core::store_registry::{ConfigRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        let store = MapConfigStore(
            [("greeting".to_owned(), "hello from registry".to_owned())]
                .into_iter()
                .collect(),
        );
        let by_id: BTreeMap<String, ConfigStoreBinding> = [(
            "app_config".to_owned(),
            ConfigStoreBinding {
                handle: ConfigStoreHandle::new(Arc::new(store)),
                default_key: "app_config".to_owned(),
            },
        )]
        .into_iter()
        .collect();
        let registry: ConfigRegistry = StoreRegistry::new(by_id, "app_config".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config/greeting")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        let mut params = HashMap::new();
        params.insert("name".to_owned(), "greeting".to_owned());
        let ctx = RequestContext::new(request, PathParams::new(params));

        let response = block_on(config_get(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .into_body()
                .into_bytes()
                .expect("buffered")
                .as_ref(),
            b"hello from registry"
        );
    }

    fn context_with_config_key(key: &str, entries: &[(&str, &str)]) -> RequestContext {
        // Hard-cutoff: wire a real `ConfigRegistry`
        // rather than a bare `ConfigStoreHandle`. The
        // registry-aware accessor `ctx.config_store_default()`
        // no longer falls back to a wired bare handle.
        use edgezero_core::store_registry::{ConfigRegistry, StoreRegistry};
        let mut request = request_builder()
            .method(Method::GET)
            .uri(format!("/config/{key}"))
            .body(Body::empty())
            .expect("request");
        let store = MapConfigStore(
            entries
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
        );
        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(store)),
            default_key: "app_config".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("app_config".to_owned(), binding);
        request.extensions_mut().insert(registry);
        let mut params = HashMap::new();
        params.insert("name".to_owned(), key.to_owned());
        RequestContext::new(request, PathParams::new(params))
    }

    fn context_with_header(path: &str, header: HeaderName, value: HeaderValue) -> RequestContext {
        let mut request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        request.headers_mut().insert(header, value);
        RequestContext::new(request, PathParams::default())
    }

    fn context_with_json(path: &str, json: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::from(json))
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    /// Build a `KvRegistry` with the two named stores
    /// (`sessions` + `cache`) the manifest declares, each backed
    /// by its own `MockKv`. The registry's `Clone` is cheap (each
    /// `KvHandle` is `Arc`-backed), so a test can share the same
    /// registry across two contexts to verify cross-request
    /// persistence — which is what the put-then-get and put-then-
    /// delete tests need.
    fn context_with_kv(
        path: &str,
        method: Method,
        body: Body,
        params: &[(&str, &str)],
    ) -> (RequestContext, KvRegistry) {
        use edgezero_core::store_registry::StoreRegistry;
        let sessions = KvHandle::new(Arc::new(MockKv::new()));
        let cache = KvHandle::new(Arc::new(MockKv::new()));
        let by_id: BTreeMap<String, KvHandle> = [
            ("sessions".to_owned(), sessions),
            ("cache".to_owned(), cache),
        ]
        .into_iter()
        .collect();
        let registry: KvRegistry = StoreRegistry::new(by_id, "sessions".to_owned());
        let mut request = request_builder()
            .method(method)
            .uri(path)
            .body(body)
            .expect("request");
        request.extensions_mut().insert(registry.clone());
        let map = params
            .iter()
            .map(|&(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<HashMap<_, _>>();
        (RequestContext::new(request, PathParams::new(map)), registry)
    }

    fn context_with_params(path: &str, params: &[(&str, &str)]) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let map = params
            .iter()
            .map(|&(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<HashMap<_, _>>();
        RequestContext::new(request, PathParams::new(map))
    }

    fn context_with_secrets(path: &str, query: &str, entries: &[(&str, &str)]) -> RequestContext {
        // Exercise the production registry path: build a one-id
        // `SecretRegistry` whose bound platform store name matches the
        // prefix the `InMemorySecretStore` is keyed under. The handler
        // uses `secrets.default()?.require_str(key)` against the bound
        // `default` store; the `InMemorySecretStore` looks up
        // `"default/<key>"`.
        use edgezero_core::store_registry::{BoundSecretStore, SecretRegistry, StoreRegistry};
        use std::collections::BTreeMap;

        const PLATFORM_NAME: &str = "default";
        let provider = InMemorySecretStore::new(entries.iter().map(|&(name, value)| {
            (
                format!("{PLATFORM_NAME}/{name}"),
                bytes::Bytes::from(value.to_owned()),
            )
        }));
        let handle = SecretHandle::new(Arc::new(provider));
        let bound = BoundSecretStore::new(handle, PLATFORM_NAME.to_owned());
        let by_id: BTreeMap<String, BoundSecretStore> =
            [("default".to_owned(), bound)].into_iter().collect();
        let registry: SecretRegistry = StoreRegistry::new(by_id, "default".to_owned());
        let uri = format!("{path}?{query}");
        let mut request = request_builder()
            .method(Method::GET)
            .uri(uri.as_str())
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        RequestContext::new(request, PathParams::default())
    }

    fn context_with_unavailable_config_store(key: &str) -> RequestContext {
        // Hard-cutoff: same registry wiring as
        // `context_with_config_key` — wire a one-id
        // `ConfigRegistry` so the registry-aware accessor
        // resolves a backend (the `UnavailableConfigStore` then
        // errors on lookup, which is what the test asserts).
        use edgezero_core::store_registry::{ConfigRegistry, StoreRegistry};
        let mut request = request_builder()
            .method(Method::GET)
            .uri(format!("/config/{key}"))
            .body(Body::empty())
            .expect("request");
        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(UnavailableConfigStore)),
            default_key: "app_config".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("app_config".to_owned(), binding);
        request.extensions_mut().insert(registry);
        let mut params = HashMap::new();
        params.insert("name".to_owned(), key.to_owned());
        RequestContext::new(request, PathParams::new(params))
    }

    #[test]
    fn echo_formats_name_from_path() {
        let ctx = context_with_params("/echo/alice", &[("name", "alice")]);
        let response = block_on(echo(ctx))
            .expect("handler ok")
            .into_response()
            .expect("response");
        let bytes = response.into_body().into_bytes().expect("buffered");
        assert_eq!(bytes.as_ref(), b"Hello, alice!");
    }

    #[test]
    fn echo_json_formats_payload() {
        let ctx = context_with_json("/echo", r#"{"name":"Edge"}"#);
        let response = block_on(echo_json(ctx))
            .expect("handler ok")
            .into_response()
            .expect("response");
        let bytes = response.into_body().into_bytes().expect("buffered");
        assert_eq!(bytes.as_ref(), b"Hello, Edge!");
    }

    fn empty_context(path: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    #[test]
    fn headers_reports_user_agent() {
        let ctx = context_with_header(
            "/headers",
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("DemoAgent"),
        );

        let response = block_on(headers(ctx))
            .expect("handler ok")
            .into_response()
            .expect("response");
        let bytes = response.into_body().into_bytes().expect("buffered");
        assert_eq!(bytes.as_ref(), b"ua=DemoAgent");
    }

    #[test]
    fn kv_counter_increments() {
        let (ctx, _) = context_with_kv("/kv/counter", Method::POST, Body::empty(), &[]);
        let resp = block_on(kv_counter(ctx)).expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().into_bytes().expect("buffered");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 1_i64);
    }

    #[test]
    fn kv_note_delete_returns_no_content() {
        let (ctx, registry) = context_with_kv(
            "/kv/notes/del",
            Method::POST,
            Body::from("to-delete"),
            &[("id", "del")],
        );
        block_on(kv_note_put(ctx)).unwrap();

        // Reuse the same registry so the delete sees the put's
        // write — KvHandle is Arc-backed, so cloning the registry
        // shares the underlying `cache` store across both ctxs.
        let ctx2 = {
            let mut request = request_builder()
                .method(Method::DELETE)
                .uri("/kv/notes/del")
                .body(Body::empty())
                .expect("request");
            request.extensions_mut().insert(registry);
            let mut map = HashMap::new();
            map.insert("id".to_owned(), "del".to_owned());
            RequestContext::new(request, PathParams::new(map))
        };
        let resp = block_on(kv_note_delete(ctx2)).expect("response");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn kv_note_get_missing_returns_404() {
        let (ctx, _) = context_with_kv(
            "/kv/notes/xyz",
            Method::GET,
            Body::empty(),
            &[("id", "xyz")],
        );
        let err = block_on(kv_note_get(ctx)).expect_err("should be NotFound");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn kv_note_put_and_get() {
        let (ctx, registry) = context_with_kv(
            "/kv/notes/abc",
            Method::POST,
            Body::from("hello world"),
            &[("id", "abc")],
        );
        let put_resp = block_on(kv_note_put(ctx)).expect("response");
        assert_eq!(put_resp.status(), StatusCode::CREATED);

        // Same registry → same `cache` store, so the get reads
        // the value the put just wrote.
        let ctx2 = {
            let mut request = request_builder()
                .method(Method::GET)
                .uri("/kv/notes/abc")
                .body(Body::empty())
                .expect("request");
            request.extensions_mut().insert(registry);
            let mut map = HashMap::new();
            map.insert("id".to_owned(), "abc".to_owned());
            RequestContext::new(request, PathParams::new(map))
        };
        let get_resp = block_on(kv_note_get(ctx2)).expect("response");
        assert_eq!(get_resp.status(), StatusCode::OK);
        assert_eq!(
            get_resp
                .into_body()
                .into_bytes()
                .expect("buffered")
                .as_ref(),
            b"hello world"
        );
    }

    #[test]
    fn proxy_demo_uses_injected_handle() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/proxy/status/201")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ProxyHandle::with_client(TestProxyClient));

        let mut params = HashMap::new();
        params.insert("rest".to_owned(), "status/201".to_owned());
        let ctx = RequestContext::new(request, PathParams::new(params));

        let response = block_on(proxy_demo(ctx)).expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[test]
    fn proxy_demo_without_handle_returns_placeholder() {
        let ctx = context_with_params("/proxy/status/200", &[("rest", "status/200")]);
        let response = block_on(proxy_demo(ctx)).expect("response");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn root_returns_static_body() {
        let ctx = empty_context("/");
        let response = block_on(root(ctx))
            .expect("handler ok")
            .into_response()
            .expect("response");
        let bytes = response.into_body().into_bytes().expect("buffered");
        assert_eq!(bytes.as_ref(), b"app-demo app");
    }

    #[test]
    fn secrets_echo_rejects_non_smoke_secret_names() {
        use edgezero_core::http::StatusCode;

        let ctx = context_with_secrets("/secrets/echo", "name=API_KEY", &[("API_KEY", "secret")]);
        let response = block_on(secrets_echo(ctx))
            .expect_err("should reject arbitrary secret names")
            .into_response()
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = String::from_utf8(
            response
                .into_body()
                .into_bytes()
                .expect("buffered")
                .to_vec(),
        )
        .expect("utf8");
        assert!(body.contains("only smoke-test secret names are allowed"));
        assert!(!body.contains("API_KEY"));
    }

    #[test]
    fn secrets_echo_returns_sanitized_500_for_missing_allowed_secret() {
        use edgezero_core::http::StatusCode;

        let ctx = context_with_secrets("/secrets/echo", "name=SMOKE_SECRET_MISSING", &[]);
        let response = block_on(secrets_echo(ctx))
            .expect_err("should fail")
            .into_response()
            .expect("response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = String::from_utf8(
            response
                .into_body()
                .into_bytes()
                .expect("buffered")
                .to_vec(),
        )
        .expect("utf8");
        assert!(body.contains("required secret is not configured"));
        assert!(!body.contains("SMOKE_SECRET_MISSING"));
    }

    #[test]
    fn secrets_echo_returns_secret_value() {
        let ctx = context_with_secrets(
            "/secrets/echo",
            "name=SMOKE_SECRET",
            &[("SMOKE_SECRET", "my-secret-value")],
        );
        let response = block_on(secrets_echo(ctx))
            .expect("handler ok")
            .into_response()
            .expect("response");
        let bytes = response.into_body().into_bytes().expect("buffered");
        assert_eq!(bytes.as_ref(), b"my-secret-value");
    }

    #[test]
    fn stream_emits_expected_chunks() {
        let ctx = empty_context("/stream");
        let response = block_on(stream(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::OK);

        let mut chunks = response.into_body().into_stream().expect("stream body");
        let collected = block_on(async {
            let mut buf = Vec::new();
            while let Some(item) = chunks.next().await {
                let chunk = item.expect("chunk");
                buf.extend_from_slice(&chunk);
            }
            buf
        });
        assert_eq!(
            String::from_utf8(collected).expect("utf8"),
            "chunk 0\nchunk 1\nchunk 2\n"
        );
    }

    #[test]
    fn state_demo_handler_reads_app_state_through_macro_router() {
        // build_router() is macro-generated and now calls `.with_state(crate::app_state())`.
        let service = crate::build_router();

        let request = request_builder()
            .method(Method::GET)
            .uri("/state-demo")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.body().as_bytes().expect("buffered"),
            b"hello from app state"
        );
    }
}
