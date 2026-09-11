#![cfg(all(test, feature = "axum"))]
#![expect(
    clippy::expect_used,
    clippy::tests_outside_test_module,
    reason = "this integration-test crate uses explicit fixture diagnostics and consists only of contract tests"
)]

use core::convert::Infallible;
use std::io::Write as _;
use std::time::Duration;

use async_stream::stream as async_body_stream;
use axum::Router;
use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::{CONTENT_ENCODING, SET_COOKIE};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use edgezero_adapter_axum::outbound::AxumOutboundClient;
use edgezero_core::body::Body as CoreBody;
use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError, ResponseLimitReason};
use edgezero_core::http::{Method, StatusCode};
use edgezero_core::time::Deadline;
use edgezero_core::{OutboundHttpClient as _, OutboundRequest, PROXY_HEADER};
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::stream;
use tokio::net::TcpListener;
use tokio::time::sleep;

fn gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).expect("gzip input");
    encoder.finish().expect("gzip output")
}

async fn start_origin(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let address = listener.local_addr().expect("origin address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve origin");
    });
    format!("http://{address}")
}

#[tokio::test]
async fn redirect_response_is_not_followed() {
    let origin = start_origin(
        Router::new()
            .route("/target", get(|| async { "followed" }))
            .route(
                "/redirect",
                get(|| async { (StatusCode::FOUND, [("location", "/target")]) }),
            ),
    )
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/redirect")).expect("request");

    let response = client.send(request).await.expect("response");

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response
            .headers()
            .get(PROXY_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("axum")
    );
}

#[tokio::test]
async fn send_all_reports_per_slot_elapsed() {
    let origin = start_origin(Router::new().route("/", get(|| async { "ok" }))).await;
    let client = AxumOutboundClient::try_new().expect("client");
    let requests = vec![
        OutboundRequest::get(format!("{origin}/")).expect("reachable request"),
        OutboundRequest::new(
            Method::GET,
            "http://127.0.0.1:1/unreachable"
                .parse()
                .expect("unreachable URI"),
        )
        .expect("unreachable request"),
    ];

    let results = client.send_all(requests).await;

    assert_eq!(results.len(), 2);
    results[0]
        .outcome
        .as_ref()
        .expect("reachable slot succeeds");
    assert!(matches!(
        results[1].outcome,
        Err(EdgeError::BadGateway {
            reason: BadGatewayReason::Unreachable,
            ..
        })
    ));
}

#[tokio::test]
async fn send_all_preflight_precedence_and_indices() {
    let client = AxumOutboundClient::try_new().expect("client");
    let streamed_upload = OutboundRequest::post("https://example.com/upload")
        .expect("upload request")
        .body(CoreBody::stream(stream::iter([bytes::Bytes::from_static(
            b"body",
        )])));
    let streamed_response = OutboundRequest::get("https://example.com/stream")
        .expect("stream request")
        .stream_response();
    let method_error = OutboundRequest::get("https://example.com/get")
        .expect("GET request")
        .body(CoreBody::stream(stream::iter([bytes::Bytes::new()])));

    let results = client
        .send_all(vec![streamed_upload, streamed_response, method_error])
        .await;

    let messages: Vec<_> = results
        .iter()
        .map(|slot| match &slot.outcome {
            Err(EdgeError::BadRequest { message }) => message.as_str(),
            other => panic!("expected preflight rejection, got {other:?}"),
        })
        .collect();
    assert_eq!(
        messages,
        [
            "send_all requires buffered request bodies; use send for a streamed upload",
            "send_all requires buffered responses; use send for a streamed response",
            "GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream",
        ]
    );
}

#[tokio::test]
async fn buffered_response_deadline_covers_body_completion() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            let body = async_body_stream! {
                yield Ok::<_, Infallible>(Bytes::from_static(b"first"));
                sleep(Duration::from_millis(100)).await;
                yield Ok::<_, Infallible>(Bytes::from_static(b"second"));
            };
            Response::new(Body::from_stream(body))
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/"))
        .expect("request")
        .timeout(Duration::from_millis(10));

    let error = client
        .send(request)
        .await
        .expect_err("body completion must share the absolute request deadline");

    assert!(matches!(
        error,
        EdgeError::GatewayTimeout {
            cause: BudgetSource::PerCallTimeout,
            ..
        }
    ));
}

#[tokio::test]
async fn send_all_preserves_absolute_deadline_provenance() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            sleep(Duration::from_millis(100)).await;
            "late"
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/"))
        .expect("request")
        .deadline(Deadline::after(Duration::from_millis(10)));

    let results = client.send_all(vec![request]).await;

    assert!(matches!(
        results[0].outcome,
        Err(EdgeError::GatewayTimeout {
            cause: BudgetSource::BatchDeadline,
            ..
        })
    ));
}

#[tokio::test]
async fn streamed_response_deadline_preserves_timeout_provenance() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            let body = async_body_stream! {
                sleep(Duration::from_millis(100)).await;
                yield Ok::<_, Infallible>(Bytes::from_static(b"late"));
            };
            Response::new(Body::from_stream(body))
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/"))
        .expect("request")
        .stream_response()
        .timeout(Duration::from_millis(10));

    let response = client.send(request).await.expect("response headers");
    let error = response
        .into_body()
        .into_bytes_bounded(1_024)
        .await
        .expect_err("stream body must retain the request deadline");

    assert!(matches!(
        error,
        EdgeError::GatewayTimeout {
            cause: BudgetSource::PerCallTimeout,
            ..
        }
    ));
}

#[tokio::test]
async fn encoded_and_decoded_limits_report_independent_origins() {
    let decoded = vec![b'x'; 4_096];
    let encoded = gzip(&decoded);
    let origin_body = encoded.clone();
    let origin = start_origin(Router::new().route(
        "/",
        get(move || {
            let bytes = origin_body.clone();
            async move {
                Response::builder()
                    .header(CONTENT_ENCODING, "gzip")
                    .body(Body::from(bytes))
                    .expect("origin response")
            }
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");

    let decoded_error = client
        .send(
            OutboundRequest::get(format!("{origin}/"))
                .expect("decoded-limit request")
                .max_encoded_response_bytes(u64::try_from(encoded.len()).expect("encoded length"))
                .max_decoded_response_bytes(128),
        )
        .await
        .expect_err("decoded response must exceed its independent limit");
    assert!(matches!(
        decoded_error,
        EdgeError::ResponseTooLarge {
            reason: ResponseLimitReason::DecodedBody,
            ..
        }
    ));

    let encoded_error = client
        .send(
            OutboundRequest::get(format!("{origin}/"))
                .expect("encoded-limit request")
                .max_encoded_response_bytes(
                    u64::try_from(encoded.len())
                        .expect("encoded length")
                        .saturating_sub(1),
                )
                .max_decoded_response_bytes(u64::try_from(decoded.len()).expect("decoded length")),
        )
        .await
        .expect_err("encoded response must exceed its independent limit");
    assert!(matches!(
        encoded_error,
        EdgeError::ResponseTooLarge {
            reason: ResponseLimitReason::EncodedBody,
            ..
        }
    ));
}

#[tokio::test]
async fn passthrough_coding_is_not_charged_to_decoded_limit() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            Response::builder()
                .header(CONTENT_ENCODING, "zstd")
                .body(Body::from("opaque"))
                .expect("origin response")
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/"))
        .expect("request")
        .max_encoded_response_bytes(6)
        .max_decoded_response_bytes(1);

    let response = client.send(request).await.expect("response");
    let bytes = response.into_bytes_bounded(6).await.expect("response body");

    assert_eq!(bytes, "opaque");
}

#[tokio::test]
async fn repeated_response_headers_are_preserved() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            let mut response = Response::new(Body::empty());
            response
                .headers_mut()
                .append(SET_COOKIE, HeaderValue::from_static("a=1"));
            response
                .headers_mut()
                .append(SET_COOKIE, HeaderValue::from_static("b=2"));
            response
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");

    let response = client
        .send(OutboundRequest::get(format!("{origin}/")).expect("request"))
        .await
        .expect("response");
    let values: Vec<_> = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| value.to_str().expect("header value"))
        .collect();

    assert_eq!(values, ["a=1", "b=2"]);
}
