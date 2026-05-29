use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::config_store::CloudflareConfigStore;
use crate::context::CloudflareRequestContext;
use crate::proxy::CloudflareProxyClient;
use crate::response::from_core_response;
use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Method as CoreMethod, Request, Uri};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, KvRegistry, SecretRegistry, StoreRegistry,
};
use std::collections::BTreeMap;
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
    pub(crate) config_registry: Option<ConfigRegistry>,
    pub(crate) config_store: Option<ConfigStoreHandle>,
    pub(crate) kv: Option<KvHandle>,
    pub(crate) kv_registry: Option<KvRegistry>,
    pub(crate) secret_registry: Option<SecretRegistry>,
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

/// Dispatch a request with a Cloudflare KV-backed config store injected.
///
/// Opens `binding_name` as a KV namespace and injects a [`CloudflareConfigStore`]
/// handle whose `get` reads asynchronously from that namespace. The KV
/// namespace bound to [`DEFAULT_KV_BINDING`] is also resolved and injected
/// (non-required: missing bindings are silently skipped).
pub async fn dispatch_with_config(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    binding_name: &str,
) -> Result<CfResponse, WorkerError> {
    let config_store_handle = open_config_or_warn(&env, binding_name);
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

fn open_config_or_warn(env: &Env, binding_name: &str) -> Option<ConfigStoreHandle> {
    match CloudflareConfigStore::from_env(env, binding_name) {
        Ok(store) => Some(ConfigStoreHandle::new(Arc::new(store))),
        Err(err) => {
            warn_missing_config_binding_once(binding_name, &err.to_string());
            None
        }
    }
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
    // Hard-cutoff: see fastly's `dispatch_core_request`
    // for the rationale. Only registries go into extensions —
    // legacy bare handles are synthesised into a one-id registry
    // at the dispatch boundary.
    let (config_registry, kv_registry, secret_registry) = synthesise_store_registries(stores);
    if let Some(registry) = config_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(registry) = kv_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(registry) = secret_registry {
        core_request.extensions_mut().insert(registry);
    }
    let svc = app.router().clone();
    let response = svc
        .oneshot(core_request)
        .await
        .map_err(edge_error_to_worker)?;
    from_core_response(response).map_err(edge_error_to_worker)
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Cloudflare capability map:
/// - KV (Multi): each declared id opens its own KV namespace binding via
///   `EDGEZERO__STORES__KV__<ID>__NAME` (default = id).
/// - Config (Multi): each declared id opens its own KV namespace via
///   `EDGEZERO__STORES__CONFIG__<ID>__NAME`, read asynchronously.
/// - Secrets (Single): one shared [`crate::secret_store::CloudflareSecretStore`]
///   is registered under every declared id.
pub(crate) async fn dispatch_with_registries(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Result<CfResponse, WorkerError> {
    let kv_registry = build_kv_registry(&env, kv_meta, env_config)?;
    let config_registry = build_config_registry(&env, config_meta, env_config);
    let secret_registry = build_secret_registry(&env, secret_meta, env_config);
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
        Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Default::default()
        },
    )
    .await
}

/// Pure synthesis: collapse a `Stores` (which may carry both a
/// wired multi-id registry AND a legacy bare handle) into the
/// three registries that go into request extensions. Precedence
/// is "registry wins": a wired registry is taken verbatim; only
/// in its absence is a bare handle wrapped into a one-id registry
/// keyed under `"default"`. The bare handle is never merged in,
/// never used as a fallback for ids the registry doesn't define.
/// Pulled out as a pure function so the precedence contract is
/// unit-testable without spinning up a real `Request` and async
/// dispatcher.
fn synthesise_store_registries(
    stores: Stores,
) -> (
    Option<ConfigRegistry>,
    Option<KvRegistry>,
    Option<SecretRegistry>,
) {
    let config_registry = stores.config_registry.or_else(|| {
        stores
            .config_store
            .map(|handle| ConfigRegistry::single_id("default".to_owned(), handle))
    });
    let kv_registry = stores.kv_registry.or_else(|| {
        stores
            .kv
            .map(|handle| KvRegistry::single_id("default".to_owned(), handle))
    });
    let secret_registry = stores.secret_registry.or_else(|| {
        stores.secrets.map(|handle| {
            SecretRegistry::single_id(
                "default".to_owned(),
                BoundSecretStore::new(handle, "default".to_owned()),
            )
        })
    });
    (config_registry, kv_registry, secret_registry)
}

fn build_kv_registry(
    env: &Env,
    kv_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Result<Option<KvRegistry>, WorkerError> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };
    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let binding = env_config.store_name("kv", id);
        // Required per-id: `[stores.kv]` is declared, so failure to open is a
        // runtime error rather than a silent skip.
        let Some(handle) = resolve_kv_handle(env, &binding, true)? else {
            continue;
        };
        by_id.insert((*id).to_owned(), handle);
    }
    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            "KV registry default id `{default_id}` could not be opened; dropping the KV registry"
        );
    }
    Ok(StoreRegistry::from_parts(by_id, default_id))
}

fn build_config_registry(
    env: &Env,
    config_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    let mut by_id: BTreeMap<String, ConfigStoreHandle> = BTreeMap::new();
    for id in meta.ids {
        let binding = env_config.store_name("config", id);
        if let Some(handle) = open_config_or_warn(env, &binding) {
            by_id.insert((*id).to_owned(), handle);
        }
    }
    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            "config registry default id `{default_id}` could not be opened; dropping the config registry"
        );
    }
    StoreRegistry::from_parts(by_id, default_id)
}

fn build_secret_registry(
    env: &Env,
    secret_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Cloudflare is `Single` for secrets — one shared handle binds every id.
    // `CloudflareSecretStore::get_bytes` ignores `store_name` (worker
    // secrets are a flat namespace), so the per-id bound name is
    // observable only via [`BoundSecretStore::store_name`].
    let handle = SecretHandle::new(std::sync::Arc::new(
        crate::secret_store::CloudflareSecretStore::from_env(env.clone()),
    ));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env_config.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    // Cloudflare secret handles are infallible to construct; `from_parts`
    // keeps the API symmetric with the KV / config builders.
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
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

fn warn_missing_config_binding_once(binding: &str, error: &impl std::fmt::Display) {
    static WARNED_BINDINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned_bindings = WARNED_BINDINGS.get_or_init(|| Mutex::new(BTreeSet::new()));

    match warned_bindings.lock() {
        Ok(mut warned_bindings) => {
            if !warned_bindings.insert(binding.to_string()) {
                return;
            }
            log::warn!("config KV binding '{}' not available: {}", binding, error);
        }
        Err(_) => {
            log::warn!("config KV binding '{}' not available: {}", binding, error);
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

#[cfg(test)]
mod synthesis_tests {
    use super::*;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::key_value_store::{KvStore, NoopKvStore};
    use edgezero_core::secret_store::{NoopSecretStore, SecretHandle};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    struct StubConfig;
    #[async_trait::async_trait(?Send)]
    impl ConfigStore for StubConfig {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(None)
        }
    }

    fn kv_handle() -> KvHandle {
        let store: Arc<dyn KvStore> = Arc::new(NoopKvStore);
        KvHandle::new(store)
    }

    fn config_handle() -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(StubConfig))
    }

    fn secret_handle() -> SecretHandle {
        SecretHandle::new(Arc::new(NoopSecretStore))
    }

    #[test]
    fn synthesis_wraps_bare_kv_handle_under_default_when_no_registry() {
        let stores = Stores {
            kv: Some(kv_handle()),
            ..Default::default()
        };
        let (config, kv, secret) = synthesise_store_registries(stores);
        assert!(config.is_none());
        assert!(secret.is_none());
        let kv = kv.expect("kv registry synthesised");
        assert_eq!(kv.default_id(), "default");
        assert!(kv.named("other").is_none());
    }

    #[test]
    fn synthesis_registry_wins_over_bare_handle_when_both_wired() {
        let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
        by_id.insert("sessions".to_owned(), kv_handle());
        let registry = KvRegistry::new(by_id, "sessions".to_owned());
        let stores = Stores {
            kv: Some(kv_handle()),
            kv_registry: Some(registry),
            ..Default::default()
        };
        let (_, kv, _) = synthesise_store_registries(stores);
        let kv = kv.expect("registry survives");
        assert_eq!(kv.default_id(), "sessions");
        assert!(
            kv.named("default").is_none(),
            "bare handle's `default` synth NOT merged in"
        );
    }

    #[test]
    fn synthesis_returns_none_for_each_kind_with_no_wiring() {
        let (config, kv, secret) = synthesise_store_registries(Stores::default());
        assert!(config.is_none() && kv.is_none() && secret.is_none());
    }

    #[test]
    fn synthesis_handles_config_and_secret_bare_handles_symmetrically() {
        let stores = Stores {
            config_store: Some(config_handle()),
            secrets: Some(secret_handle()),
            ..Default::default()
        };
        let (config, _, secret) = synthesise_store_registries(stores);
        assert_eq!(config.expect("config").default_id(), "default");
        let secret = secret.expect("secret");
        assert_eq!(secret.default_id(), "default");
        assert_eq!(secret.default().expect("bound").store_name(), "default");
    }
}
