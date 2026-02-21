use crate::proxy::CloudflareProxyClient;
use crate::response::from_core_response;
use crate::CloudflareRequestContext;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Method as CoreMethod, Request, Uri};
use edgezero_core::kv::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use worker::{
    Context, Env, Error as WorkerError, Method, Request as CfRequest, Response as CfResponse,
};

/// Default Cloudflare Workers KV binding name.
///
/// If a KV namespace with this binding exists in your `wrangler.toml`,
/// it will be automatically available to handlers via the `Kv` extractor.
pub const DEFAULT_KV_BINDING: &str = "EDGEZERO_KV";

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

pub async fn dispatch(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
) -> Result<CfResponse, WorkerError> {
    dispatch_with_kv(app, req, env, ctx, DEFAULT_KV_BINDING).await
}

/// Dispatch a Cloudflare Worker request with a custom KV binding name.
pub async fn dispatch_with_kv(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    kv_binding: &str,
) -> Result<CfResponse, WorkerError> {
    // Try to open the KV binding from `env` before consuming it in `into_core_request`.
    // We borrow `env` here; `into_core_request` takes ownership afterwards.
    let kv_handle = crate::kv::CloudflareKvStore::from_env(&env, kv_binding)
        .ok()
        .map(|store| KvHandle::new(std::sync::Arc::new(store)));

    let mut core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;

    if let Some(handle) = kv_handle {
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

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

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
