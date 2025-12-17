use std::net::SocketAddr;

use edgezero_core::http::Request;

/// Axum-specific context data attached to each request.
#[derive(Clone, Debug)]
pub struct AxumRequestContext {
    pub remote_addr: Option<SocketAddr>,
}

impl AxumRequestContext {
    pub fn insert(request: &mut Request, context: AxumRequestContext) {
        request.extensions_mut().insert(context);
    }

    pub fn get(request: &Request) -> Option<&AxumRequestContext> {
        request.extensions().get::<AxumRequestContext>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;

    #[test]
    fn inserts_and_reads_context() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        let context = AxumRequestContext {
            remote_addr: Some("127.0.0.1:3000".parse().unwrap()),
        };
        AxumRequestContext::insert(&mut request, context.clone());

        let retrieved = AxumRequestContext::get(&request).expect("context present");
        assert_eq!(retrieved.remote_addr, context.remote_addr);
    }

    #[test]
    fn missing_context_returns_none() {
        let request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        assert!(AxumRequestContext::get(&request).is_none());
    }
}
