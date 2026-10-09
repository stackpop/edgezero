use std::net::IpAddr;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use std::net::SocketAddr;

use edgezero_core::http::Request;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::http::Uri;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::request::{
    CapturedTarget, HeaderFidelity, InboundOrigin, OriginSource, Preservation, RequestIngress,
    TargetSource,
};
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

impl SpinRequestContext {
    /// Retrieve a previously-inserted context from request extensions.
    #[inline]
    pub fn get(request: &Request) -> Option<&SpinRequestContext> {
        request.extensions().get::<SpinRequestContext>()
    }

    /// Store this context in the request's extensions.
    #[inline]
    pub fn insert(request: &mut Request, context: SpinRequestContext) {
        request.extensions_mut().insert(context);
    }
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
#[expect(
    clippy::expect_used,
    reason = "the empty override list cannot contain duplicate names"
)]
pub(crate) fn capture_request_ingress(uri: &Uri) -> RequestIngress {
    let origin = uri
        .scheme_str()
        .zip(uri.authority())
        .and_then(|(scheme, authority)| {
            InboundOrigin::parse(scheme, authority.as_str(), OriginSource::RuntimeUri).ok()
        });
    let source = if uri.scheme().is_some() {
        TargetSource::RuntimeUrl
    } else {
        TargetSource::RuntimePathAndQuery
    };
    let target = CapturedTarget::capture(&uri.to_string(), source, Preservation::Unknown);
    let headers = HeaderFidelity::new(
        Preservation::Unknown,
        Preservation::Unknown,
        Preservation::Unknown,
        Preservation::Unavailable,
    );
    RequestIngress::new(target, origin, headers, Vec::new())
        .expect("should use unique header overrides")
}

/// Parse an IP address from a `host:port` string.
///
/// Falls back to parsing the raw value as a bare IP (no port) and also
/// handles IPv6 bracket notation (`[::1]:port`).
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub(crate) fn parse_client_addr(raw: &str) -> Option<IpAddr> {
    // Try `ip:port` (IPv4) or `[ip]:port` (IPv6 bracket notation).
    if let Ok(sock) = raw.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    // Bare IP with no port.
    raw.parse::<IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::request_builder;
    use std::str::FromStr as _;

    #[test]
    fn get_returns_none_when_missing() {
        let request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        assert!(SpinRequestContext::get(&request).is_none());
    }

    #[test]
    fn inserts_and_retrieves_context() {
        let mut request = request_builder()
            .uri("https://example.com")
            .body(Body::empty())
            .expect("request");

        let context = SpinRequestContext {
            client_addr: Some(IpAddr::from_str("127.0.0.1").unwrap()),
            full_url: Some("https://example.com/path".to_owned()),
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
    fn parse_client_addr_invalid() {
        assert!(parse_client_addr("not-an-ip").is_none());
    }

    #[test]
    fn parse_client_addr_ipv4_bare() {
        let ip = parse_client_addr("10.0.0.1").unwrap();
        assert_eq!(ip, IpAddr::from_str("10.0.0.1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv4_with_port() {
        let ip = parse_client_addr("192.168.1.1:8080").unwrap();
        assert_eq!(ip, IpAddr::from_str("192.168.1.1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv6_bare() {
        let ip = parse_client_addr("::1").unwrap();
        assert_eq!(ip, IpAddr::from_str("::1").unwrap());
    }

    #[test]
    fn parse_client_addr_ipv6_bracket() {
        let ip = parse_client_addr("[::1]:3000").unwrap();
        assert_eq!(ip, IpAddr::from_str("::1").unwrap());
    }
}
#[cfg(test)]
mod ingress_tests {
    use super::capture_request_ingress;
    use edgezero_core::http::Uri;
    use edgezero_core::request::{CapturedTarget, OriginSource, Preservation};

    #[test]
    fn ingress_captures_runtime_uri_and_origin_only() {
        let uri: Uri = "https://example.com/reserved/%2e//%2F?q=a%2Fb"
            .parse()
            .expect("should parse URI");
        let ingress = capture_request_ingress(&uri);
        let CapturedTarget::Complete(target) = ingress.target() else {
            panic!("should capture target")
        };
        assert_eq!(target.value(), uri.to_string());
        let origin = ingress.origin().expect("should capture runtime origin");
        assert_eq!(origin.scheme(), "https");
        assert_eq!(origin.authority(), "example.com");
        assert_eq!(origin.source(), OriginSource::RuntimeUri);
        assert_eq!(
            ingress
                .header_fidelity(&"cookie".parse().expect("should parse name"))
                .global_field_order(),
            Preservation::Unavailable
        );
        assert!(
            capture_request_ingress(&"/reserved".parse().expect("should parse path"))
                .origin()
                .is_none()
        );
        assert!(
            capture_request_ingress(
                &"ftp://example.com/reserved"
                    .parse()
                    .expect("should parse URI")
            )
            .origin()
            .is_none()
        );
    }
}
