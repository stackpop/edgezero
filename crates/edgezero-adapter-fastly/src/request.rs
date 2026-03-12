use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};

use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::proxy::ProxyHandle;
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;

use crate::config_store::FastlyConfigStore;
use crate::proxy::FastlyProxyClient;
use crate::response::{from_core_response, parse_uri};
use crate::FastlyRequestContext;

const WARNED_STORE_CACHE_LIMIT: usize = 64;

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
    let core_request = into_core_request(req).map_err(map_edge_error)?;
    dispatch_core_request(app, core_request, None)
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
pub fn dispatch_with_config_handle(
    app: &App,
    req: FastlyRequest,
    config_store_handle: ConfigStoreHandle,
) -> Result<FastlyResponse, FastlyError> {
    let core_request = into_core_request(req).map_err(map_edge_error)?;
    dispatch_core_request(app, core_request, Some(config_store_handle))
}

/// Dispatch a request with a Fastly Config Store injected into extensions.
///
/// If the named store is not available, suppresses repeated warnings for
/// recently seen store names and dispatches without it.
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
    let core_request = into_core_request(req).map_err(map_edge_error)?;
    dispatch_core_request(app, core_request, config_store_handle)
}

fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    config_store_handle: Option<ConfigStoreHandle>,
) -> Result<FastlyResponse, FastlyError> {
    if let Some(handle) = config_store_handle {
        core_request.extensions_mut().insert(handle);
    }
    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
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
