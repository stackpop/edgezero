#![cfg(feature = "axum")]

use core::convert::Infallible;
use std::io::Write as _;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::{CONTENT_ENCODING, SET_COOKIE};
use axum::response::Response;
use axum::routing::get;
use edgezero_adapter_axum::outbound::AxumOutboundClient;
use edgezero_core::body::Body as CoreBody;
use edgezero_core::error::{BadGatewayReason, EdgeError, ResponseLimitReason};
use edgezero_core::http::{Method, StatusCode};
use edgezero_core::{OutboundHttpClient, OutboundRequest};
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::stream;
use tokio::net::TcpListener;

fn gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

async fn start_origin(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
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
    let client = AxumOutboundClient::try_new().unwrap();
    let request = OutboundRequest::get(format!("{origin}/redirect")).unwrap();

    let response = client.send(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
}

#[tokio::test]
async fn send_all_reports_per_slot_elapsed() {
    let origin = start_origin(Router::new().route("/", get(|| async { "ok" }))).await;
    let client = AxumOutboundClient::try_new().unwrap();
    let requests = vec![
        OutboundRequest::get(format!("{origin}/")).unwrap(),
        OutboundRequest::new(
            Method::GET,
            "http://127.0.0.1:1/unreachable".parse().unwrap(),
        )
        .unwrap(),
    ];

    let results = client.send_all(requests).await;

    assert_eq!(results.len(), 2);
    assert!(results[0].outcome.is_ok());
    assert!(matches!(
        results[1].outcome,
        Err(EdgeError::BadGateway {
            reason: BadGatewayReason::Unreachable,
            ..
        })
    ));
}

#[tokio::test]
async fn send_all_rejects_streamed_shapes_in_preflight() {
    let client = AxumOutboundClient::try_new().unwrap();
    let streamed_upload = OutboundRequest::post("https://example.com/upload")
        .unwrap()
        .body(CoreBody::stream(stream::iter([bytes::Bytes::from_static(
            b"body",
        )])));
    let streamed_response = OutboundRequest::get("https://example.com/stream")
        .unwrap()
        .stream_response();
    let method_error = OutboundRequest::get("https://example.com/get")
        .unwrap()
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
            let body = async_stream::stream! {
                yield Ok::<_, Infallible>(bytes::Bytes::from_static(b"first"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                yield Ok::<_, Infallible>(bytes::Bytes::from_static(b"second"));
            };
            Response::new(Body::from_stream(body))
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().unwrap();
    let request = OutboundRequest::get(format!("{origin}/"))
        .unwrap()
        .timeout(Duration::from_millis(10));

    let error = client
        .send(request)
        .await
        .expect_err("body completion must share the absolute request deadline");

    assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
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
                    .unwrap()
            }
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().unwrap();

    let decoded_error = client
        .send(
            OutboundRequest::get(format!("{origin}/"))
                .unwrap()
                .max_encoded_response_bytes(u64::try_from(encoded.len()).unwrap())
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
                .unwrap()
                .max_encoded_response_bytes(u64::try_from(encoded.len()).unwrap().saturating_sub(1))
                .max_decoded_response_bytes(u64::try_from(decoded.len()).unwrap()),
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
                .unwrap()
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().unwrap();
    let request = OutboundRequest::get(format!("{origin}/"))
        .unwrap()
        .max_encoded_response_bytes(6)
        .max_decoded_response_bytes(1);

    let response = client.send(request).await.unwrap();
    let bytes = response.into_bytes_bounded(6).await.unwrap();

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
    let client = AxumOutboundClient::try_new().unwrap();

    let response = client
        .send(OutboundRequest::get(format!("{origin}/")).unwrap())
        .await
        .unwrap();
    let values: Vec<_> = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();

    assert_eq!(values, ["a=1", "b=2"]);
}
