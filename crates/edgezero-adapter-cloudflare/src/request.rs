use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::config_store::CloudflareConfigStore;
use crate::proxy::CloudflareProxyClient;
use crate::response::from_core_response;
use crate::CloudflareRequestContext;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Method as CoreMethod, Request, Uri};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use worker::{
    Context, Env, Error as WorkerError, Method, Request as CfRequest, Response as CfResponse,
};

/// Default Cloudflare Workers KV binding name.
///
/// If a KV namespace with this binding exists in your `wrangler.toml`,
/// it will be automatically available to handlers via the `Kv` extractor.
pub const DEFAULT_KV_BINDING: &str = edgezero_core::manifest::DEFAULT_KV_STORE_NAME;

/// Groups the optional per-request store handles injected at dispatch time.
///
/// Use `..Default::default()` for fields you do not need:
///
/// ```rust,ignore
/// let stores = Stores { kv: Some(kv_handle), ..Default::default() };
/// ```
#[derive(Default)]
pub(crate) struct Stores {
    pub(crate) config_store: Option<ConfigStoreHandle>,
    pub(crate) kv: Option<KvHandle>,
    pub(crate) secrets: Option<SecretHandle>,
}

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
    dispatch_with_kv(app, req, env, ctx, DEFAULT_KV_BINDING, false).await
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

/// Dispatch a Cloudflare Worker request with a custom KV binding name.
///
/// `kv_required` should be `true` when `[stores.kv]` is explicitly present
/// in the manifest, causing the request to fail if the binding is unavailable
/// rather than silently degrading.
pub async fn dispatch_with_kv(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    kv_binding: &str,
    kv_required: bool,
) -> Result<CfResponse, WorkerError> {
    let kv = resolve_kv_handle(&env, kv_binding, kv_required)?;
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            kv,
            ..Default::default()
        },
    )
    .await
}

/// Dispatch a request with a prepared config-store handle injected.
///
/// This is the advanced/manual path. Prefer `dispatch_with_config` when you
/// want the adapter to resolve the configured backend for you.
///
/// The KV namespace bound to [`DEFAULT_KV_BINDING`] is also resolved and injected
/// (non-required: missing bindings are silently skipped).
pub async fn dispatch_with_config_handle(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    config_store_handle: ConfigStoreHandle,
) -> Result<CfResponse, WorkerError> {
    let kv = resolve_kv_handle(&env, DEFAULT_KV_BINDING, false)?;
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            config_store: Some(config_store_handle),
            kv,
            ..Default::default()
        },
    )
    .await
}

/// Dispatch a request with a Cloudflare JSON config store injected.
///
/// Reads `binding_name` from `env` (a `[vars]` string whose value is a JSON object),
/// parses it into a `CloudflareConfigStore`, and injects the handle before dispatch
/// when the binding is present and valid.
///
/// The KV namespace bound to [`DEFAULT_KV_BINDING`] is also resolved and injected
/// (non-required: missing bindings are silently skipped).
pub async fn dispatch_with_config(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    binding_name: &str,
) -> Result<CfResponse, WorkerError> {
    let config_store_handle = CloudflareConfigStore::try_new(&env, binding_name)
        .map(|store| ConfigStoreHandle::new(Arc::new(store)));
    let kv = resolve_kv_handle(&env, DEFAULT_KV_BINDING, false)?;
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            config_store: config_store_handle,
            kv,
            ..Default::default()
        },
    )
    .await
}

pub(crate) async fn dispatch_with_bindings(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    config_binding: Option<&str>,
    kv_binding: &str,
    kv_required: bool,
    secrets_required: bool,
) -> Result<CfResponse, WorkerError> {
    let config_store_handle = config_binding.and_then(|binding_name| {
        CloudflareConfigStore::try_new(&env, binding_name)
            .map(|store| ConfigStoreHandle::new(Arc::new(store)))
    });
    let kv = resolve_kv_handle(&env, kv_binding, kv_required)?;
    let secrets = resolve_secret_handle(&env, secrets_required);
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            config_store: config_store_handle,
            kv,
            secrets,
        },
    )
    .await
}

/// Dispatch a Cloudflare Worker request with a secret store attached (no KV store).
///
/// Use this when your application accesses secrets but does not need a KV store.
/// For applications that need both, use [`dispatch_with_kv_and_secrets`] instead.
///
/// For most applications, prefer [`crate::run_app`] which resolves all stores
/// from the manifest automatically. Use `dispatch_with_secrets` only when you
/// need direct control over the dispatch lifecycle without a manifest.
///
/// The store is only attached when `secrets_required` is `true`.
/// Individual missing secrets surface as `SecretError::NotFound` at access time.
pub async fn dispatch_with_secrets(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    secrets_required: bool,
) -> Result<CfResponse, WorkerError> {
    let secrets = resolve_secret_handle(&env, secrets_required);
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            secrets,
            ..Default::default()
        },
    )
    .await
}

/// Dispatch a Cloudflare Worker request with both KV and secret stores attached.
///
/// Note: Cloudflare secrets have no namespace concept, so no secret binding name is needed.
///
/// For most applications, prefer [`crate::run_app`] which resolves all stores
/// from the manifest automatically. Use `dispatch_with_kv_and_secrets` only
/// when you need direct control over the dispatch lifecycle without a manifest.
pub async fn dispatch_with_kv_and_secrets(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    kv_binding: &str,
    kv_required: bool,
    secrets_required: bool,
) -> Result<CfResponse, WorkerError> {
    let kv = resolve_kv_handle(&env, kv_binding, kv_required)?;
    let secrets = resolve_secret_handle(&env, secrets_required);
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            kv,
            secrets,
            ..Default::default()
        },
    )
    .await
}

pub(crate) async fn dispatch_with_handles(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    stores: Stores,
) -> Result<CfResponse, WorkerError> {
    let core_request = into_core_request(req, env, ctx)
        .await
        .map_err(edge_error_to_worker)?;
    dispatch_core_request(app, core_request, stores).await
}

async fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    stores: Stores,
) -> Result<CfResponse, WorkerError> {
    if let Some(handle) = stores.config_store {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(handle) = stores.kv {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(handle) = stores.secrets {
        core_request.extensions_mut().insert(handle);
    }
    let svc = app.router().clone();
    let response = svc.oneshot(core_request).await;
    from_core_response(response).map_err(edge_error_to_worker)
}

pub(crate) fn resolve_kv_handle(
    env: &Env,
    kv_binding: &str,
    kv_required: bool,
) -> Result<Option<KvHandle>, WorkerError> {
    match crate::key_value_store::CloudflareKvStore::from_env(env, kv_binding) {
        Ok(store) => Ok(Some(KvHandle::new(std::sync::Arc::new(store)))),
        Err(e) => {
            if kv_required {
                return Err(WorkerError::RustError(format!(
                    "KV binding '{}' is explicitly configured but could not be opened: {}",
                    kv_binding, e
                )));
            }
            warn_missing_kv_binding_once(kv_binding, &e);
            Ok(None)
        }
    }
}

pub(crate) fn resolve_secret_handle(env: &Env, secrets_required: bool) -> Option<SecretHandle> {
    if !secrets_required {
        return None;
    }

    let secret_store = crate::secret_store::CloudflareSecretStore::from_env(env.clone());
    Some(SecretHandle::new(std::sync::Arc::new(secret_store)))
}

fn edge_error_to_worker(err: EdgeError) -> WorkerError {
    WorkerError::RustError(err.to_string())
}

fn warn_missing_kv_binding_once(kv_binding: &str, error: &impl std::fmt::Display) {
    static WARNED_BINDINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned_bindings = WARNED_BINDINGS.get_or_init(|| Mutex::new(BTreeSet::new()));

    match warned_bindings.lock() {
        Ok(mut warned_bindings) => {
            if !warned_bindings.insert(kv_binding.to_string()) {
                return;
            }
            log::warn!("KV binding '{}' not available: {}", kv_binding, error);
        }
        Err(_) => {
            log::warn!("KV binding '{}' not available: {}", kv_binding, error);
        }
    }
}

fn into_core_method(method: Method) -> CoreMethod {
    let bytes = method.as_ref().as_bytes();
    CoreMethod::from_bytes(bytes).unwrap_or_else(|_| {
        log::warn!(
            "unknown HTTP method {:?}, defaulting to GET",
            method.as_ref()
        );
        CoreMethod::GET
    })
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
