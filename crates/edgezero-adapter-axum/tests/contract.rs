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
use axum::http::header::{CACHE_CONTROL, CONTENT_ENCODING, HOST, SET_COOKIE};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use edgezero_adapter_axum::outbound::AxumOutboundClient;
use edgezero_core::body::Body as CoreBody;
use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError, ResponseLimitReason};
use edgezero_core::http::{HeaderMap, Method, StatusCode};
use edgezero_core::time::Deadline;
use edgezero_core::{OutboundCachePolicy, OutboundHttpClient as _, OutboundRequest, PROXY_HEADER};
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
async fn batch_collection_reports_per_slot_elapsed() {
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

    let results = client
        .start_batch_until(requests, Deadline::after(Duration::from_secs(1)))
        .collect()
        .await;

    assert_eq!(results.slots.len(), 2);
    results.slots[0]
        .as_ref()
        .expect("reachable slot resolved")
        .outcome
        .as_ref()
        .expect("reachable slot succeeds");
    assert!(matches!(
        results.slots[1]
            .as_ref()
            .expect("unreachable slot resolved")
            .outcome,
        Err(EdgeError::BadGateway {
            reason: BadGatewayReason::Unreachable,
            ..
        })
    ));
}

#[tokio::test]
async fn batch_preflight_precedence_and_indices() {
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
        .start_batch_until(
            vec![streamed_upload, streamed_response, method_error],
            Deadline::after(Duration::from_secs(1)),
        )
        .collect()
        .await;

    let messages: Vec<_> = results
        .slots
        .iter()
        .map(
            |slot| match &slot.as_ref().expect("preflight slot resolved").outcome {
                Err(EdgeError::BadRequest { message }) => message.as_str(),
                other => panic!("expected preflight rejection, got {other:?}"),
            },
        )
        .collect();
    assert_eq!(
        messages,
        [
            "outbound batches require buffered request bodies; use send for a streamed upload",
            "outbound batches require buffered responses; use send for a streamed response",
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
async fn batch_preserves_request_deadline_provenance() {
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

    let results = client
        .start_batch_until(vec![request], Deadline::after(Duration::from_secs(1)))
        .collect()
        .await;

    assert!(matches!(
        results.slots[0]
            .as_ref()
            .expect("request deadline slot resolved")
            .outcome,
        Err(EdgeError::GatewayTimeout {
            cause: BudgetSource::RequestDeadline,
            ..
        })
    ));
}

#[tokio::test]
async fn batch_yields_complete_exchanges_in_completion_order() {
    let origin = start_origin(
        Router::new()
            .route(
                "/slow",
                get(|| async {
                    sleep(Duration::from_millis(75)).await;
                    "slow"
                }),
            )
            .route("/fast", get(|| async { "fast" })),
    )
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let requests = vec![
        OutboundRequest::get(format!("{origin}/slow")).expect("slow request"),
        OutboundRequest::get(format!("{origin}/fast")).expect("fast request"),
    ];
    let mut batch = client.start_batch_until(requests, Deadline::after(Duration::from_secs(1)));

    let first = batch.next().await.expect("first completion");
    let second = batch.next().await.expect("second completion");

    assert_eq!(first.index, 1);
    assert_eq!(second.index, 0);
    assert!(batch.next().await.is_none());
}

#[tokio::test]
async fn batch_cutoff_preserves_completed_slots_and_leaves_pending_none() {
    let origin = start_origin(
        Router::new()
            .route("/fast", get(|| async { "fast" }))
            .route(
                "/slow",
                get(|| async {
                    sleep(Duration::from_millis(100)).await;
                    "slow"
                }),
            ),
    )
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let results = client
        .start_batch_until(
            vec![
                OutboundRequest::get(format!("{origin}/fast")).expect("fast request"),
                OutboundRequest::get(format!("{origin}/slow")).expect("slow request"),
            ],
            Deadline::after(Duration::from_millis(25)),
        )
        .collect()
        .await;

    assert!(results.slots[0].is_some());
    assert!(results.slots[1].is_none());
}

#[tokio::test]
async fn authority_override_changes_host_only_and_cache_bypass_adds_no_origin_header() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|headers: HeaderMap| async move {
            let host = headers
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("missing");
            let cache_control = headers
                .get(CACHE_CONTROL)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("absent");
            format!("{host}|{cache_control}")
        }),
    ))
    .await;
    let client = AxumOutboundClient::try_new().expect("client");
    let request = OutboundRequest::get(format!("{origin}/"))
        .expect("request")
        .host_authority_override("virtual.example:8443")
        .expect("authority override")
        .cache_policy(OutboundCachePolicy::Bypass);

    let body = client
        .send(request)
        .await
        .expect("response")
        .into_bytes_bounded(1_024)
        .await
        .expect("response body");

    assert_eq!(body, "virtual.example:8443|absent");
}

#[tokio::test]
async fn streamed_response_deadline_preserves_timeout_provenance() {
    let origin = start_origin(Router::new().route(
        "/",
        get(|| async {
            let body = async_body_stream! {
                sleep(Duration::from_secs(2)).await;
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
        .timeout(Duration::from_millis(500));

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
