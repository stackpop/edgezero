use async_stream::stream;
use async_trait::async_trait;
use bytes::Bytes;
use core::num::NonZeroU64;
use core::time::Duration;
use edgezero_core::body::{Body, BodyStream};
use edgezero_core::compression::{
    ContentEncoding, classify_content_encoding, decode_brotli_stream, decode_gzip_stream,
};
use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError};
use edgezero_core::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH};
use edgezero_core::http::{HeaderMap, HeaderValue, Method, StatusCode};
use edgezero_core::outbound::{
    OutboundHttpClient, OutboundRequest, OutboundRequestParts, OutboundResponse,
    OutboundSlotResult, PROXY_HEADER, ResponseBodyDisposition, ResponseHeaderLimiter, ResponseMode,
    collect_response_stream, enforce_payload_content_length, limit_decoded_stream,
    limit_encoded_stream, normalize_for_dispatch, normalize_response_headers, rechunk_stream,
    validate_for_dispatch,
};
use edgezero_core::time::{DispatchBudget, MonotonicClock, MonotonicInstant, dispatch_budget};
use futures_util::StreamExt as _;
use futures_util::future::join_all;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use reqwest::redirect::Policy;
use tokio::time::timeout;

/// Native outbound HTTP implementation used by the Axum adapter.
pub struct AxumOutboundClient {
    client: reqwest::Client,
    clock: MonotonicClock,
}

struct PreparedRequest {
    budget: DispatchBudget,
    parts: OutboundRequestParts,
}

enum PreparedSlot {
    Finished(OutboundSlotResult),
    Pending(Box<PreparedRequest>),
}

impl AxumOutboundClient {
    async fn execute(&self, prepared: PreparedRequest) -> Result<OutboundResponse, EdgeError> {
        let PreparedRequest { budget, parts } = prepared;
        let OutboundRequestParts {
            body,
            mut headers,
            max_brotli_decoder_bytes,
            max_brotli_window_bits,
            max_chunk_bytes,
            max_decoded_response_bytes,
            max_encoded_response_bytes,
            max_request_body_bytes,
            max_response_header_bytes,
            max_response_header_count,
            method,
            response_mode,
            uri,
            ..
        } = parts;

        if !headers.contains_key(ACCEPT_ENCODING) {
            headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
        let request_body =
            collect_request_body(body, max_request_body_bytes, budget, &self.clock).await?;
        let remaining = budget_remaining(budget, &self.clock)?;
        let request = self
            .client
            .request(reqwest_method(&method)?, uri.to_string())
            .headers(headers)
            .body(request_body)
            .timeout(remaining);
        let response = request
            .send()
            .await
            .map_err(|error| classify_send_error(&error, budget, &self.clock))?;

        process_response(
            response,
            method,
            response_mode,
            budget,
            max_brotli_decoder_bytes,
            max_brotli_window_bits,
            max_chunk_bytes,
            max_decoded_response_bytes,
            max_encoded_response_bytes,
            max_response_header_bytes,
            max_response_header_count,
            self.clock.clone(),
        )
        .await
    }

    fn prepare(
        request: OutboundRequest,
        started_at: MonotonicInstant,
    ) -> Result<PreparedRequest, EdgeError> {
        validate_for_dispatch(&request)?;
        Self::prepare_validated(request, started_at)
    }

    fn prepare_batch(
        request: OutboundRequest,
        started_at: MonotonicInstant,
    ) -> Result<PreparedRequest, EdgeError> {
        validate_for_dispatch(&request)?;
        if request.is_stream_body() {
            return Err(EdgeError::bad_request(
                "send_all requires buffered request bodies; use send for a streamed upload",
            ));
        }
        if request.is_stream_response() {
            return Err(EdgeError::bad_request(
                "send_all requires buffered responses; use send for a streamed response",
            ));
        }
        Self::prepare_validated(request, started_at)
    }

    fn prepare_validated(
        mut request: OutboundRequest,
        started_at: MonotonicInstant,
    ) -> Result<PreparedRequest, EdgeError> {
        let budget = dispatch_budget(&request, started_at)?;
        normalize_for_dispatch(&mut request)?;
        Ok(PreparedRequest {
            budget,
            parts: request.into_parts(),
        })
    }

    /// Builds a client with redirects and automatic content decoding disabled.
    ///
    /// # Errors
    /// Returns the underlying client-construction error when TLS initialization fails.
    #[inline]
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Self::try_with_clock(MonotonicClock::default())
    }

    /// Builds a client that evaluates every outbound lifetime against `clock`.
    ///
    /// # Errors
    /// Returns the underlying client-construction error when TLS initialization fails.
    #[inline]
    pub fn try_with_clock(clock: MonotonicClock) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .no_brotli()
            .no_deflate()
            .no_gzip()
            .no_zstd()
            .build()?;
        Ok(Self { client, clock })
    }
}

#[async_trait(?Send)]
impl OutboundHttpClient for AxumOutboundClient {
    #[inline]
    async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
        let started_at = self.clock.now();
        let prepared = Self::prepare(request, started_at)?;
        self.execute(prepared).await
    }

    #[inline]
    async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
        let batch_started_at = self.clock.now();
        let preflight: Vec<PreparedSlot> = requests
            .into_iter()
            .map(|request| {
                Self::prepare_batch(request, batch_started_at).map_or_else(
                    |error| {
                        PreparedSlot::Finished(finish_slot(
                            batch_started_at,
                            Err(error),
                            &self.clock,
                        ))
                    },
                    |prepared| PreparedSlot::Pending(Box::new(prepared)),
                )
            })
            .collect();

        join_all(preflight.into_iter().map(|slot| async move {
            match slot {
                PreparedSlot::Pending(prepared) => {
                    let outcome = self.execute(*prepared).await;
                    finish_slot(batch_started_at, outcome, &self.clock)
                }
                PreparedSlot::Finished(done) => done,
            }
        }))
        .await
    }
}

async fn collect_request_body(
    body: Body,
    maximum: u64,
    budget: DispatchBudget,
    clock: &MonotonicClock,
) -> Result<Bytes, EdgeError> {
    match body {
        Body::Once(bytes) => {
            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if length > maximum {
                return Err(EdgeError::bad_request(
                    "outbound request body exceeded configured limit",
                ));
            }
            budget_remaining(budget, clock)?;
            Ok(bytes)
        }
        Body::Stream(mut source) => {
            let mut collected = Vec::new();
            let mut total = 0_u64;
            loop {
                let remaining = budget_remaining(budget, clock)?;
                let next_item = timeout(remaining, source.next())
                    .await
                    .map_err(|_elapsed| timeout_error(budget.cause))?;
                budget_remaining(budget, clock)?;
                let Some(item) = next_item else {
                    return Ok(Bytes::from(collected));
                };
                let bytes = item?;
                let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let Some(next_total) = total.checked_add(length) else {
                    return Err(EdgeError::bad_request(
                        "outbound request body size accounting overflow",
                    ));
                };
                if next_total > maximum {
                    return Err(EdgeError::bad_request(
                        "outbound request body exceeded configured limit",
                    ));
                }
                collected.extend_from_slice(&bytes);
                total = next_total;
            }
        }
    }
}

fn deadline_stream(
    mut source: BodyStream,
    budget: DispatchBudget,
    clock: MonotonicClock,
) -> BodyStream {
    stream! {
        loop {
            let remaining = match budget_remaining(budget, &clock) {
                Ok(remaining) => remaining,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let next_item = match timeout(remaining, source.next()).await {
                Ok(item) => item,
                Err(_elapsed) => {
                    yield Err(timeout_error(budget.cause));
                    return;
                }
            };
            if budget_remaining(budget, &clock).is_err() {
                yield Err(timeout_error(budget.cause));
                return;
            }
            match next_item {
                Some(Ok(bytes)) => yield Ok(bytes),
                Some(Err(error)) => {
                    yield Err(error);
                    return;
                }
                None => return,
            }
        }
    }
    .boxed_local()
}

fn finish_slot(
    started_at: MonotonicInstant,
    outcome: Result<OutboundResponse, EdgeError>,
    clock: &MonotonicClock,
) -> OutboundSlotResult {
    let completed_at = clock.now();
    match completed_at.checked_duration_since(started_at) {
        Some(elapsed) => OutboundSlotResult::new(elapsed, outcome),
        None => OutboundSlotResult::new(
            Duration::ZERO,
            Err(EdgeError::internal(anyhow::anyhow!(
                "monotonic clock moved backwards during outbound dispatch"
            ))),
        ),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the adapter consumes the independent request policy fields without hiding them"
)]
async fn process_response(
    response: reqwest::Response,
    request_method: Method,
    response_mode: ResponseMode,
    budget: DispatchBudget,
    max_brotli_decoder_bytes: u64,
    max_brotli_window_bits: u8,
    max_chunk_bytes: Option<NonZeroU64>,
    max_decoded_response_bytes: Option<u64>,
    max_encoded_response_bytes: Option<u64>,
    max_response_header_bytes: Option<u64>,
    max_response_header_count: Option<u64>,
    clock: MonotonicClock,
) -> Result<OutboundResponse, EdgeError> {
    budget_remaining(budget, &clock)?;
    let response_clock = clock.clone();
    let status = StatusCode::from_u16(response.status().as_u16()).map_err(EdgeError::internal)?;
    let mut headers = copy_headers(response.headers());
    let mut header_limiter =
        ResponseHeaderLimiter::new(max_response_header_bytes, max_response_header_count);
    header_limiter.observe(&headers)?;
    let disposition = normalize_response_headers(&request_method, status, &mut headers)?;
    headers.insert(PROXY_HEADER, HeaderValue::from_static("axum"));

    if disposition == ResponseBodyDisposition::FramingBodyless {
        return Ok(OutboundResponse::new_with_monotonic_clock(
            request_method,
            status,
            headers,
            Body::empty(),
            response_clock,
        ));
    }

    let declared_reset_body = matches!(
        disposition,
        ResponseBodyDisposition::ResetContent {
            declared_body: true
        }
    );
    let native = response_stream(response, budget, clock.clone());
    if matches!(disposition, ResponseBodyDisposition::ResetContent { .. }) {
        if !declared_reset_body {
            let mut reset_stream = deadline_stream(native, budget, clock);
            if let Some(item) = reset_stream.next().await {
                item?;
            }
        }
        return Ok(OutboundResponse::new_with_monotonic_clock(
            request_method,
            status,
            headers,
            Body::empty(),
            response_clock,
        ));
    }

    let encoding = classify_content_encoding(&headers);
    let max_buffered = match response_mode {
        ResponseMode::Buffered { max_bytes } => Some(max_bytes),
        ResponseMode::Streamed => None,
    };
    enforce_payload_content_length(
        &headers,
        encoding,
        max_buffered,
        max_decoded_response_bytes,
        max_encoded_response_bytes,
    )?;
    let encoded = limit_encoded_stream(native, max_encoded_response_bytes);
    let decoded = match encoding {
        ContentEncoding::Brotli => {
            decode_brotli_stream(encoded, max_brotli_window_bits, max_brotli_decoder_bytes)
        }
        ContentEncoding::Gzip => decode_gzip_stream(encoded),
        ContentEncoding::Identity | ContentEncoding::Passthrough => encoded,
    };
    if matches!(encoding, ContentEncoding::Brotli | ContentEncoding::Gzip) {
        headers.remove(CONTENT_ENCODING);
        headers.remove(CONTENT_LENGTH);
    }
    let output = match encoding {
        ContentEncoding::Brotli | ContentEncoding::Gzip | ContentEncoding::Identity => {
            limit_decoded_stream(decoded, max_decoded_response_bytes)
        }
        ContentEncoding::Passthrough => decoded,
    };
    let shaped = rechunk_stream(output, max_chunk_bytes);
    let deadline_bound = deadline_stream(shaped, budget, clock);
    let body = match response_mode {
        ResponseMode::Buffered { max_bytes } => {
            Body::from(collect_response_stream(deadline_bound, max_bytes).await?)
        }
        ResponseMode::Streamed => Body::from_stream(deadline_bound),
    };
    Ok(OutboundResponse::new_with_monotonic_clock(
        request_method,
        status,
        headers,
        body,
        response_clock,
    ))
}

fn budget_remaining(budget: DispatchBudget, clock: &MonotonicClock) -> Result<Duration, EdgeError> {
    budget
        .deadline
        .remaining_at(clock.now())
        .map(|remaining| remaining.min(budget.duration))
        .ok_or_else(|| timeout_error(budget.cause))
}

fn classify_send_error(
    error: &reqwest::Error,
    budget: DispatchBudget,
    clock: &MonotonicClock,
) -> EdgeError {
    if budget.deadline.is_expired_at(clock.now()) {
        return timeout_error(budget.cause);
    }
    if error.is_timeout() {
        return timeout_error(budget.cause);
    }
    let reason = if error.is_connect() {
        BadGatewayReason::Unreachable
    } else if error.is_builder() {
        BadGatewayReason::Protocol
    } else {
        BadGatewayReason::Transport
    };
    EdgeError::bad_gateway_with_reason("upstream request failed", reason)
}

fn copy_headers(headers: &ReqwestHeaderMap) -> HeaderMap {
    let mut copied = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        copied.append(name.clone(), value.clone());
    }
    copied
}

fn reqwest_method(method: &Method) -> Result<reqwest::Method, EdgeError> {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(EdgeError::internal)
}

fn response_stream(
    mut response: reqwest::Response,
    budget: DispatchBudget,
    clock: MonotonicClock,
) -> BodyStream {
    stream! {
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => yield Ok(bytes),
                Ok(None) => return,
                Err(error) => {
                    let mapped = if budget.deadline.is_expired_at(clock.now()) || error.is_timeout() {
                        timeout_error(budget.cause)
                    } else {
                        EdgeError::bad_gateway_with_reason(
                            "upstream response body failed",
                            BadGatewayReason::Transport,
                        )
                    };
                    yield Err(mapped);
                    return;
                }
            }
        }
    }
    .boxed_local()
}

fn timeout_error(cause: BudgetSource) -> EdgeError {
    EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
}

#[cfg(test)]
mod clock_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use edgezero_core::time::Deadline;
    use futures_util::stream;

    use super::*;

    fn scripted_clock(script: Vec<MonotonicInstant>) -> MonotonicClock {
        let observations = Arc::new(Mutex::new(VecDeque::from(script)));
        MonotonicClock::new(move || {
            observations
                .lock()
                .expect("clock observations")
                .pop_front()
                .expect("clock observation")
        })
    }

    fn test_budget(start: MonotonicInstant, duration: Duration) -> DispatchBudget {
        DispatchBudget {
            cause: BudgetSource::PerCallTimeout,
            deadline: Deadline::at_instant(start.checked_add(duration).expect("deadline")),
            duration,
        }
    }

    #[tokio::test]
    async fn method_entry_and_preflight_elapsed_use_the_injected_clock() {
        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(9))
            .expect("completed instant");
        let client = AxumOutboundClient::try_with_clock(scripted_clock(vec![start, completed]))
            .expect("client");
        let request = OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();

        let results = client.send_all(vec![request]).await;

        assert_eq!(results[0].elapsed, Duration::from_millis(9));
        assert!(matches!(
            results[0].outcome,
            Err(EdgeError::BadRequest { .. })
        ));
    }

    #[tokio::test]
    async fn backwards_clock_fails_slot_without_invalid_elapsed() {
        let start = MonotonicInstant::now();
        let earlier = start
            .checked_sub(Duration::from_millis(1))
            .expect("earlier instant");
        let client = AxumOutboundClient::try_with_clock(scripted_clock(vec![start, earlier]))
            .expect("client");
        let request = OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();

        let results = client.send_all(vec![request]).await;

        assert_eq!(results[0].elapsed, Duration::ZERO);
        assert!(matches!(
            results[0].outcome,
            Err(EdgeError::Internal { .. })
        ));
    }

    #[test]
    fn backwards_clock_cannot_expand_the_selected_budget() {
        let start = MonotonicInstant::now();
        let budget = test_budget(start, Duration::from_millis(10));
        let earlier = start
            .checked_sub(Duration::from_millis(5))
            .expect("earlier instant");
        let clock = scripted_clock(vec![earlier]);

        assert_eq!(
            budget_remaining(budget, &clock).expect("remaining budget"),
            budget.duration
        );
    }

    #[tokio::test]
    async fn buffered_request_preparation_reduces_the_remaining_injected_budget() {
        let start = MonotonicInstant::now();
        let budget = test_budget(start, Duration::from_millis(10));
        let observed = start
            .checked_add(Duration::from_millis(3))
            .expect("observed instant");
        let clock = scripted_clock(vec![observed, observed]);

        let body = collect_request_body(Body::from("body"), 16, budget, &clock)
            .await
            .expect("prepared body");
        let remaining = budget_remaining(budget, &clock).expect("remaining budget");

        assert_eq!(body, Bytes::from_static(b"body"));
        assert_eq!(remaining, Duration::from_millis(7));
    }

    #[tokio::test]
    async fn streamed_upload_checks_injected_clock_after_ready_item() {
        let start = MonotonicInstant::now();
        let budget = test_budget(start, Duration::from_millis(10));
        let clock = scripted_clock(vec![start, budget.deadline.instant()]);
        let body = Body::from_stream(stream::once(async { Ok(Bytes::from_static(b"body")) }));

        let error = collect_request_body(body, 16, budget, &clock)
            .await
            .expect_err("post-ready expiry");

        assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
    }

    #[tokio::test]
    async fn response_stream_retains_clock_for_post_ready_expiry() {
        let start = MonotonicInstant::now();
        let budget = test_budget(start, Duration::from_millis(10));
        let clock = scripted_clock(vec![start, budget.deadline.instant()]);
        let source = stream::once(async { Ok(Bytes::from_static(b"body")) }).boxed_local();
        let mut body = deadline_stream(source, budget, clock);

        let error = body
            .next()
            .await
            .expect("terminal item")
            .expect_err("post-ready expiry");

        assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
    }
}
