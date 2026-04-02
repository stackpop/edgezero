use std::net::IpAddr;

use edgezero_core::http::Request;

/// Platform-specific request context for Spin.
///
/// Spin exposes client information via special headers
/// (`spin-client-addr`, `spin-full-url`, etc.) rather than
/// a separate runtime context object.
#[derive(Debug, Clone)]
pub struct SpinRequestContext {
    /// The client IP address, parsed from the `spin-client-addr` header.
    /// The header value has the format `ip:port`; only the IP is retained.
    pub client_addr: Option<IpAddr>,
    /// The full URL of the incoming request.
    pub full_url: Option<String>,
}

/// Parse an IP address from a `host:port` string.
///
/// Falls back to parsing the raw value as a bare IP (no port) and also
/// handles IPv6 bracket notation (`[::1]:port`).
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub(crate) fn parse_client_addr(raw: &str) -> Option<IpAddr> {
    // Try `ip:port` (IPv4) or `[ip]:port` (IPv6 bracket notation).
    if let Ok(sock) = raw.parse::<std::net::SocketAddr>() {
        return Some(sock.ip());
    }
    // Bare IP with no port.
    raw.parse::<IpAddr>().ok()
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
    use std::str::FromStr;

    #[test]
    fn inserts_and_retrieves_context() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        let context = SpinRequestContext {
            client_addr: Some(IpAddr::from_str("127.0.0.1").unwrap()),
            full_url: Some("https://example.com/path".to_string()),
        };
        SpinRequestContext::insert(&mut request, context);

        let retrieved = SpinRequestContext::get(&request).expect("context");
        assert_eq!(
            retrieved.client_addr,
            Some(IpAddr::from_str("127.0.0.1").unwrap())
        );
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

    #[test]
    fn parse_client_addr_ipv4_with_port() {
        let ip = parse_client_addr("192.168.1.1:8080").unwrap();
        assert_eq!(ip, IpAddr::from_str("192.168.1.1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv4_bare() {
        let ip = parse_client_addr("10.0.0.1").unwrap();
        assert_eq!(ip, IpAddr::from_str("10.0.0.1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv6_bracket() {
        let ip = parse_client_addr("[::1]:3000").unwrap();
        assert_eq!(ip, IpAddr::from_str("::1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv6_bare() {
        let ip = parse_client_addr("::1").unwrap();
        assert_eq!(ip, IpAddr::from_str("::1").unwrap());
    }

    #[test]
    fn parse_client_addr_invalid() {
        assert!(parse_client_addr("not-an-ip").is_none());
    }
}
