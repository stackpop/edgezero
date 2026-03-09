use std::io::Read;
use std::sync::Arc;

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
    let core_request = into_core_request(req).map_err(map_edge_error)?;
    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
}

/// Dispatch a request with a Fastly Config Store injected into extensions.
///
/// If the named store is not available, logs at info level and dispatches without it.
pub fn dispatch_with_config(
    app: &App,
    req: FastlyRequest,
    store_name: &str,
) -> Result<FastlyResponse, FastlyError> {
    let mut core_request = into_core_request(req).map_err(map_edge_error)?;

    match FastlyConfigStore::try_open(store_name) {
        Some(store) => {
            core_request
                .extensions_mut()
                .insert(ConfigStoreHandle::new(Arc::new(store)));
        }
        None => {
            log::warn!(
                "configured Fastly config store '{}' is unavailable; skipping config-store injection",
                store_name
            );
        }
    }

    let response = executor::block_on(app.router().oneshot(core_request));
    from_core_response(response).map_err(map_edge_error)
}

fn map_edge_error(err: EdgeError) -> FastlyError {
    FastlyError::msg(err.to_string())
}
