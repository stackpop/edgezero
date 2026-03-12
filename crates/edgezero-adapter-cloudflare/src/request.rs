use std::sync::Arc;

use crate::config_store::CloudflareConfigStore;
use crate::proxy::CloudflareProxyClient;
use crate::response::from_core_response;
use crate::CloudflareRequestContext;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Method as CoreMethod, Request, Uri};
use edgezero_core::proxy::ProxyHandle;
use worker::{
    Context, Env, Error as WorkerError, Method, Request as CfRequest, Response as CfResponse,
};

use wasm_bindgen_test::wasm_bindgen_test;

pub async fn into_core_request(
    mut req: CfRequest,
    env: Env,
    ctx: Context,
) -> Result<Request, EdgeError> {
    let method = into_core_method(req.method());
    let url = req
        .url()
        .map_err(|err| EdgeError::bad_request(format!("invalid URL: {}", err)))?;
    let uri: Uri = url
        .as_str()
        .parse()
        .map_err(|err| EdgeError::bad_request(format!("invalid URI: {}", err)))?;

    let mut builder = request_builder().method(method).uri(uri);
    let headers = req.headers();
    for (name, value) in headers.entries() {
        builder = builder.header(name.as_str(), value);
    }

    let bytes = req.bytes().await.map_err(EdgeError::internal)?;

    let mut request = builder
        .body(Body::from(bytes))
        .map_err(EdgeError::internal)?;

    CloudflareRequestContext::insert(&mut request, env, ctx);
    request
        .extensions_mut()
        .insert(ProxyHandle::with_client(CloudflareProxyClient));
    Ok(request)
}

pub(crate) async fn dispatch_raw(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
) -> Result<CfResponse, WorkerError> {
    let core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;
    dispatch_core_request(app, core_request, None).await
}

/// Low-level manual dispatch.
///
/// This path does not resolve or inject config-store metadata from a manifest.
/// Prefer `run_app` or `dispatch_with_config` for normal config-store-aware
/// dispatch. Use `dispatch_with_config_handle` only when you already have a
/// prepared `ConfigStoreHandle`.
#[deprecated(
    note = "dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_handle()"
)]
pub async fn dispatch(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
) -> Result<CfResponse, WorkerError> {
    dispatch_raw(app, req, env, ctx).await
}

/// Dispatch a request with a prepared config-store handle injected.
///
/// This is the advanced/manual path. Prefer `dispatch_with_config` when you
/// want the adapter to resolve the configured backend for you.
pub async fn dispatch_with_config_handle(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    config_store_handle: ConfigStoreHandle,
) -> Result<CfResponse, WorkerError> {
    let core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;
    dispatch_core_request(app, core_request, Some(config_store_handle)).await
}

/// Dispatch a request with a Cloudflare JSON config store injected.
///
/// Reads `binding_name` from `env` (a `[vars]` string whose value is a JSON object),
/// parses it into a `CloudflareConfigStore`, and injects the handle before dispatch
/// when the binding is present and valid.
pub async fn dispatch_with_config(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    binding_name: &str,
) -> Result<CfResponse, WorkerError> {
    let config_handle = CloudflareConfigStore::try_new(&env, binding_name)
        .map(|store| ConfigStoreHandle::new(Arc::new(store)));
    let core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;
    dispatch_core_request(app, core_request, config_handle).await
}

async fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    config_store_handle: Option<ConfigStoreHandle>,
) -> Result<CfResponse, WorkerError> {
    if let Some(handle) = config_store_handle {
        core_request.extensions_mut().insert(handle);
    }
    let svc = app.router().clone();
    let response = svc.oneshot(core_request).await;
    from_core_response(response).map_err(edge_error_to_worker)
}

fn edge_error_to_worker(err: EdgeError) -> WorkerError {
    WorkerError::RustError(err.to_string())
}

fn into_core_method(method: Method) -> CoreMethod {
    CoreMethod::from_bytes(method.as_ref().as_bytes()).unwrap_or(CoreMethod::GET)
}

mod tests {
    use super::*;

    #[wasm_bindgen_test]
    fn into_http_method_maps_known_methods() {
        assert_eq!(into_core_method(Method::Get), CoreMethod::GET);
        assert_eq!(into_core_method(Method::Post), CoreMethod::POST);
        assert_eq!(into_core_method(Method::Put), CoreMethod::PUT);
        assert_eq!(into_core_method(Method::Delete), CoreMethod::DELETE);
    }

    #[wasm_bindgen_test]
    fn into_http_method_defaults_unknown_to_get() {
        let method = Method::from("FOO".to_string());
        assert_eq!(into_core_method(method), CoreMethod::GET);
    }
}
