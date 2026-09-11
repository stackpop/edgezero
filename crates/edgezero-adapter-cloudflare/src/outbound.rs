#![cfg_attr(
    all(feature = "cloudflare", target_arch = "wasm32"),
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "the target-gated implementation module is kept before shared test seams"
    )
)]
#![cfg_attr(
    all(feature = "cloudflare", target_arch = "wasm32"),
    expect(
        clippy::pub_use,
        reason = "the target-gated implementation keeps Workers imports out of native builds"
    )
)]

use edgezero_core::error::EdgeError;
#[cfg(any(
    all(feature = "cloudflare", target_arch = "wasm32"),
    feature = "test-utils"
))]
use edgezero_core::error::{BadGatewayReason, BudgetSource};
#[cfg(any(all(feature = "cloudflare", target_arch = "wasm32"), test))]
use edgezero_core::http::{HeaderMap, HeaderName};
use edgezero_core::outbound::{OutboundRequest, validate_for_dispatch};
#[cfg(any(all(feature = "cloudflare", target_arch = "wasm32"), test))]
use std::str;

#[cfg(any(all(feature = "cloudflare", target_arch = "wasm32"), test))]
pub(crate) trait HeaderSink {
    fn append(&mut self, name: &str, value: &str) -> Result<(), EdgeError>;
}

#[cfg(any(all(feature = "cloudflare", target_arch = "wasm32"), test))]
pub(crate) fn copy_header_values<Sink, InvalidValue>(
    source: &HeaderMap,
    sink: &mut Sink,
    invalid_value: InvalidValue,
) -> Result<(), EdgeError>
where
    Sink: HeaderSink,
    InvalidValue: Fn(&HeaderName) -> EdgeError,
{
    for (name, value) in source {
        let header_value =
            str::from_utf8(value.as_bytes()).map_err(|_encoding_error| invalid_value(name))?;
        sink.append(name.as_str(), header_value)?;
    }
    Ok(())
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl HeaderSink for worker::Headers {
    fn append(&mut self, name: &str, value: &str) -> Result<(), EdgeError> {
        worker::Headers::append(self, name, value).map_err(EdgeError::internal)
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod worker_impl {
    use std::cell::RefCell;
    use std::num::NonZeroU64;
    use std::rc::Rc;
    use std::time::Duration;

    use async_stream::stream;
    use async_trait::async_trait;
    use bytes::Bytes;
    use edgezero_core::body::{Body, BodyStream};
    use edgezero_core::compression::{
        ContentEncoding, classify_content_encoding, decode_brotli_stream, decode_gzip_stream,
    };
    use edgezero_core::error::{BadGatewayReason, EdgeError};
    use edgezero_core::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH};
    use edgezero_core::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
    use edgezero_core::outbound::{
        OutboundHttpClient, OutboundRequest, OutboundRequestParts, OutboundResponse,
        OutboundSlotResult, PROXY_HEADER, ResponseBodyDisposition, ResponseHeaderLimiter,
        ResponseMode, collect_response_stream, enforce_payload_content_length,
        limit_decoded_stream, limit_encoded_stream, normalize_for_dispatch,
        normalize_response_headers, rechunk_stream, validate_for_dispatch,
    };
    use edgezero_core::time::{DispatchBudget, MonotonicClock, MonotonicInstant, dispatch_budget};
    use futures_util::StreamExt as _;
    use futures_util::future::{Either, join_all, select};
    use worker::js_sys::{Reflect, Uint8Array, global};
    use worker::wasm_bindgen::{JsCast as _, JsValue};
    use worker::wasm_bindgen_futures::JsFuture;
    use worker::web_sys;
    use worker::{
        Body as WorkerBody, Delay, Headers, Method as WorkerMethod, Request as WorkerRequest,
        RequestInit, RequestRedirect, Response as WorkerResponse,
    };

    use super::timeout_error;

    const READY_ITEM_YIELD_QUOTA: u32 = 1_024;

    /// Native outbound HTTP implementation for Cloudflare Workers.
    pub struct CloudflareOutboundClient {
        clock: MonotonicClock,
    }

    struct AbortGuard {
        controller: Option<web_sys::AbortController>,
    }

    struct PreparedRequest {
        budget: DispatchBudget,
        parts: OutboundRequestParts,
    }

    enum PreparedSlot {
        Finished(OutboundSlotResult),
        Pending(Box<PreparedRequest>),
    }

    impl AbortGuard {
        fn disarm(&mut self) {
            self.controller = None;
        }

        fn new(controller: web_sys::AbortController) -> Self {
            Self {
                controller: Some(controller),
            }
        }
    }

    impl CloudflareOutboundClient {
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
            let upload_error = Rc::new(RefCell::new(None));
            let worker_body = request_body(
                body,
                max_request_body_bytes,
                budget,
                self.clock.clone(),
                Rc::clone(&upload_error),
            )?;
            let url = uri.to_string();
            let request = build_worker_request(&method, &url, &headers, worker_body)?;
            let (response, abort_guard) =
                raw_fetch(request, budget, &self.clock, Rc::clone(&upload_error)).await?;

            process_response(
                response,
                abort_guard,
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
            super::validate_batch_request(&request)?;
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

        /// Builds a client using the process-default monotonic clock.
        #[must_use]
        #[inline]
        pub fn new() -> Self {
            Self::with_clock(MonotonicClock::default())
        }

        /// Builds a client that evaluates every outbound lifetime against `clock`.
        #[must_use]
        #[inline]
        pub fn with_clock(clock: MonotonicClock) -> Self {
            Self { clock }
        }
    }

    impl Default for CloudflareOutboundClient {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for AbortGuard {
        fn drop(&mut self) {
            if let Some(controller) = self.controller.take() {
                controller.abort();
            }
        }
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for CloudflareOutboundClient {
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

    fn build_headers(headers: &HeaderMap) -> Result<Headers, EdgeError> {
        let mut worker_headers = Headers::new();
        super::copy_header_values(headers, &mut worker_headers, |name| {
            EdgeError::bad_request(format!("header value is not valid UTF-8: {name}"))
        })?;
        Ok(worker_headers)
    }

    fn build_worker_request(
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        request_body: Option<JsValue>,
    ) -> Result<WorkerRequest, EdgeError> {
        let mut init = RequestInit::new();
        init.with_headers(build_headers(headers)?)
            .with_method(worker_method(method))
            .with_redirect(RequestRedirect::Manual);
        if let Some(worker_body) = request_body {
            init.with_body(Some(worker_body));
        }
        WorkerRequest::new_with_init(url, &init).map_err(EdgeError::internal)
    }

    fn deadline_stream(
        mut source: BodyStream,
        budget: DispatchBudget,
        clock: MonotonicClock,
        mut abort_guard: AbortGuard,
    ) -> BodyStream {
        stream! {
            let mut ready_items = 0_u32;
            loop {
                let remaining = match budget_remaining(budget, &clock) {
                    Ok(remaining) => remaining,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let next = source.next();
                let timer = Delay::from(remaining);
                futures_util::pin_mut!(next, timer);
                let next_item = match select(next, timer).await {
                    Either::Left((item, _timer)) => item,
                    Either::Right(((), _next)) => {
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
                    None => {
                        abort_guard.disarm();
                        return;
                    }
                }
                ready_items = ready_items.saturating_add(1);
                if ready_items >= READY_ITEM_YIELD_QUOTA {
                    Delay::from(Duration::ZERO).await;
                    ready_items = 0;
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
        mut response: WorkerResponse,
        mut abort_guard: AbortGuard,
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
        let status = StatusCode::from_u16(response.status_code()).map_err(EdgeError::internal)?;
        let mut headers = response_headers(&response)?;
        let mut header_limiter =
            ResponseHeaderLimiter::new(max_response_header_bytes, max_response_header_count);
        header_limiter.observe(&headers)?;
        let disposition = normalize_response_headers(&request_method, status, &mut headers)?;
        headers.insert(PROXY_HEADER, HeaderValue::from_static("cloudflare"));

        if disposition == ResponseBodyDisposition::FramingBodyless {
            return Ok(OutboundResponse::new(
                request_method,
                status,
                headers,
                Body::empty(),
            ));
        }

        let declared_reset_body = matches!(
            disposition,
            ResponseBodyDisposition::ResetContent {
                declared_body: true
            }
        );
        let native = response_stream(&mut response)?;
        if matches!(disposition, ResponseBodyDisposition::ResetContent { .. }) {
            if !declared_reset_body {
                let remaining = budget_remaining(budget, &clock)?;
                let next = native.into_future();
                let timer = Delay::from(remaining);
                futures_util::pin_mut!(next, timer);
                let ready = match select(next, timer).await {
                    Either::Left((ready, _timer)) => ready,
                    Either::Right(((), _next)) => return Err(timeout_error(budget.cause)),
                };
                budget_remaining(budget, &clock)?;
                match ready {
                    (Some(item), _rest) => {
                        item?;
                    }
                    (None, _rest) => abort_guard.disarm(),
                }
            }
            return Ok(OutboundResponse::new(
                request_method,
                status,
                headers,
                Body::empty(),
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
        let deadline_bound = deadline_stream(shaped, budget, clock, abort_guard);
        let body = match response_mode {
            ResponseMode::Buffered { max_bytes } => {
                Body::from(collect_response_stream(deadline_bound, max_bytes).await?)
            }
            ResponseMode::Streamed => Body::from_stream(deadline_bound),
        };
        Ok(OutboundResponse::new(request_method, status, headers, body))
    }

    async fn raw_fetch(
        request: WorkerRequest,
        budget: DispatchBudget,
        clock: &MonotonicClock,
        upload_error: Rc<RefCell<Option<EdgeError>>>,
    ) -> Result<(WorkerResponse, AbortGuard), EdgeError> {
        let remaining = budget_remaining(budget, clock)?;
        let controller = web_sys::AbortController::new().map_err(|_error| {
            EdgeError::internal(anyhow::anyhow!("failed to construct abort controller"))
        })?;
        let signal = controller.signal();
        let abort_guard = AbortGuard::new(controller);
        let fetch_init = web_sys::RequestInit::new();
        fetch_init.set_signal(Some(&signal));
        let set = Reflect::set(
            fetch_init.as_ref(),
            &JsValue::from_str("encodeResponseBody"),
            &JsValue::from_str("manual"),
        )
        .map_err(|_error| {
            EdgeError::internal(anyhow::anyhow!("failed to set manual response encoding"))
        })?;
        if !set {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "runtime refused manual response encoding"
            )));
        }

        let global: web_sys::WorkerGlobalScope = global().unchecked_into();
        let promise = global.fetch_with_request_and_init(request.inner(), &fetch_init);
        let fetch = JsFuture::from(promise);
        let timer = Delay::from(remaining);
        futures_util::pin_mut!(fetch, timer);
        let result = match select(fetch, timer).await {
            Either::Left((result, _timer)) => result,
            Either::Right(((), _fetch)) => return Err(timeout_error(budget.cause)),
        };
        budget_remaining(budget, clock)?;
        let value = match result {
            Ok(value) => value,
            Err(_error) => {
                if let Some(source_error) = upload_error.borrow_mut().take() {
                    return Err(source_error);
                }
                return Err(super::generic_fetch_failure());
            }
        };
        let web_response: web_sys::Response = value.dyn_into().map_err(|_non_response| {
            EdgeError::internal(anyhow::anyhow!("fetch returned a non-response value"))
        })?;
        Ok((WorkerResponse::from(web_response), abort_guard))
    }

    fn request_body(
        body: Body,
        maximum: u64,
        budget: DispatchBudget,
        clock: MonotonicClock,
        upload_error: Rc<RefCell<Option<EdgeError>>>,
    ) -> Result<Option<JsValue>, EdgeError> {
        match body {
            Body::Once(bytes) => {
                let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if length > maximum {
                    return Err(EdgeError::bad_request(
                        "outbound request body exceeded configured limit",
                    ));
                }
                budget_remaining(budget, &clock)?;
                if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Uint8Array::from(bytes.as_ref()).into()))
                }
            }
            Body::Stream(source) => {
                let bounded = upload_stream(source, maximum, budget, clock);
                let mapped = bounded
                    .map(move |result| match result {
                        Ok(bytes) => Ok(bytes.to_vec()),
                        Err(error) => {
                            *upload_error.borrow_mut() = Some(error);
                            Err(JsValue::from_str("outbound upload source failed"))
                        }
                    })
                    .boxed_local();
                let worker_body = WorkerBody::from_stream(mapped).map_err(EdgeError::internal)?;
                Ok(worker_body.into_inner().map(JsValue::from))
            }
        }
    }

    fn response_headers(response: &WorkerResponse) -> Result<HeaderMap, EdgeError> {
        let mut headers = HeaderMap::new();
        for (raw_name, raw_value) in response.headers().entries() {
            let parsed_name = HeaderName::from_bytes(raw_name.as_bytes()).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "upstream response contains an invalid header name",
                    BadGatewayReason::Protocol,
                )
            })?;
            let parsed_value = HeaderValue::from_bytes(raw_value.as_bytes()).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "upstream response contains an invalid header value",
                    BadGatewayReason::Protocol,
                )
            })?;
            headers.append(parsed_name, parsed_value);
        }
        Ok(headers)
    }

    fn response_stream(response: &mut WorkerResponse) -> Result<BodyStream, EdgeError> {
        let source = response.stream().map_err(|_error| {
            EdgeError::bad_gateway_with_reason(
                "upstream response body is unavailable",
                BadGatewayReason::Transport,
            )
        })?;
        Ok(source
            .map(|result| {
                result.map(Bytes::from).map_err(|_error| {
                    EdgeError::bad_gateway_with_reason(
                        "upstream response body failed",
                        BadGatewayReason::Transport,
                    )
                })
            })
            .boxed_local())
    }

    fn budget_remaining(
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> Result<Duration, EdgeError> {
        budget
            .deadline
            .remaining_at(clock.now())
            .map(|remaining| remaining.min(budget.duration))
            .ok_or_else(|| timeout_error(budget.cause))
    }

    fn upload_stream(
        mut source: BodyStream,
        maximum: u64,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> BodyStream {
        stream! {
        let mut ready_items = 0_u32;
        let mut total = 0_u64;
        loop {
            let remaining = match budget_remaining(budget, &clock) {
                Ok(remaining) => remaining,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let next = source.next();
            let timer = Delay::from(remaining);
            futures_util::pin_mut!(next, timer);
            let next_item = match select(next, timer).await {
                Either::Left((item, _timer)) => item,
                Either::Right(((), _next)) => {
                    yield Err(timeout_error(budget.cause));
                    return;
                }
            };
            if budget_remaining(budget, &clock).is_err() {
                yield Err(timeout_error(budget.cause));
                return;
            }
            let Some(item) = next_item else {
                return;
            };
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let Some(next_total) = total.checked_add(length) else {
                yield Err(EdgeError::bad_request("outbound request body size accounting overflow"));
                return;
            };
            if next_total > maximum {
                yield Err(EdgeError::bad_request("outbound request body exceeded configured limit"));
                return;
            }
            total = next_total;
            yield Ok(bytes);
            ready_items = ready_items.saturating_add(1);
            if ready_items >= READY_ITEM_YIELD_QUOTA {
                Delay::from(Duration::ZERO).await;
                ready_items = 0;
            }
        }
    }
    .boxed_local()
    }

    fn worker_method(method: &Method) -> WorkerMethod {
        match *method {
            Method::DELETE => WorkerMethod::Delete,
            Method::HEAD => WorkerMethod::Head,
            Method::OPTIONS => WorkerMethod::Options,
            Method::PATCH => WorkerMethod::Patch,
            Method::POST => WorkerMethod::Post,
            Method::PUT => WorkerMethod::Put,
            _ => WorkerMethod::Get,
        }
    }

    #[cfg(test)]
    mod clock_tests {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use edgezero_core::error::BudgetSource;
        use edgezero_core::time::Deadline;
        use futures_util::stream;
        use wasm_bindgen_test::wasm_bindgen_test;

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

        #[wasm_bindgen_test]
        async fn method_entry_and_preflight_elapsed_use_the_injected_clock() {
            let start = MonotonicInstant::now();
            let completed = start
                .checked_add(Duration::from_millis(9))
                .expect("completed instant");
            let client =
                CloudflareOutboundClient::with_clock(scripted_clock(vec![start, completed]));
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

        #[wasm_bindgen_test]
        async fn backwards_clock_fails_slot_without_invalid_elapsed() {
            let start = MonotonicInstant::now();
            let earlier = start
                .checked_sub(Duration::from_millis(1))
                .expect("earlier instant");
            let client = CloudflareOutboundClient::with_clock(scripted_clock(vec![start, earlier]));
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

        #[wasm_bindgen_test]
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

        #[wasm_bindgen_test]
        fn buffered_request_preparation_reduces_the_remaining_injected_budget() {
            let start = MonotonicInstant::now();
            let budget = test_budget(start, Duration::from_millis(10));
            let observed = start
                .checked_add(Duration::from_millis(3))
                .expect("observed instant");
            let clock = scripted_clock(vec![observed, observed]);
            let upload_error = Rc::new(RefCell::new(None));

            let body = request_body(Body::from("body"), 16, budget, clock.clone(), upload_error)
                .expect("prepared body");
            let remaining = budget_remaining(budget, &clock).expect("remaining budget");

            assert!(body.is_some());
            assert_eq!(remaining, Duration::from_millis(7));
        }

        #[wasm_bindgen_test]
        async fn streamed_upload_checks_injected_clock_after_ready_item() {
            let start = MonotonicInstant::now();
            let budget = test_budget(start, Duration::from_millis(10));
            let clock = scripted_clock(vec![start, budget.deadline.instant()]);
            let source = stream::once(async { Ok(Bytes::from_static(b"body")) }).boxed_local();
            let mut body = upload_stream(source, 16, budget, clock);

            let error = body
                .next()
                .await
                .expect("terminal item")
                .expect_err("post-ready expiry");

            assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
        }

        #[wasm_bindgen_test]
        async fn response_stream_retains_clock_for_post_ready_expiry() {
            let start = MonotonicInstant::now();
            let budget = test_budget(start, Duration::from_millis(10));
            let clock = scripted_clock(vec![start, budget.deadline.instant()]);
            let source = stream::once(async { Ok(Bytes::from_static(b"body")) }).boxed_local();
            let controller = web_sys::AbortController::new().expect("abort controller");
            let mut body = deadline_stream(source, budget, clock, AbortGuard::new(controller));

            let error = body
                .next()
                .await
                .expect("terminal item")
                .expect_err("post-ready expiry");

            assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
        }
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use worker_impl::CloudflareOutboundClient;

#[cfg(any(
    all(feature = "cloudflare", target_arch = "wasm32"),
    feature = "test-utils"
))]
fn generic_fetch_failure() -> EdgeError {
    EdgeError::bad_gateway_with_reason("outbound fetch failed", BadGatewayReason::Unspecified)
}

#[cfg(any(
    all(feature = "cloudflare", target_arch = "wasm32"),
    feature = "test-utils"
))]
fn timeout_error(cause: BudgetSource) -> EdgeError {
    EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
}

fn validate_batch_request(request: &OutboundRequest) -> Result<(), EdgeError> {
    validate_for_dispatch(request)?;
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
    Ok(())
}

/// Runs the target-neutral Cloudflare batch preflight contract in native tests.
///
/// # Errors
/// Returns the same portable validation or batch-shape error as production dispatch.
#[cfg(feature = "test-utils")]
#[inline]
pub fn validate_batch_request_for_test(request: &OutboundRequest) -> Result<(), EdgeError> {
    validate_batch_request(request)
}

/// Returns the target-neutral classification for an opaque Workers fetch rejection.
#[cfg(feature = "test-utils")]
#[must_use]
#[inline]
pub fn generic_fetch_failure_for_test() -> EdgeError {
    generic_fetch_failure()
}

/// Returns the same attributed timeout emitted by Workers budget timers.
#[cfg(feature = "test-utils")]
#[must_use]
#[inline]
pub fn timeout_error_for_test(cause: BudgetSource) -> EdgeError {
    timeout_error(cause)
}

#[cfg(test)]
mod header_bridge_tests {
    use std::collections::BTreeMap;

    use edgezero_core::http::HeaderValue;

    use super::*;

    #[derive(Default)]
    struct AppendSink(BTreeMap<String, Vec<String>>);

    impl HeaderSink for AppendSink {
        fn append(&mut self, name: &str, value: &str) -> Result<(), EdgeError> {
            self.0
                .entry(name.to_owned())
                .or_default()
                .push(value.to_owned());
            Ok(())
        }
    }

    #[test]
    fn response_header_copy_preserves_duplicate_set_cookie_values() {
        let mut source = HeaderMap::new();
        source.append("set-cookie", HeaderValue::from_static("first=1"));
        source.append("set-cookie", HeaderValue::from_static("second=2"));
        let mut sink = AppendSink::default();

        copy_header_values(&source, &mut sink, |_name| {
            EdgeError::internal(anyhow::anyhow!("invalid response header"))
        })
        .expect("copy response headers");

        assert_eq!(
            sink.0.get("set-cookie"),
            Some(&vec!["first=1".to_owned(), "second=2".to_owned()])
        );
    }

    #[test]
    fn response_header_copy_accepts_non_ascii_utf8() {
        let mut source = HeaderMap::new();
        source.append(
            "x-label",
            HeaderValue::from_bytes("caf\u{e9}".as_bytes()).expect("valid header bytes"),
        );
        let mut sink = AppendSink::default();

        copy_header_values(&source, &mut sink, |_name| {
            EdgeError::internal(anyhow::anyhow!("invalid response header"))
        })
        .expect("copy UTF-8 response header");

        assert_eq!(sink.0.get("x-label"), Some(&vec!["caf\u{e9}".to_owned()]));
    }
}
