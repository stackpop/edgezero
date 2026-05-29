use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::io::Read as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::manifest::DEFAULT_KV_STORE_NAME as CORE_DEFAULT_KV_STORE_NAME;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, KvRegistry, SecretRegistry, StoreRegistry,
};
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;
use std::collections::BTreeMap;

use crate::config_store::FastlyConfigStore;
use crate::context::FastlyRequestContext;
use crate::key_value_store::FastlyKvStore;
use crate::proxy::FastlyProxyClient;
use crate::response::{from_core_response, parse_uri};
use crate::secret_store::FastlySecretStore;

/// Default Fastly KV Store name.
///
/// If a KV Store with this name exists in your Fastly service, it will
/// be automatically available to handlers via the `Kv` extractor.
pub const DEFAULT_KV_STORE_NAME: &str = CORE_DEFAULT_KV_STORE_NAME;

const WARNED_STORE_CACHE_LIMIT: usize = 64;

#[derive(Default)]
struct RecentStringSet {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentStringSet {
    fn insert(&mut self, key: &str, limit: usize) -> bool {
        let owned = key.to_owned();
        if !self.keys.insert(owned.clone()) {
            return false;
        }
        self.order.push_back(owned);
        while limit > 0 && self.order.len() > limit {
            if let Some(oldest) = self.order.pop_front() {
                self.keys.remove(&oldest);
            }
        }
        true
    }
}

/// Groups the optional per-request store handles injected at dispatch time.
///
/// Use `..Default::default()` for fields you do not need:
///
/// ```rust,ignore
/// let stores = Stores { kv: Some(kv_handle), ..Default::default() };
/// ```
#[derive(Default)]
struct Stores {
    config_registry: Option<ConfigRegistry>,
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    secret_registry: Option<SecretRegistry>,
    secrets: Option<SecretHandle>,
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
/// # Errors
/// Returns an error if request conversion fails or the underlying handler returns an error.
#[inline]
pub fn dispatch(app: &App, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
    dispatch_raw(app, req)
}

fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    stores: Stores,
) -> Result<FastlyResponse, FastlyError> {
    // Hard-cutoff: legacy bare handles are no longer
    // inserted into request extensions. `dispatch_with_config_handle`
    // still accepts a `ConfigStoreHandle`, but the dispatcher
    // synthesises a one-id `<kind>Registry` from any wired handle
    // and only the registry goes into extensions. The
    // `ctx.{config,kv,secret}_handle()` accessors are gone; handlers
    // use `ctx.{config,kv,secret}_store_default()` or the
    // `Kv` / `Config` / `Secrets` extractors.
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
    let response = executor::block_on(app.router().oneshot(core_request))
        .map_err(|err| map_edge_error(&err))?;
    from_core_response(response).map_err(|err| map_edge_error(&err))
}

pub(crate) fn dispatch_raw(app: &App, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
    dispatch_with_kv(app, req, DEFAULT_KV_STORE_NAME, false)
}

/// Dispatch a request with a Fastly Config Store injected into extensions.
///
/// If the named store is not available, suppresses repeated warnings for
/// recently seen store names and dispatches without it.
///
/// The KV store named [`DEFAULT_KV_STORE_NAME`] is also resolved and injected
/// (non-required: unavailable stores are silently skipped).
///
/// # Errors
/// Returns an error if the named config store cannot be opened or the underlying handler returns an error.
#[inline]
pub fn dispatch_with_config(
    app: &App,
    req: FastlyRequest,
    store_name: &str,
) -> Result<FastlyResponse, FastlyError> {
    let config_store_handle = match FastlyConfigStore::try_open(store_name) {
        Ok(store) => Some(ConfigStoreHandle::new(Arc::new(store))),
        Err(err) => {
            warn_missing_store_once(store_name, &err.to_string());
            None
        }
    };
    let kv = resolve_kv_handle(DEFAULT_KV_STORE_NAME, false)?;
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_store: config_store_handle,
            kv,
            ..Default::default()
        },
    )
}

/// Dispatch a request with a prepared config-store handle injected into extensions.
///
/// This is the advanced/manual path. Prefer `dispatch_with_config` when you
/// want the adapter to resolve the configured backend for you.
///
/// The KV store named [`DEFAULT_KV_STORE_NAME`] is also resolved and injected
/// (non-required: unavailable stores are silently skipped).
///
/// # Errors
/// Returns an error if request conversion fails or the underlying handler returns an error.
#[inline]
pub fn dispatch_with_config_handle(
    app: &App,
    req: FastlyRequest,
    config_store_handle: ConfigStoreHandle,
) -> Result<FastlyResponse, FastlyError> {
    let kv = resolve_kv_handle(DEFAULT_KV_STORE_NAME, false)?;
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_store: Some(config_store_handle),
            kv,
            ..Default::default()
        },
    )
}

fn dispatch_with_handles(
    app: &App,
    req: FastlyRequest,
    stores: Stores,
) -> Result<FastlyResponse, FastlyError> {
    let core_request = into_core_request(req).map_err(|err| map_edge_error(&err))?;
    dispatch_core_request(app, core_request, stores)
}

/// Dispatch a Fastly request with a custom KV store name.
///
/// `kv_required` should be `true` when `[stores.kv]` is explicitly present
/// in the manifest, causing the request to fail if the store is unavailable
/// rather than silently degrading.
///
/// # Errors
/// Returns an error if the named KV store cannot be opened or the underlying handler returns an error.
#[inline]
pub fn dispatch_with_kv(
    app: &App,
    req: FastlyRequest,
    kv_store_name: &str,
    kv_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let kv = resolve_kv_handle(kv_store_name, kv_required)?;
    dispatch_with_handles(
        app,
        req,
        Stores {
            kv,
            ..Default::default()
        },
    )
}

/// Dispatch a Fastly request with both KV and secret stores attached.
///
/// For most applications, prefer [`crate::run_app`] which resolves all stores
/// from the manifest automatically. Use `dispatch_with_kv_and_secrets` only
/// when you need direct control over the dispatch lifecycle without a manifest.
///
/// # Errors
/// Returns an error if a required store cannot be opened or the underlying handler returns an error.
#[inline]
pub fn dispatch_with_kv_and_secrets(
    app: &App,
    req: FastlyRequest,
    kv_store_name: &str,
    kv_required: bool,
    secrets_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let kv = resolve_kv_handle(kv_store_name, kv_required)?;
    let secrets = resolve_secret_handle(secrets_required);
    dispatch_with_handles(
        app,
        req,
        Stores {
            kv,
            secrets,
            ..Default::default()
        },
    )
}

/// Dispatch a Fastly request with a secret store attached.
///
/// For most applications, prefer [`crate::run_app`] which resolves all stores
/// from the manifest automatically. Use `dispatch_with_secrets` only when you
/// need direct control over the dispatch lifecycle without a manifest.
///
/// Platform-name binding: the synthesised `SecretRegistry` binds
/// the handle to a `BoundSecretStore` whose underlying Fastly
/// Secret Store name is the literal string `"default"`. So
/// handlers reading `ctx.secret_store_default()?.require_str(key)`
/// open a Fastly Secret Store named `"default"` -- the operator's
/// Fastly account must have a Secret Store with that exact name,
/// or the runtime `require_str` will surface a clear store-name
/// error. Use `dispatch_with_kv_and_secrets` (or the manifest-aware
/// `run_app`) if your account uses a different store name.
///
/// # Errors
/// Returns an error if the named secret store is required but cannot be opened, or the underlying handler returns an error.
#[inline]
pub fn dispatch_with_secrets(
    app: &App,
    req: FastlyRequest,
    secrets_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let secrets = resolve_secret_handle(secrets_required);
    dispatch_with_handles(
        app,
        req,
        Stores {
            secrets,
            ..Default::default()
        },
    )
}

pub(crate) fn dispatch_with_store_names(
    app: &App,
    req: FastlyRequest,
    config_store_name: Option<&str>,
    kv_store_name: &str,
    kv_required: bool,
    secrets_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let config_store_handle = match config_store_name {
        Some(store_name) => match FastlyConfigStore::try_open(store_name) {
            Ok(store) => Some(ConfigStoreHandle::new(Arc::new(store))),
            Err(err) => {
                warn_missing_store_once(store_name, &err.to_string());
                None
            }
        },
        None => None,
    };
    let kv = resolve_kv_handle(kv_store_name, kv_required)?;
    let secrets = resolve_secret_handle(secrets_required);
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_store: config_store_handle,
            kv,
            secrets,
            ..Default::default()
        },
    )
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Fastly is `Multi` for all three kinds, so each declared id resolves to
/// its own platform store via `EDGEZERO__STORES__<KIND>__<ID>__NAME` (or the
/// id default). KV failures escalate when `kv_required` is set; missing
/// config / secret stores degrade silently with a one-time warning.
pub(crate) fn dispatch_with_registries(
    app: &App,
    req: FastlyRequest,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Result<FastlyResponse, FastlyError> {
    let kv_registry = build_kv_registry(kv_meta, env)?;
    let config_registry = build_config_registry(config_meta, env);
    let secret_registry = build_secret_registry(secret_meta, env);
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Default::default()
        },
    )
}

/// Pure synthesis: collapse a `Stores` (which may carry both a
/// wired multi-id registry AND a legacy bare handle) into the
/// three registries that go into request extensions. Precedence
/// is "registry wins": a wired registry is taken verbatim; only
/// in its absence is a bare handle wrapped into a one-id registry
/// keyed under `"default"`. The bare handle is never merged
/// in, never used as a fallback for ids the registry doesn't
/// define. Pulled out as a pure function so the precedence
/// contract is unit-testable without spinning up a real
/// `Request` and async dispatcher.
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
    kv_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Result<Option<KvRegistry>, FastlyError> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };
    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("kv", id);
        // KV is required: if `[stores.kv]` is declared, an id failing to open
        // is a runtime error rather than a silent degradation.
        let Some(handle) = resolve_kv_handle(&store_name, true)? else {
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
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    let mut by_id: BTreeMap<String, ConfigStoreHandle> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("config", id);
        match FastlyConfigStore::try_open(&store_name) {
            Ok(store) => {
                by_id.insert((*id).to_owned(), ConfigStoreHandle::new(Arc::new(store)));
            }
            Err(err) => warn_missing_store_once(&store_name, &err.to_string()),
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
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Fastly is `Multi` for secrets. The provider trait is stateless —
    // `FastlySecretStore::get_bytes(store_name, key)` opens the named Fastly
    // Secret Store per call — so we share one provider handle across all
    // bindings, then capture the per-id platform store name in the bound
    // wrapper. `EDGEZERO__STORES__SECRETS__<ID>__NAME` (default = the logical
    // id) decides which Fastly store each id resolves to at runtime.
    let handle = SecretHandle::new(Arc::new(FastlySecretStore));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    // Fastly's secret-store handle wrappers are infallible to construct;
    // `from_parts` keeps the API symmetric with the KV / config builders.
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

/// # Errors
/// Returns [`EdgeError::Internal`] if the Fastly request cannot be reconstituted into a core request (e.g., method or URI conversion failure).
#[inline]
pub fn into_core_request(mut req: FastlyRequest) -> Result<Request, EdgeError> {
    let method = req.get_method().clone();
    let uri = parse_uri(req.get_url_str())?;

    let mut builder = request_builder().method(method).uri(uri);
    for (name, value) in req.get_headers() {
        builder = builder.header(name.as_str(), value.as_bytes());
    }

    let mut body = req.take_body();
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).map_err(EdgeError::internal)?;

    let mut request = builder
        .body(Body::from(bytes))
        .map_err(EdgeError::internal)?;

    let context = FastlyRequestContext {
        client_ip: req.get_client_ip_addr(),
    };
    FastlyRequestContext::insert(&mut request, context);
    request
        .extensions_mut()
        .insert(ProxyHandle::with_client(FastlyProxyClient));

    Ok(request)
}

fn map_edge_error(err: &EdgeError) -> FastlyError {
    FastlyError::msg(err.to_string())
}

fn resolve_kv_handle(
    kv_store_name: &str,
    kv_required: bool,
) -> Result<Option<KvHandle>, FastlyError> {
    match FastlyKvStore::open(kv_store_name) {
        Ok(store) => Ok(Some(KvHandle::new(Arc::new(store)))),
        Err(err) => {
            if kv_required {
                return Err(FastlyError::msg(format!(
                    "KV store '{kv_store_name}' is explicitly configured but could not be opened: {err}"
                )));
            }
            warn_missing_kv_store_once(kv_store_name, &err);
            Ok(None)
        }
    }
}

fn resolve_secret_handle(secrets_required: bool) -> Option<SecretHandle> {
    if !secrets_required {
        return None;
    }
    Some(SecretHandle::new(Arc::new(FastlySecretStore)))
}

fn warn_missing_kv_store_once(kv_store_name: &str, error: &impl Display) {
    static WARNED_KV_STORES: OnceLock<Mutex<RecentStringSet>> = OnceLock::new();
    warn_missing_once(&WARNED_KV_STORES, "KV store", kv_store_name, error);
}

fn warn_missing_once(
    cache: &'static OnceLock<Mutex<RecentStringSet>>,
    item_type: &str,
    name: &str,
    detail: &impl Display,
) {
    let set = cache.get_or_init(|| Mutex::new(RecentStringSet::default()));
    let mut guard = set.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.insert(name, WARNED_STORE_CACHE_LIMIT) {
        log::warn!("{item_type} '{name}' not available: {detail}");
    }
}

fn warn_missing_store_once(store_name: &str, detail: &str) {
    static WARNED_STORES: OnceLock<Mutex<RecentStringSet>> = OnceLock::new();
    warn_missing_once(
        &WARNED_STORES,
        "configured Fastly config store",
        store_name,
        &format!("{detail}; skipping config-store injection"),
    );
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
        let (config_out, kv_out, secret_out) = synthesise_store_registries(stores);
        assert!(
            config_out.is_none(),
            "no config wiring -> no config registry"
        );
        assert!(
            secret_out.is_none(),
            "no secret wiring -> no secret registry"
        );
        let kv_reg = kv_out.expect("kv registry synthesised from bare handle");
        assert_eq!(
            kv_reg.default_id(),
            "default",
            "synthesised id is `default`"
        );
        assert!(kv_reg.named("default").is_some());
        assert!(
            kv_reg.named("other").is_none(),
            "synthesised registry only knows the `default` id"
        );
    }

    #[test]
    fn synthesis_registry_wins_over_bare_handle_when_both_wired() {
        // Multi-id registry declaring only `sessions` paired with a
        // bare handle that would otherwise synthesise to a
        // `default`-keyed entry. Precedence rule: the bare handle
        // is dropped entirely; the registry stands alone with no
        // `default` id.
        let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
        by_id.insert("sessions".to_owned(), kv_handle());
        let registry = KvRegistry::new(by_id, "sessions".to_owned());
        let stores = Stores {
            kv: Some(kv_handle()),
            kv_registry: Some(registry),
            ..Default::default()
        };
        let (_, kv_out, _) = synthesise_store_registries(stores);
        let kv_reg = kv_out.expect("registry survives synthesis");
        assert_eq!(kv_reg.default_id(), "sessions");
        assert!(
            kv_reg.named("default").is_none(),
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
        let (config_out, _, secret_out) = synthesise_store_registries(stores);
        let config_reg = config_out.expect("config wrapped");
        assert_eq!(config_reg.default_id(), "default");
        let secret_reg = secret_out.expect("secret wrapped");
        assert_eq!(secret_reg.default_id(), "default");
        // BoundSecretStore binds the synthesised secret to platform
        // store name "default" -- if the underlying Fastly account
        // has no Secret Store literally named "default", the
        // require_str() call from a handler will fail with a clear
        // store-name error rather than silent miss.
        assert_eq!(
            secret_reg.default().expect("default bound").store_name(),
            "default"
        );
    }
}
