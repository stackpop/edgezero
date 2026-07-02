//! Framework-supplied introspection handlers. Bind via `[[triggers.http]]`:
//! `handler = "edgezero_core::introspection::manifest"` etc.

use crate::blob_envelope::BlobEnvelope;
use crate::body::Body;
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::extractor::FromRequest;
// NOTE: `Response` is an HTTP alias exported from `crate::http`, NOT
// `crate::response` (response.rs itself imports it from crate::http).
use crate::http::{response_builder, Response, StatusCode};
use crate::router::RouteInfo;
use async_trait::async_trait;
use edgezero_core::action;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct RouteView {
    method: String,
    path: String,
}

/// Extractor for the baked manifest JSON carried in the request's
/// [`crate::router::IntrospectionData`]. Errors with 500 if the data is
/// absent (i.e. the router did not inject it).
pub struct ManifestJson(pub Arc<str>);

#[async_trait(?Send)]
impl FromRequest for ManifestJson {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.introspection()
            .and_then(|data| data.manifest_json.clone())
            .map(ManifestJson)
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!("manifest introspection data not available"))
            })
    }
}

/// Extractor for the live route index carried in the request's
/// [`crate::router::IntrospectionData`]. Errors with 500 if the data is absent.
pub struct RouteTable(pub Arc<[RouteInfo]>);

#[async_trait(?Send)]
impl FromRequest for RouteTable {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.introspection()
            .map(|data| RouteTable(Arc::clone(&data.routes)))
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "route-table introspection data not available"
                ))
            })
    }
}

fn json_response(status: StatusCode, body: Body) -> Result<Response, EdgeError> {
    response_builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body)
        .map_err(EdgeError::internal)
}

/// GET — the app manifest as JSON (baked at compile time by `app!`).
#[action]
pub async fn manifest(ManifestJson(json): ManifestJson) -> Result<Response, EdgeError> {
    json_response(StatusCode::OK, Body::text(json.to_string()))
}

/// GET — `[{ "method", "path" }]` for every registered route.
#[action]
pub async fn routes(RouteTable(table): RouteTable) -> Result<Response, EdgeError> {
    let views: Vec<RouteView> = table
        .iter()
        .map(|route| RouteView {
            method: route.method().as_str().to_owned(),
            path: route.path().to_owned(),
        })
        .collect();
    let body = Body::json(&views).map_err(EdgeError::internal)?;
    json_response(StatusCode::OK, body)
}

/// GET — the default config-store envelope `data` (secret-safe: secret
/// fields remain unresolved key-name references).
#[action]
pub async fn config(ctx: RequestContext) -> Result<Response, EdgeError> {
    let binding = ctx
        .config_store_default_binding()
        .ok_or_else(|| EdgeError::not_found("no default config store registered"))?;
    // ConfigStoreError → EdgeError preserves 503/400/500 (see extractor.rs).
    let raw = binding
        .handle
        .get(&binding.default_key)
        .await
        .map_err(EdgeError::from)?
        .ok_or_else(|| EdgeError::not_found("no config blob in default store"))?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {err}")))?;
    envelope.verify().map_err(|err| {
        EdgeError::internal(anyhow::anyhow!("envelope verification failed: {err}"))
    })?;
    let body = Body::json(&envelope.into_data()).map_err(EdgeError::internal)?;
    json_response(StatusCode::OK, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use crate::http::{request_builder, Method, Response};
    use crate::router::RouterService;
    use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
    use async_trait::async_trait;
    use futures::executor::block_on;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // A config store returning a fixed result for `get`, used to drive the
    // config handler's status-code mapping. Mirrors the pattern in
    // extractor.rs::config_extractor_resolves_from_registry.
    struct StubStore(Result<Option<String>, ConfigStoreError>);
    #[async_trait(?Send)]
    impl ConfigStore for StubStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            match &self.0 {
                Ok(val) => Ok(val.clone()),
                Err(ConfigStoreError::Unavailable { .. }) => {
                    Err(ConfigStoreError::unavailable("down"))
                }
                Err(ConfigStoreError::InvalidKey { .. }) => {
                    Err(ConfigStoreError::invalid_key("bad"))
                }
                Err(_) => Err(ConfigStoreError::internal(anyhow::anyhow!("boom"))),
            }
        }
    }

    // Collect a buffered response body into JSON (introspection responses are
    // always `Body::Once`). `Body::to_json` works on the buffered variant.
    fn body_json(resp: Response) -> serde_json::Value {
        resp.into_body().to_json().expect("buffered JSON body")
    }

    // Build a request carrying a default ConfigRegistry backed by `store`, and
    // drive it THROUGH THE ROUTER via `oneshot` (which maps handler `EdgeError`
    // to a response internally — so we neither import `IntoResponse` nor unwrap
    // an error path by hand).
    fn run_config(store: StubStore) -> Response {
        let registry: ConfigRegistry = StoreRegistry::new(
            [(
                "default".to_owned(),
                ConfigStoreBinding {
                    handle: ConfigStoreHandle::new(Arc::new(store)),
                    default_key: "default".to_owned(),
                },
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "default".to_owned(),
        );
        let router = RouterService::builder().get("/c", config).build();
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/c")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(registry);
        block_on(router.oneshot(request)).unwrap()
    }

    fn valid_envelope_json(data: serde_json::Value) -> String {
        // Build a real envelope so sha/version are correct.
        serde_json::to_string(&BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned())).unwrap()
    }

    #[test]
    fn manifest_returns_injected_json() {
        let router = RouterService::builder()
            .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
            .get("/m", manifest)
            .build();
        let req = request_builder()
            .method(Method::GET)
            .uri("/m")
            .body(Body::empty())
            .unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        // Body is the injected manifest JSON verbatim.
        assert_eq!(
            body_json(resp),
            serde_json::json!({ "app": { "name": "t" } })
        );
    }

    #[test]
    fn manifest_without_baked_json_is_500() {
        // No `with_manifest_json`: IntrospectionData is still injected, but
        // `manifest_json` is None, so the `ManifestJson` extractor errors 500.
        let router = RouterService::builder().get("/m", manifest).build();
        let req = request_builder()
            .method(Method::GET)
            .uri("/m")
            .body(Body::empty())
            .unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn routes_lists_registered_routes() {
        let router = RouterService::builder().get("/r", routes).build();
        let req = request_builder()
            .method(Method::GET)
            .uri("/r")
            .body(Body::empty())
            .unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        // Shape: [{ "method", "path" }] — the /r route itself is present.
        let body = body_json(resp);
        let arr = body.as_array().expect("routes array");
        assert!(arr
            .iter()
            .any(|entry| entry["method"] == "GET" && entry["path"] == "/r"));
    }

    #[test]
    fn config_without_store_is_not_found() {
        let router = RouterService::builder().get("/c", config).build();
        let req = request_builder()
            .method(Method::GET)
            .uri("/c")
            .body(Body::empty())
            .unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_happy_path_returns_envelope_data_secret_safe() {
        let data = serde_json::json!({ "greeting": "hi", "api_token": "demo_api_token" });
        let resp = run_config(StubStore(Ok(Some(valid_envelope_json(data)))));
        assert_eq!(resp.status(), StatusCode::OK);
        // Raw envelope `data` verbatim: the secret field holds the KEY NAME,
        // never a resolved value.
        let body = body_json(resp);
        assert_eq!(body["greeting"], "hi");
        assert_eq!(body["api_token"], "demo_api_token");
    }

    #[test]
    fn config_missing_blob_is_not_found() {
        let resp = run_config(StubStore(Ok(None)));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_backend_unavailable_maps_503() {
        let resp = run_config(StubStore(Err(ConfigStoreError::unavailable("x"))));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn config_invalid_key_maps_400() {
        let resp = run_config(StubStore(Err(ConfigStoreError::invalid_key("x"))));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn config_backend_internal_maps_500() {
        let resp = run_config(StubStore(Err(ConfigStoreError::internal(anyhow::anyhow!(
            "x"
        )))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_malformed_envelope_maps_500() {
        let resp = run_config(StubStore(Ok(Some("not json".to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_sha_mismatch_maps_500() {
        // Valid JSON envelope shape but wrong sha → verify() fails.
        let bad = r#"{"data":{"a":1},"generated_at":"t","sha256":"deadbeef","version":1}"#;
        let resp = run_config(StubStore(Ok(Some(bad.to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_unknown_version_maps_500() {
        let bad = r#"{"data":{},"generated_at":"t","sha256":"x","version":99}"#;
        let resp = run_config(StubStore(Ok(Some(bad.to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
