use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body as AxumBody, BodyDataStream};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{Request, request::Parts};
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Request as CoreRequest, header::CONTENT_TYPE};
use edgezero_core::outbound::HttpClient;
use edgezero_core::time::{Deadline, MonotonicClock};
use futures_util::{StreamExt as _, stream};
use tokio::time::timeout;

use crate::context::AxumRequestContext;
use crate::outbound::AxumOutboundClient;
use crate::run_options::JsonBodyLimit;

/// Convert an Axum/Hyper request into an `EdgeZero` core request while preserving streaming bodies
/// and exposing connection metadata through `AxumRequestContext`.
/// Attaches a finite 2 MiB JSON buffering policy without reading ambient configuration.
/// Taken streams and manual body collection do not enforce this policy.
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
    into_core_request_parts(parts, axum_body, None, None, JsonBodyLimit::DEFAULT)
}

pub(crate) fn into_core_request_parts(
    parts: Parts,
    axum_body: AxumBody,
    read_lifetime: Option<(Deadline, MonotonicClock)>,
    outbound_transport: Option<Arc<reqwest::Client>>,
    json_body_limit: JsonBodyLimit,
) -> Result<CoreRequest, String> {
    let ingress_is_json = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_media_type);
    let outbound_clock = read_lifetime
        .as_ref()
        .map_or_else(MonotonicClock::default, |(_, clock)| clock.clone());
    let body = match read_lifetime {
        Some((deadline, monotonic_clock)) => {
            deadline_body(axum_body.into_data_stream(), deadline, monotonic_clock)
        }
        None => Body::from_external_stream(axum_body.into_data_stream()),
    };

    let managed_body = body.with_buffered_json_policy(json_body_limit.get(), ingress_is_json);
    let mut core_request = CoreRequest::from_parts(parts, managed_body);

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

    let outbound_client = match outbound_transport {
        Some(transport) => AxumOutboundClient::with_transport_and_clock(transport, outbound_clock),
        None => AxumOutboundClient::try_with_clock(outbound_clock)
            .map_err(|_error| "failed to build outbound HTTP client".to_owned())?,
    };
    core_request
        .extensions_mut()
        .insert(HttpClient::with_client(outbound_client));

    Ok(core_request)
}

fn is_json_media_type(raw: &str) -> bool {
    raw.parse::<mime::Mime>().is_ok_and(|media| {
        media.type_() == mime::APPLICATION
            && (media.subtype() == mime::JSON || media.suffix() == Some(mime::JSON))
    })
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
    use edgezero_core::http::{Method, StatusCode};
    use edgezero_core::time::MonotonicInstant;
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

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
        assert!(
            core_request.body().is_stream(),
            "body should remain streaming"
        );

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
    async fn supplied_outbound_transport_is_retained_by_the_core_request() {
        let transport = AxumOutboundClient::try_transport().expect("transport");
        let request = Request::builder()
            .method(Method::GET)
            .uri("/demo")
            .body(AxumBody::empty())
            .expect("request");
        let (parts, body) = request.into_parts();

        let core_request = into_core_request_parts(
            parts,
            body,
            None,
            Some(Arc::clone(&transport)),
            JsonBodyLimit::DEFAULT,
        )
        .expect("request conversion");

        assert_eq!(Arc::strong_count(&transport), 2);
        drop(core_request);
        assert_eq!(Arc::strong_count(&transport), 1);
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

    #[test]
    fn ingress_json_classifier_validates_media_type_syntax() {
        for raw in [
            "application/json",
            "APPLICATION/JSON; charset=utf-8",
            "application/problem+json",
            "application/vnd.fixture+json; profile=\"a;b\"",
        ] {
            assert!(is_json_media_type(raw), "{raw}");
        }
        for raw in [
            "text/json",
            "application/jsonp",
            "application/json; broken",
            "application/json extra",
            "application/",
            "",
        ] {
            assert!(!is_json_media_type(raw), "{raw}");
        }
    }

    #[tokio::test]
    async fn json_content_type_stays_streaming() {
        let json_payload = serde_json::json!({"name": "test"}).to_string();
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

        assert!(core_request.body().is_stream());
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

        assert!(core_request.body().is_stream());
    }
}
