use std::net::IpAddr;

use anyedge_core::Request;

/// Fastly-specific context data stored on each request.
#[derive(Clone, Debug)]
pub struct FastlyRequestContext {
    pub client_ip: Option<IpAddr>,
}

impl FastlyRequestContext {
    pub fn insert(request: &mut Request, context: FastlyRequestContext) {
        request.extensions_mut().insert(context);
    }

    pub fn get(request: &Request) -> Option<&FastlyRequestContext> {
        request.extensions().get::<FastlyRequestContext>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{request_builder, Body};
    use std::net::IpAddr;
    use std::str::FromStr;

    #[test]
    fn inserts_and_retrieves_client_ip() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        let context = FastlyRequestContext {
            client_ip: Some(IpAddr::from_str("127.0.0.1").unwrap()),
        };
        FastlyRequestContext::insert(&mut request, context.clone());

        let retrieved = FastlyRequestContext::get(&request).expect("context");
        assert_eq!(retrieved.client_ip, context.client_ip);
    }

    #[test]
    fn get_returns_none_when_missing() {
        let request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        assert!(FastlyRequestContext::get(&request).is_none());
    }
}
