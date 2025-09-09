use anyedge_core::proxy::{BackendTarget, Proxy, ProxyError};
use anyedge_core::{Request as ARequest, Response as AResponse};

/// Register a proxy handler placeholder for Cloudflare Workers.
///
/// Note: Workers `fetch` is async; AnyEdge core is currently sync. This stub
/// returns an error until an async client facade is available.
pub fn register_proxy() {
    let handler = Box::new(
        |_req: ARequest, _target: BackendTarget| -> Result<AResponse, ProxyError> {
            Err(ProxyError::new(
                "Cloudflare proxy not implemented (requires async fetch)",
            ))
        },
    );
    let _ = Proxy::set(handler);
}
