use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::io::Read as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::manifest::DEFAULT_KV_STORE_NAME as CORE_DEFAULT_KV_STORE_NAME;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;

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
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
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
    if let Some(handle) = stores.config_store {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(handle) = stores.kv {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(handle) = stores.secrets {
        core_request.extensions_mut().insert(handle);
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
/// Missing or unreadable config stores are logged and skipped — not
/// surfaced as errors. Returns an error only if request conversion, KV
/// resolution, handler dispatch, or response conversion fails.
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
/// Returns an error if request conversion, handler dispatch, or response
/// conversion fails. Also returns an error when `kv_required` is `true` and
/// the named KV store cannot be opened; if `kv_required` is `false`, missing
/// stores are logged and dispatched without the KV handle injected.
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
        },
    )
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
