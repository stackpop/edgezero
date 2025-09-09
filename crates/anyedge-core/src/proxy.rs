use std::sync::OnceLock;

use crate::http::{Request, Response};

#[derive(Debug, Clone)]
pub struct ProxyError {
    pub message: String,
}

impl ProxyError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum BackendTarget {
    /// Provider-named backend (e.g., Fastly backend name)
    Named(String),
    /// Absolute URL to fetch (provider may ignore depending on capabilities)
    Url(String),
}

pub type ProxyHandler =
    Box<dyn Fn(Request, BackendTarget) -> Result<Response, ProxyError> + Send + Sync + 'static>;

static PROXY_HANDLER: OnceLock<ProxyHandler> = OnceLock::new();

pub struct Proxy;

impl Proxy {
    /// Register a process-wide proxy handler. Returns false if one was already registered.
    pub fn set(handler: ProxyHandler) -> bool {
        PROXY_HANDLER.set(handler).is_ok()
    }

    /// Whether a proxy handler has been configured.
    pub fn is_configured() -> bool {
        PROXY_HANDLER.get().is_some()
    }

    /// Send a Request to the specified backend target using the configured handler.
    pub fn send(req: Request, target: BackendTarget) -> Result<Response, ProxyError> {
        if let Some(h) = PROXY_HANDLER.get() {
            (h)(req, target)
        } else {
            Err(ProxyError::new("proxy handler not configured"))
        }
    }
}
