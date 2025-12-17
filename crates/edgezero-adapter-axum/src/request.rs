use std::net::SocketAddr;

use edgezero_core::body::Body;
use edgezero_core::http::Request as CoreRequest;
use edgezero_core::proxy::ProxyHandle;
use axum::body::Body as AxumBody;
use axum::extract::connect_info::ConnectInfo;
use axum::http::Request;
use http::header::CONTENT_TYPE;
use http::HeaderValue;

use crate::context::AxumRequestContext;
use crate::proxy::AxumProxyClient;

/// Convert an Axum/Hyper request into an EdgeZero core request while preserving streaming bodies
/// and exposing connection metadata through `AxumRequestContext`.
pub async fn into_core_request(request: Request<AxumBody>) -> Result<CoreRequest, String> {
    let (parts, body) = request.into_parts();

    let body = match parts.headers.get(CONTENT_TYPE) {
        Some(value) if is_json_content_type(value) => {
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .map_err(|e| format!("Failed to convert body into bytes: {e}"))?;
            Body::from_bytes(bytes)
        }
        _ => {
            let stream = body.into_data_stream();
            Body::from_stream(stream)
        }
    };

    let mut core_request = CoreRequest::from_parts(parts, body);

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

    core_request
        .extensions_mut()
        .insert(ProxyHandle::with_client(AxumProxyClient::default()));

    Ok(core_request)
}

fn is_json_content_type(value: &HeaderValue) -> bool {
    let Ok(raw) = value.to_str() else {
        return false;
    };

    let media_type = raw.split(';').next().map(str::trim).unwrap_or("");
    if media_type.eq_ignore_ascii_case("application/json") {
        return true;
    }

    let Some((ty, subtype)) = media_type.split_once('/') else {
        return false;
    };

    if !ty.eq_ignore_ascii_case("application") {
        return false;
    }

    let subtype = subtype.trim();
    subtype.len() >= 5 && subtype[subtype.len() - 5..].eq_ignore_ascii_case("+json")
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
        assert!(core_request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_none());
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
    fn test_is_json_content_type() {
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
