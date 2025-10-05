use std::net::SocketAddr;

use anyedge_core::body::Body;
use anyedge_core::http::Request as CoreRequest;
use anyedge_core::proxy::ProxyHandle;
use axum::body::Body as AxumBody;
use axum::extract::connect_info::ConnectInfo;
use axum::http::Request;

use crate::context::AxumRequestContext;
use crate::proxy::AxumProxyClient;

/// Convert an Axum/Hyper request into an AnyEdge core request while preserving streaming bodies
/// and exposing connection metadata through `AxumRequestContext`.
pub fn into_core_request(request: Request<AxumBody>) -> CoreRequest {
    let (parts, body) = request.into_parts();
    let stream = body.into_data_stream();
    let body = Body::from_stream(stream);
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

    core_request
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::body::Body;
    use anyedge_core::http::Method;

    #[test]
    fn converts_request_and_records_connect_info() {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/demo")
            .header("x-test", "1")
            .body(AxumBody::from("payload"))
            .expect("request");
        request
            .extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("127.0.0.1:4000".parse().unwrap()));

        let core_request = into_core_request(request);
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

    #[test]
    fn missing_connect_info_is_handled_gracefully() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/demo")
            .body(AxumBody::empty())
            .expect("request");

        let core_request = into_core_request(request);
        assert!(AxumRequestContext::get(&core_request).is_none());
    }
}
