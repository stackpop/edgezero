use anyedge_core::{
    proxy::{BackendTarget, Proxy, ProxyError},
    Request as ARequest, Response as AResponse,
};

use crate::http::{to_anyedge_response, to_fastly_request};

/// Register a proxy handler that forwards requests to Fastly backends by name,
/// or to absolute URLs (the latter will have scheme/host ignored when using named backends).
#[cfg(feature = "fastly")]
pub fn register_proxy() {
    let handler = Box::new(
        |req: ARequest, target: BackendTarget| -> Result<AResponse, ProxyError> {
            match target {
                BackendTarget::Named(name) => {
                    let f_req = to_fastly_request(req, "http://backend.internal");
                    let f_resp = f_req
                        .send(&name)
                        .map_err(|e| ProxyError::new(e.to_string()))?;
                    Ok(to_anyedge_response(f_resp))
                }
                BackendTarget::Url(url) => {
                    let f_req = to_fastly_request(req, &url);
                    // When URL is absolute, Fastly will use it directly if permitted
                    let f_resp = f_req.send("").map_err(|e| ProxyError::new(e.to_string()))?;
                    Ok(to_anyedge_response(f_resp))
                }
            }
        },
    );
    let _ = Proxy::set(handler);
}

#[cfg(not(feature = "fastly"))]
pub fn register_proxy() {
    // No-op when Fastly feature is disabled
}
