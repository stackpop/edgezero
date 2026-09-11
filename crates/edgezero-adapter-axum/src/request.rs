use std::net::SocketAddr;
use std::pin::Pin;

use axum::body::{Body as AxumBody, BodyDataStream};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, request::Parts};
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::Request as CoreRequest;
use edgezero_core::outbound::HttpClient;
use edgezero_core::time::{Deadline, MonotonicClock};
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
    read_lifetime: Option<(Deadline, MonotonicClock)>,
) -> Result<CoreRequest, String> {
    let outbound_clock = read_lifetime
        .as_ref()
        .map_or_else(MonotonicClock::default, |(_, clock)| clock.clone());
    let body = match read_lifetime {
        Some((deadline, monotonic_clock)) => {
            deadline_body(axum_body.into_data_stream(), deadline, monotonic_clock)
        }
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

    let outbound_client = AxumOutboundClient::try_with_clock(outbound_clock)
        .map_err(|err| format!("failed to build outbound HTTP client: {err}"))?;
    core_request
        .extensions_mut()
        .insert(HttpClient::with_client(outbound_client));

    Ok(core_request)
}

fn deadline_body(
    body_stream: BodyDataStream,
    deadline: Deadline,
    monotonic_clock: MonotonicClock,
) -> Body {
    let deadline_stream = stream::unfold(
        Some(Box::pin(body_stream)),
        move |stream_state: Option<Pin<Box<BodyDataStream>>>| {
            let clock = monotonic_clock.clone();
            async move {
                let mut state_stream = stream_state?;
                let Some(remaining) = deadline.remaining_at(clock.now()) else {
                    return Some((
                        Err(EdgeError::request_timeout(
                            "inbound body read deadline exceeded",
                        )),
                        None,
                    ));
                };
                let item = match timeout(remaining, state_stream.next()).await {
                    Ok(item) => item,
                    Err(_elapsed) => {
                        return Some((
                            Err(EdgeError::request_timeout(
                                "inbound body read deadline exceeded",
                            )),
                            None,
                        ));
                    }
                };
                if deadline.is_expired_at(clock.now()) {
                    return Some((
                        Err(EdgeError::request_timeout(
                            "inbound body read deadline exceeded",
                        )),
                        None,
                    ));
                }
                match item {
                    Some(Ok(bytes)) => Some((Ok(bytes), Some(state_stream))),
                    Some(Err(error)) => Some((Err(EdgeError::internal(error)), None)),
                    None => None,
                }
            }
        },
    );
    Body::from_stream(deadline_stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use edgezero_core::body::Body;
    use edgezero_core::http::{Method, StatusCode};
    use edgezero_core::time::MonotonicInstant;
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    struct DropSignal(Arc<AtomicUsize>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn deadline_body_releases_source_when_timeout_is_emitted() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let signal = DropSignal(Arc::clone(&dropped));
        let source = stream::poll_fn(move |_cx| {
            let _keep_alive = &signal;
            Poll::<Option<Result<Bytes, io::Error>>>::Pending
        });
        let start = MonotonicInstant::now();
        let clock = MonotonicClock::new(move || start);
        let body = deadline_body(
            AxumBody::from_stream(source).into_data_stream(),
            Deadline::at_instant(start),
            clock,
        );
        let mut body_stream = body.into_stream().expect("stream");

        let error = body_stream
            .next()
            .await
            .expect("terminal timeout item")
            .expect_err("timeout");
        assert_eq!(error.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn admitted_request_installs_outbound_client_with_the_ingress_clock() {
        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(7))
            .expect("completed instant");
        let observations = Arc::new(Mutex::new(vec![completed, start]));
        let clock_observations = Arc::clone(&observations);
        let clock = MonotonicClock::new(move || {
            clock_observations
                .lock()
                .expect("clock observations")
                .pop()
                .expect("clock observation")
        });
        let request = Request::builder()
            .method(Method::GET)
            .uri("/demo")
            .body(AxumBody::empty())
            .expect("request");
        let (parts, body) = request.into_parts();
        let core_request = into_core_request_parts(
            parts,
            body,
            Some((
                Deadline::at_instant(
                    start
                        .checked_add(Duration::from_secs(1))
                        .expect("read deadline"),
                ),
                clock,
            )),
        )
        .expect("request conversion");
        let client = core_request
            .extensions()
            .get::<HttpClient>()
            .expect("outbound client")
            .clone();
        let invalid_batch_request = edgezero_core::OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();

        let results = client.send_all(vec![invalid_batch_request]).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].elapsed, Duration::from_millis(7));
        assert!(matches!(
            results[0].outcome,
            Err(EdgeError::BadRequest { .. })
        ));
        assert!(observations.lock().expect("clock observations").is_empty());
    }

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
