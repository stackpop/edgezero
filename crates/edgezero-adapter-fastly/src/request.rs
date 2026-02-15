use std::io::Read;

use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::kv::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;

use crate::kv::FastlyKvStore;
use crate::proxy::FastlyProxyClient;
use crate::response::{from_core_response, parse_uri};
use crate::FastlyRequestContext;

/// Default Fastly KV Store name.
///
/// If a KV Store with this name exists in your Fastly service, it will
/// be automatically available to handlers via the `Kv` extractor.
pub const DEFAULT_KV_STORE_NAME: &str = "EDGEZERO_KV";

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

pub fn dispatch(app: &App, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
    dispatch_with_kv(app, req, DEFAULT_KV_STORE_NAME)
}

/// Dispatch a Fastly request with a custom KV store name.
pub fn dispatch_with_kv(
    app: &App,
    req: FastlyRequest,
    kv_store_name: &str,
) -> Result<FastlyResponse, FastlyError> {
    let mut core_request = into_core_request(req).map_err(map_edge_error)?;

    // Inject KV handle if the store exists — graceful fallback.
    if let Ok(store) = FastlyKvStore::open(kv_store_name) {
        let handle = KvHandle::new(std::sync::Arc::new(store));
        core_request.extensions_mut().insert(handle);
    }

    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
}

fn map_edge_error(err: EdgeError) -> FastlyError {
    FastlyError::msg(err.to_string())
}
