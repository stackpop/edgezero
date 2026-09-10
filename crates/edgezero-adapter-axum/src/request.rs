use std::net::SocketAddr;
use std::pin::Pin;

use axum::body::{Body as AxumBody, BodyDataStream};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, request::Parts};
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Request as CoreRequest;
use edgezero_core::outbound::HttpClient;
use edgezero_core::time::Deadline;
use futures_util::{StreamExt as _, stream};
use tokio::time::timeout;

use crate::context::AxumRequestContext;
use crate::outbound::AxumOutboundClient;

/// Convert an Axum/Hyper request into an `EdgeZero` core request while preserving streaming bodies
/// and exposing connection metadata through `AxumRequestContext`.
///
/// # Errors
/// Returns an error if the outbound client cannot be initialized.
#[inline]
#[expect(
    clippy::unused_async,
    reason = "the public converter retains its established async API while request bodies remain lazy"
)]
pub async fn into_core_request(request: Request<AxumBody>) -> Result<CoreRequest, String> {
    let (parts, axum_body) = request.into_parts();
    into_core_request_parts(parts, axum_body, None)
}

pub(crate) fn into_core_request_parts(
    parts: Parts,
    axum_body: AxumBody,
    read_deadline: Option<Deadline>,
) -> Result<CoreRequest, String> {
    let body = match read_deadline {
        Some(deadline) => deadline_body(axum_body.into_data_stream(), deadline),
        None => Body::from_external_stream(axum_body.into_data_stream()),
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

    let outbound_client = AxumOutboundClient::try_new()
        .map_err(|err| format!("failed to build outbound HTTP client: {err}"))?;
    core_request
        .extensions_mut()
        .insert(HttpClient::with_client(outbound_client));

    Ok(core_request)
}

fn deadline_body(body_stream: BodyDataStream, deadline: Deadline) -> Body {
    let deadline_stream = stream::unfold(
        (Box::pin(body_stream), false),
        move |(mut state_stream, terminal): (Pin<Box<BodyDataStream>>, bool)| async move {
            if terminal {
                return None;
            }
            let Some(remaining) = deadline.remaining() else {
                return Some((
                    Err(EdgeError::request_timeout(
                        "inbound body read deadline exceeded",
                    )),
                    (state_stream, true),
                ));
            };
            let item = match timeout(remaining, state_stream.next()).await {
                Ok(item) => item,
                Err(_elapsed) => {
                    return Some((
                        Err(EdgeError::request_timeout(
                            "inbound body read deadline exceeded",
                        )),
                        (state_stream, true),
                    ));
                }
            };
            if deadline.is_expired() {
                return Some((
                    Err(EdgeError::request_timeout(
                        "inbound body read deadline exceeded",
                    )),
                    (state_stream, true),
                ));
            }
            match item {
                Some(Ok(bytes)) => Some((Ok(bytes), (state_stream, false))),
                Some(Err(error)) => Some((Err(EdgeError::internal(error)), (state_stream, true))),
                None => None,
            }
        },
    );
    Body::from_stream(deadline_stream)
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
        assert!(core_request.extensions().get::<HttpClient>().is_some());
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
    async fn json_content_type_stays_streaming() {
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

        assert!(matches!(core_request.body(), Body::Stream(_)));
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
}
