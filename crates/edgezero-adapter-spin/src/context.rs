use edgezero_core::http::Request;

/// Platform-specific request context for Spin.
///
/// Spin exposes client information via special headers
/// (`spin-client-addr`, `spin-full-url`, etc.) rather than
/// a separate runtime context object.
#[derive(Debug, Clone)]
pub struct SpinRequestContext {
    /// The client IP address, extracted from the `spin-client-addr` header.
    pub client_addr: Option<String>,
    /// The full URL of the incoming request.
    pub full_url: Option<String>,
}

impl SpinRequestContext {
    /// Store this context in the request's extensions.
    pub fn insert(request: &mut Request, context: SpinRequestContext) {
        request.extensions_mut().insert(context);
    }

    /// Retrieve a previously-inserted context from request extensions.
    pub fn get(request: &Request) -> Option<&SpinRequestContext> {
        request.extensions().get::<SpinRequestContext>()
    }
}
