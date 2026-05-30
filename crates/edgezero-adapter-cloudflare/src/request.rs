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

/// Cloudflare per-request dispatch service.
///
/// Builds a Worker invocation with the stores the operator wants
/// injected into request extensions, then dispatches one request
/// against the wrapped `App`. The store wiring is a per-Service
/// decision; on Cloudflare Workers that means per-request (the
/// runtime invokes the entrypoint per HTTP request), but the
/// Service type itself is cheap to build.
///
/// Replaces the prior `dispatch_with_*` variant fan-out. Each
/// builder method is independent: enable any combination of KV,
/// config, and secret stores by chaining the relevant `with_*` /
/// `require_*` calls. The manifest-driven `run_app` is still the
/// recommended entrypoint for normal flows -- the Service builder
/// is for manual / no-manifest deployments.
///
/// ```rust,ignore
/// CloudflareService::new(&app)
///     .with_kv("sessions").require_kv()
///     .with_config("app_config")
///     .with_secrets()
///     .dispatch(req, env, ctx).await
/// ```
pub struct CloudflareService<'app> {
    app: &'app App,
    config: ConfigSource,
    kv: Option<KvSource>,
    secrets: SecretSource,
}

enum ConfigSource {
    Binding(String),
    Handle(ConfigStoreHandle),
    None,
}

struct KvSource {
    binding: String,
    required: bool,
}

enum SecretSource {
    Off,
    On { required: bool },
}

impl<'app> CloudflareService<'app> {
    /// Resolve every wired store at request time and dispatch
    /// against the wrapped `App`. `env` and `ctx` come from the
    /// Worker runtime per request, NOT the Service builder.
    /// Consumes the service so a builder can't be reused with stale
    /// wiring.
    pub async fn dispatch(
        self,
        req: CfRequest,
        env: Env,
        ctx: Context,
    ) -> Result<CfResponse, WorkerError> {
        let config_store = match self.config {
            ConfigSource::Binding(binding) => open_config_or_warn(&env, &binding),
            ConfigSource::Handle(handle) => Some(handle),
            ConfigSource::None => None,
        };
        let kv = match self.kv {
            Some(source) => resolve_kv_handle(&env, &source.binding, source.required)?,
            None => None,
        };
        let secrets = match self.secrets {
            SecretSource::Off => None,
            SecretSource::On { required } => resolve_secret_handle(&env, required),
        };
        dispatch_with_handles(
            self.app,
            req,
            env,
            ctx,
            Stores {
                config_store,
                kv,
                secrets,
                ..Default::default()
            },
        )
        .await
    }

    /// Build a new service that dispatches against `app` with NO
    /// stores wired. Chain `.with_*` / `.require_*` to add stores.
    #[must_use]
    #[inline]
    pub fn new(app: &'app App) -> Self {
        Self {
            app,
            config: ConfigSource::None,
            kv: None,
            secrets: SecretSource::Off,
        }
    }

    /// Promote the previously-wired KV binding to required: an
    /// unavailable namespace causes dispatch to return an error.
    /// No-op when `with_kv` wasn't called.
    #[must_use]
    #[inline]
    pub fn require_kv(mut self) -> Self {
        if let Some(kv) = self.kv.as_mut() {
            kv.required = true;
        }
        self
    }

    /// Promote the previously-wired secret store to required.
    /// No-op when `with_secrets` wasn't called.
    #[must_use]
    #[inline]
    pub fn require_secrets(mut self) -> Self {
        if let SecretSource::On { ref mut required } = self.secrets {
            *required = true;
        }
        self
    }

    /// Open the KV namespace bound as `binding` (per `wrangler.toml`)
    /// as a Cloudflare config store and inject its handle. If the
    /// binding is absent the dispatcher logs once and proceeds
    /// without it.
    #[must_use]
    #[inline]
    pub fn with_config<S: Into<String>>(mut self, binding: S) -> Self {
        self.config = ConfigSource::Binding(binding.into());
        self
    }

    /// Inject a pre-built `ConfigStoreHandle`. Use this when the
    /// caller has already opened (or mocked) the backend. Mutually
    /// exclusive with `with_config(binding)` -- the last call wins.
    #[must_use]
    #[inline]
    pub fn with_config_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config = ConfigSource::Handle(handle);
        self
    }

    /// Open the KV namespace bound as `binding` and inject its
    /// handle. Non-required by default: an absent binding logs
    /// once and dispatch continues. Pair with `require_kv()` when
    /// the manifest declares `[stores.kv]`.
    #[must_use]
    #[inline]
    pub fn with_kv<S: Into<String>>(mut self, binding: S) -> Self {
        self.kv = Some(KvSource {
            binding: binding.into(),
            required: false,
        });
        self
    }

    /// Enable Cloudflare Worker secrets and inject the secret-store
    /// handle. Worker secrets have no namespace concept, so no
    /// name is needed. Non-required by default; pair with
    /// `require_secrets()` when the manifest declares
    /// `[stores.secrets]`. Individual missing secrets surface as
    /// `SecretError::NotFound` at access time.
    #[must_use]
    #[inline]
    pub fn with_secrets(mut self) -> Self {
        self.secrets = SecretSource::On { required: false };
        self
    }
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
        // BoundSecretStore binds the synthesised secret to platform
        // store name "default". A handler reading via
        // `ctx.secret_store_default()?.require_str(key)` resolves
        // the cloudflare Worker Secret literally named "default";
        // if the operator's wrangler.toml uses a different name,
        // the runtime require_str() surfaces a clear store-name
        // error rather than a silent miss.
        assert_eq!(secret.default().expect("bound").store_name(), "default");
    }
}
