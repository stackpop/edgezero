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

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;

    #[test]
    fn inserts_and_retrieves_context() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        let context = SpinRequestContext {
            client_addr: Some("127.0.0.1:12345".to_string()),
            full_url: Some("https://example.com/path".to_string()),
        };
        SpinRequestContext::insert(&mut request, context);

        let retrieved = SpinRequestContext::get(&request).expect("context");
        assert_eq!(retrieved.client_addr.as_deref(), Some("127.0.0.1:12345"));
        assert_eq!(
            retrieved.full_url.as_deref(),
            Some("https://example.com/path")
        );
    }

    #[test]
    fn get_returns_none_when_missing() {
        let request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        assert!(SpinRequestContext::get(&request).is_none());
    }
}
