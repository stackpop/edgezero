use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;

use crate::config_store::FastlyConfigStore;
use crate::key_value_store::FastlyKvStore;
use crate::proxy::FastlyProxyClient;
use crate::response::{from_core_response, parse_uri};
use crate::FastlyRequestContext;

const WARNED_STORE_CACHE_LIMIT: usize = 64;

/// Default Fastly KV Store name.
///
/// If a KV Store with this name exists in your Fastly service, it will
/// be automatically available to handlers via the `Kv` extractor.
pub const DEFAULT_KV_STORE_NAME: &str = edgezero_core::manifest::DEFAULT_KV_STORE_NAME;

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

pub(crate) fn dispatch_raw(app: &App, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
    dispatch_with_kv(app, req, DEFAULT_KV_STORE_NAME, false)
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
pub fn dispatch(app: &App, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
    dispatch_raw(app, req)
}

/// Dispatch a request with a prepared config-store handle injected into extensions.
///
/// This is the advanced/manual path. Prefer `dispatch_with_config` when you
/// want the adapter to resolve the configured backend for you.
///
/// The KV store named [`DEFAULT_KV_STORE_NAME`] is also resolved and injected
/// (non-required: unavailable stores are silently skipped).
pub fn dispatch_with_config_handle(
    app: &App,
    req: FastlyRequest,
    config_store_handle: ConfigStoreHandle,
) -> Result<FastlyResponse, FastlyError> {
    let kv_handle = resolve_kv_handle(DEFAULT_KV_STORE_NAME, false)?;
    dispatch_with_handles(app, req, Some(config_store_handle), kv_handle)
}

/// Dispatch a request with a Fastly Config Store injected into extensions.
///
/// If the named store is not available, suppresses repeated warnings for
/// recently seen store names and dispatches without it.
///
/// The KV store named [`DEFAULT_KV_STORE_NAME`] is also resolved and injected
/// (non-required: unavailable stores are silently skipped).
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
    let kv_handle = resolve_kv_handle(DEFAULT_KV_STORE_NAME, false)?;
    dispatch_with_handles(app, req, config_store_handle, kv_handle)
}

/// Dispatch a Fastly request with a custom KV store name.
///
/// `kv_required` should be `true` when `[stores.kv]` is explicitly present
/// in the manifest, causing the request to fail if the store is unavailable
/// rather than silently degrading.
pub fn dispatch_with_kv(
    app: &App,
    req: FastlyRequest,
    kv_store_name: &str,
    kv_required: bool,
) -> Result<FastlyResponse, FastlyError> {
    let kv_handle = resolve_kv_handle(kv_store_name, kv_required)?;
    dispatch_with_handles(app, req, None, kv_handle)
}

pub(crate) fn dispatch_with_store_names(
    app: &App,
    req: FastlyRequest,
    config_store_name: Option<&str>,
    kv_store_name: &str,
    kv_required: bool,
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
    let kv_handle = resolve_kv_handle(kv_store_name, kv_required)?;
    dispatch_with_handles(app, req, config_store_handle, kv_handle)
}

pub(crate) fn dispatch_with_handles(
    app: &App,
    req: FastlyRequest,
    config_store_handle: Option<ConfigStoreHandle>,
    kv_handle: Option<KvHandle>,
) -> Result<FastlyResponse, FastlyError> {
    let core_request = into_core_request(req).map_err(map_edge_error)?;
    dispatch_core_request(app, core_request, config_store_handle, kv_handle)
}

fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    config_store_handle: Option<ConfigStoreHandle>,
    kv_handle: Option<KvHandle>,
) -> Result<FastlyResponse, FastlyError> {
    if let Some(handle) = config_store_handle {
        core_request.extensions_mut().insert(handle);
    }

    if let Some(handle) = kv_handle {
        core_request.extensions_mut().insert(handle);
    }

    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
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
                    "KV store '{}' is explicitly configured but could not be opened: {}",
                    kv_store_name, err
                )));
            }
            warn_missing_kv_store_once(kv_store_name, &err);
            Ok(None)
        }
    }
}

fn warn_missing_store_once(store_name: &str, detail: &str) {
    let warned = warned_store_cache().get_or_init(|| Mutex::new(RecentStringSet::default()));
    let mut warned = warned
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.insert(store_name, WARNED_STORE_CACHE_LIMIT) {
        log::warn!(
            "configured Fastly config store '{}' is unavailable ({}); skipping config-store injection",
            store_name,
            detail
        );
    }
}

fn warned_store_cache() -> &'static OnceLock<Mutex<RecentStringSet>> {
    static WARNED_STORES: OnceLock<Mutex<RecentStringSet>> = OnceLock::new();
    &WARNED_STORES
}

#[derive(Default)]
struct RecentStringSet {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentStringSet {
    fn insert(&mut self, key: &str, limit: usize) -> bool {
        if self.keys.contains(key) {
            return false;
        }

        if limit == 0 {
            return true;
        }

        if self.order.len() >= limit {
            if let Some(oldest) = self.order.pop_front() {
                self.keys.remove(&oldest);
            }
        }

        let key = key.to_string();
        self.keys.insert(key.clone());
        self.order.push_back(key);
        true
    }
}

fn map_edge_error(err: EdgeError) -> FastlyError {
    FastlyError::msg(err.to_string())
}

fn warn_missing_kv_store_once(kv_store_name: &str, error: &impl std::fmt::Display) {
    static WARNED_STORES: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned_stores = WARNED_STORES.get_or_init(|| Mutex::new(BTreeSet::new()));

    match warned_stores.lock() {
        Ok(mut warned_stores) => {
            if !warned_stores.insert(kv_store_name.to_string()) {
                return;
            }
            log::warn!("KV store '{}' not available: {}", kv_store_name, error);
        }
        Err(_) => {
            log::warn!("KV store '{}' not available: {}", kv_store_name, error);
        }
    }
}
