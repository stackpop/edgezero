use std::net::SocketAddr;

use axum::body::{Body as AxumBody, to_bytes};
use axum::extract::connect_info::ConnectInfo;
use axum::http::Request;
use edgezero_core::body::Body;
use edgezero_core::http::Request as CoreRequest;
use edgezero_core::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST};
use edgezero_core::http::{HeaderMap, HeaderValue, Uri};
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::request::{
    CapturedTarget, HeaderFidelity, InboundOrigin, OriginSource, Preservation, RequestIngress,
    TargetSource,
};

use crate::context::AxumRequestContext;
use crate::proxy::AxumProxyClient;

/// Marks the plain HTTP listener owned by this adapter.
#[derive(Clone, Copy)]
pub(crate) struct HttpTransport;

/// Convert an Axum/Hyper request into an `EdgeZero` core request while preserving streaming bodies
/// and exposing connection metadata through `AxumRequestContext`.
///
/// # Errors
/// Returns an error if a buffered (`application/json`) body cannot be read into memory.
#[inline]
pub async fn into_core_request(request: Request<AxumBody>) -> Result<CoreRequest, String> {
    let (mut parts, axum_body) = request.into_parts();
    let transport = parts.extensions.remove::<HttpTransport>();
    let ingress = capture_request_ingress(&parts.uri, &parts.headers, transport);

    let body = match parts.headers.get(CONTENT_TYPE) {
        Some(value) if is_json_content_type(value) => {
            let bytes = to_bytes(axum_body, usize::MAX)
                .await
                .map_err(|err| format!("Failed to convert body into bytes: {err}"))?;
            Body::from_bytes(bytes)
        }
        _ => {
            let stream = axum_body.into_data_stream();
            Body::from_stream(stream)
        }
    };

    let mut core_request = CoreRequest::from_parts(parts, body);
    core_request.extensions_mut().insert(ingress);

    if let Some(remote_addr) = core_request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
    {
        core_request
            .extensions_mut()
            .remove::<ConnectInfo<SocketAddr>>();
        AxumRequestContext::insert(
            &mut core_request,
            AxumRequestContext {
                remote_addr: Some(remote_addr),
            },
        );
    }

    let proxy_client =
        AxumProxyClient::try_new().map_err(|err| format!("failed to build proxy client: {err}"))?;
    core_request
        .extensions_mut()
        .insert(ProxyHandle::with_client(proxy_client));

    Ok(core_request)
}

#[expect(
    clippy::expect_used,
    reason = "the static override list contains exactly one unique header"
)]
fn capture_request_ingress(
    uri: &Uri,
    headers: &HeaderMap,
    transport: Option<HttpTransport>,
) -> RequestIngress {
    let source = if uri.scheme().is_some() {
        TargetSource::RuntimeUrl
    } else {
        TargetSource::RuntimePathAndQuery
    };
    let target = CapturedTarget::capture(&uri.to_string(), source, Preservation::Unknown);
    let common = HeaderFidelity::new(
        Preservation::Unknown,
        Preservation::Unknown,
        Preservation::Unknown,
        Preservation::Unavailable,
    );
    let content_length = HeaderFidelity::new(
        Preservation::Unknown,
        if transport.is_some() {
            Preservation::Transformed
        } else {
            Preservation::Unknown
        },
        Preservation::Unknown,
        Preservation::Unavailable,
    );
    RequestIngress::new(
        target,
        inbound_origin(uri, headers, transport),
        common,
        vec![(CONTENT_LENGTH, content_length)],
    )
    .expect("should use unique header overrides")
}

fn inbound_origin(
    uri: &Uri,
    headers: &HeaderMap,
    transport: Option<HttpTransport>,
) -> Option<InboundOrigin> {
    transport?;
    let mut hosts = headers.get_all(HOST).iter();
    let host = hosts.next()?.to_str().ok()?;
    if hosts.next().is_some() {
        return None;
    }
    let origin = InboundOrigin::parse("http", host, OriginSource::TransportBinding).ok()?;
    if uri
        .scheme_str()
        .is_some_and(|scheme| !scheme.eq_ignore_ascii_case("http"))
    {
        return None;
    }
    if let Some(authority) = uri.authority() {
        let supplied =
            InboundOrigin::parse("http", authority.as_str(), OriginSource::TransportBinding)
                .ok()?;
        let parsed_host = format!("http://{host}/").parse::<Uri>().ok()?;
        let parsed_uri = format!("http://{}/", supplied.authority())
            .parse::<Uri>()
            .ok()?;
        if !parsed_host.host()?.eq_ignore_ascii_case(parsed_uri.host()?)
            || parsed_host.port_u16().unwrap_or(80) != parsed_uri.port_u16().unwrap_or(80)
        {
            return None;
        }
    }
    Some(origin)
}

fn is_json_content_type(value: &HeaderValue) -> bool {
    let Ok(raw) = value.to_str() else {
        return false;
    };

    let media_type = raw.split(';').next().map_or("", str::trim);
    if media_type.eq_ignore_ascii_case("application/json") {
        return true;
    }

    let Some((ty, raw_subtype)) = media_type.split_once('/') else {
        return false;
    };

    if !ty.eq_ignore_ascii_case("application") {
        return false;
    }

    let subtype = raw_subtype.trim();
    let Some(suffix_start) = subtype.len().checked_sub(5) else {
        return false;
    };
    subtype
        .get(suffix_start..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::Method;

    #[tokio::test]
    async fn converts_request_and_records_connect_info() {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/demo")
            .header("x-test", "1")
            .body(AxumBody::from("payload"))
            .expect("request");
        request
            .extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("127.0.0.1:4000".parse().unwrap()));

        let core_request = into_core_request(request)
            .await
            .expect("request conversion");
        assert_eq!(core_request.method(), &Method::POST);
        assert_eq!(core_request.uri().path(), "/demo");
        assert_eq!(core_request.headers()["x-test"], "1");
        match core_request.body() {
            Body::Stream(_) => {} // streaming bodies stay streaming
            Body::Once(_) => panic!("body should remain streaming"),
        }

        let context = AxumRequestContext::get(&core_request).expect("context");
        assert_eq!(context.remote_addr, Some("127.0.0.1:4000".parse().unwrap()));
        assert!(
            core_request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .is_none()
        );
    }

    #[tokio::test]
    async fn missing_connect_info_is_handled_gracefully() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/demo")
            .body(AxumBody::empty())
            .expect("request");

        let core_request = into_core_request(request)
            .await
            .expect("request conversion");
        assert!(AxumRequestContext::get(&core_request).is_none());
    }

    #[tokio::test]
    async fn json_content_type_buffers_body() {
        let json_payload = r#"{"name":"test"}"#;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/test")
            .header("content-type", "application/json")
            .body(AxumBody::from(json_payload))
            .expect("request");

        let core_request = into_core_request(request)
            .await
            .expect("request conversion");
        assert_eq!(core_request.method(), &Method::POST);

        match core_request.body() {
            Body::Once(bytes) => {
                assert_eq!(bytes.as_ref(), json_payload.as_bytes());
            }
            Body::Stream(_) => panic!("JSON body should be buffered, not streaming"),
        }
    }

    #[tokio::test]
    async fn non_json_content_type_streams_body() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/upload")
            .header("content-type", "application/octet-stream")
            .body(AxumBody::from("binary data"))
            .expect("request");

        let core_request = into_core_request(request)
            .await
            .expect("request conversion");

        assert!(matches!(core_request.body(), Body::Stream(_)));
    }

    #[test]
    fn json_content_type_detection() {
        assert!(is_json_content_type(&HeaderValue::from_static(
            "application/json"
        )));
        assert!(is_json_content_type(&HeaderValue::from_static(
            "application/json; charset=utf-8"
        )));
        assert!(is_json_content_type(&HeaderValue::from_static(
            "application/vnd.api+json"
        )));
        assert!(is_json_content_type(&HeaderValue::from_static(
            "APPLICATION/VND.CUSTOM+JSON; CHARSET=UTF-8"
        )));

        assert!(!is_json_content_type(&HeaderValue::from_static(
            "text/json"
        )));
        assert!(!is_json_content_type(&HeaderValue::from_static(
            "application/json+xml"
        )));
    }
}

#[cfg(test)]
mod ingress_tests {
    use super::{HttpTransport, into_core_request};
    use axum::body::Body as AxumBody;
    use edgezero_core::http::{HeaderValue, Method, request_builder};
    use edgezero_core::request::{
        CapturedTarget, MAX_TARGET_BYTES, OriginSource, Preservation, RequestIngress,
        TargetUnavailable,
    };

    #[tokio::test]
    async fn ingress_retains_runtime_parts_without_trusting_headers() {
        let mut request = request_builder()
            .method(Method::from_bytes(b"EXAMPLE-METHOD").expect("should parse method"))
            .uri("/reserved/%2e//%2F?q=a%2Fb")
            .header("host", "example.com")
            .header("forwarded", "proto=https;host=spoof.example.com")
            .body(AxumBody::empty())
            .expect("should build request");
        request.headers_mut().append(
            "cookie",
            HeaderValue::from_bytes(b"a=\xff").expect("should parse bytes"),
        );
        request
            .headers_mut()
            .append("cookie", HeaderValue::from_static("b=2"));
        let core = into_core_request(request)
            .await
            .expect("should convert request");
        let ingress = core
            .extensions()
            .get::<RequestIngress>()
            .expect("should insert metadata");
        assert!(ingress.origin().is_none());
        let CapturedTarget::Complete(target) = ingress.target() else {
            panic!("should capture whole target")
        };
        assert_eq!(target.value(), "/reserved/%2e//%2F?q=a%2Fb");
        assert_eq!(core.method().as_str(), "EXAMPLE-METHOD");
        let values: Vec<_> = core
            .headers()
            .get_all("cookie")
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(values, [b"a=\xff".as_slice(), b"b=2".as_slice()]);
        assert_eq!(
            ingress
                .header_fidelity(&"content-length".parse().expect("should parse name"))
                .field_multiplicity(),
            Preservation::Unknown
        );
    }

    #[tokio::test]
    async fn ingress_bound_does_not_change_uri_body_or_extensions() {
        let path = format!("/{}", "a".repeat(MAX_TARGET_BYTES));
        let mut request = request_builder()
            .uri(&path)
            .header("content-type", "application/json")
            .body(AxumBody::from("{}"))
            .expect("should build request");
        request.extensions_mut().insert(17_u32);
        let converted = into_core_request(request)
            .await
            .expect("should convert bounded metadata");
        assert_eq!(converted.uri().path(), path);
        assert_eq!(converted.body().as_bytes(), Some(b"{}".as_slice()));
        assert_eq!(converted.extensions().get::<u32>(), Some(&17));
        assert!(matches!(
            converted
                .extensions()
                .get::<RequestIngress>()
                .expect("should insert metadata")
                .target(),
            CapturedTarget::Unavailable(TargetUnavailable::TooLarge)
        ));
    }

    #[tokio::test]
    async fn ingress_transport_origin_requires_consistent_single_authority() {
        for (uri, hosts, expected) in [
            ("/reserved", vec!["example.com"], true),
            ("http://EXAMPLE.com:80/reserved", vec!["example.com"], true),
            ("https://example.com/reserved", vec!["example.com"], false),
            (
                "http://other.example.com/reserved",
                vec!["example.com"],
                false,
            ),
            ("/reserved", vec!["example.com", "example.com"], false),
            ("/reserved", vec!["example.com/path"], false),
            ("/reserved", vec![], false),
        ] {
            let mut request = request_builder()
                .uri(uri)
                .body(AxumBody::empty())
                .expect("should build request");
            for host in hosts {
                request.headers_mut().append(
                    "host",
                    HeaderValue::from_str(host).expect("should parse host"),
                );
            }
            request.extensions_mut().insert(HttpTransport);
            let core = into_core_request(request)
                .await
                .expect("should convert request");
            let ingress = core
                .extensions()
                .get::<RequestIngress>()
                .expect("should insert metadata");
            assert_eq!(ingress.origin().is_some(), expected);
            if let Some(origin) = ingress.origin() {
                assert_eq!(origin.scheme(), "http");
                assert_eq!(origin.source(), OriginSource::TransportBinding);
            }
            assert!(core.extensions().get::<HttpTransport>().is_none());
        }
    }
}
