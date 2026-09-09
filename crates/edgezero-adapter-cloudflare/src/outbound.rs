use edgezero_core::error::EdgeError;
use edgezero_core::outbound::{OutboundRequest, validate_for_dispatch};

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod worker_impl {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use async_stream::stream;
    use async_trait::async_trait;
    use bytes::Bytes;
    use edgezero_core::body::{Body, BodyStream};
    use edgezero_core::compression::{
        ContentEncoding, classify_content_encoding, decode_brotli_stream, decode_gzip_stream,
    };
    use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError};
    use edgezero_core::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH};
    use edgezero_core::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
    use edgezero_core::outbound::{
        OutboundHttpClient, OutboundRequest, OutboundRequestParts, OutboundResponse,
        OutboundSlotResult, ResponseBodyDisposition, ResponseHeaderLimiter, ResponseMode,
        collect_response_stream, enforce_payload_content_length, limit_decoded_stream,
        limit_encoded_stream, normalize_for_dispatch, normalize_response_headers, rechunk_stream,
        validate_for_dispatch,
    };
    use edgezero_core::time::{DispatchBudget, MonotonicInstant, dispatch_budget};
    use futures_util::StreamExt as _;
    use futures_util::future::{Either, join_all, select};
    use worker::js_sys::{Reflect, Uint8Array};
    use worker::wasm_bindgen::{JsCast as _, JsValue};
    use worker::wasm_bindgen_futures::JsFuture;
    use worker::web_sys;
    use worker::{
        Body as WorkerBody, Delay, Headers, Method as WorkerMethod, Request as WorkerRequest,
        RequestInit, RequestRedirect, Response as WorkerResponse,
    };

    const READY_ITEM_YIELD_QUOTA: u32 = 1_024;

    /// Native outbound HTTP implementation for Cloudflare Workers.
    pub struct CloudflareOutboundClient;

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
                Rc::clone(&upload_error),
            )?;
            let request = build_worker_request(&method, uri.to_string(), &headers, worker_body)?;
            let (response, abort_guard) =
                raw_fetch(request, budget, Rc::clone(&upload_error)).await?;

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
            let started_at = MonotonicInstant::now();
            let prepared = Self::prepare(request, started_at)?;
            self.execute(prepared).await
        }

        #[inline]
        async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
            let batch_started_at = MonotonicInstant::now();
            let preflight: Vec<PreparedSlot> = requests
                .into_iter()
                .map(|request| {
                    Self::prepare_batch(request, batch_started_at).map_or_else(
                        |error| PreparedSlot::Finished(finish_slot(batch_started_at, Err(error))),
                        |prepared| PreparedSlot::Pending(Box::new(prepared)),
                    )
                })
                .collect();

            join_all(preflight.into_iter().map(|slot| async move {
                match slot {
                    PreparedSlot::Pending(prepared) => {
                        let outcome = self.execute(*prepared).await;
                        finish_slot(batch_started_at, outcome)
                    }
                    PreparedSlot::Finished(done) => done,
                }
            }))
            .await
        }
    }

    fn build_headers(headers: &HeaderMap) -> Result<Headers, EdgeError> {
        let worker_headers = Headers::new();
        for (name, value) in headers {
            let value = value.to_str().map_err(|_error| {
                EdgeError::bad_request(format!("header value is not valid UTF-8: {name}"))
            })?;
            worker_headers
                .append(name.as_str(), value)
                .map_err(EdgeError::internal)?;
        }
        Ok(worker_headers)
    }

    fn build_worker_request(
        method: &Method,
        url: String,
        headers: &HeaderMap,
        body: Option<JsValue>,
    ) -> Result<WorkerRequest, EdgeError> {
        let mut init = RequestInit::new();
        init.with_headers(build_headers(headers)?)
            .with_method(worker_method(method))
            .with_redirect(RequestRedirect::Manual);
        if let Some(body) = body {
            init.with_body(Some(body));
        }
        WorkerRequest::new_with_init(&url, &init).map_err(EdgeError::internal)
    }

    fn deadline_stream(
        mut source: BodyStream,
        budget: DispatchBudget,
        mut abort_guard: AbortGuard,
    ) -> BodyStream {
        stream! {
            let mut ready_items = 0_u32;
            loop {
                let remaining = match budget_remaining(budget) {
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
                if budget_remaining(budget).is_err() {
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
    ) -> OutboundSlotResult {
        let completed_at = MonotonicInstant::now();
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
        max_chunk_bytes: Option<std::num::NonZeroU64>,
        max_decoded_response_bytes: Option<u64>,
        max_encoded_response_bytes: Option<u64>,
        max_response_header_bytes: Option<u64>,
        max_response_header_count: Option<u64>,
    ) -> Result<OutboundResponse, EdgeError> {
        budget_remaining(budget)?;
        let status = StatusCode::from_u16(response.status_code()).map_err(EdgeError::internal)?;
        let mut headers = response_headers(&response)?;
        let mut header_limiter =
            ResponseHeaderLimiter::new(max_response_header_bytes, max_response_header_count);
        header_limiter.observe(&headers)?;
        let disposition = normalize_response_headers(&request_method, status, &mut headers)?;

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
                let remaining = budget_remaining(budget)?;
                let next = native.into_future();
                let timer = Delay::from(remaining);
                futures_util::pin_mut!(next, timer);
                match select(next, timer).await {
                    Either::Left(((Some(item), _rest), _timer)) => {
                        item?;
                    }
                    Either::Left(((None, _rest), _timer)) => abort_guard.disarm(),
                    Either::Right(((), _next)) => return Err(timeout_error(budget.cause)),
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
        let deadline_bound = deadline_stream(shaped, budget, abort_guard);
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
        upload_error: Rc<RefCell<Option<EdgeError>>>,
    ) -> Result<(WorkerResponse, AbortGuard), EdgeError> {
        let remaining = budget_remaining(budget)?;
        let controller = web_sys::AbortController::new().map_err(|error| {
            EdgeError::internal(anyhow::anyhow!(
                "failed to construct abort controller: {error:?}"
            ))
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
        .map_err(|error| {
            EdgeError::internal(anyhow::anyhow!(
                "failed to set manual response encoding: {error:?}"
            ))
        })?;
        if !set {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "runtime refused manual response encoding"
            )));
        }

        let global: web_sys::WorkerGlobalScope = worker::js_sys::global().unchecked_into();
        let promise = global.fetch_with_request_and_init(request.inner(), &fetch_init);
        let fetch = JsFuture::from(promise);
        let timer = Delay::from(remaining);
        futures_util::pin_mut!(fetch, timer);
        let result = match select(fetch, timer).await {
            Either::Left((result, _timer)) => result,
            Either::Right(((), _fetch)) => return Err(timeout_error(budget.cause)),
        };
        budget_remaining(budget)?;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if let Some(source_error) = upload_error.borrow_mut().take() {
                    return Err(source_error);
                }
                return Err(EdgeError::bad_gateway_with_reason(
                    format!("outbound fetch failed: {error:?}"),
                    BadGatewayReason::Unspecified,
                ));
            }
        };
        let web_response: web_sys::Response = value.dyn_into().map_err(|value| {
            EdgeError::internal(anyhow::anyhow!(
                "fetch returned a non-response value: {value:?}"
            ))
        })?;
        Ok((WorkerResponse::from(web_response), abort_guard))
    }

    fn request_body(
        body: Body,
        maximum: u64,
        budget: DispatchBudget,
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
                budget_remaining(budget)?;
                if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Uint8Array::from(bytes.as_ref()).into()))
                }
            }
            Body::Stream(source) => {
                let bounded = upload_stream(source, maximum, budget);
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
        for (name, value) in response.headers().entries() {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "upstream response contains an invalid header name",
                    BadGatewayReason::Protocol,
                )
            })?;
            let value = HeaderValue::from_bytes(value.as_bytes()).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "upstream response contains an invalid header value",
                    BadGatewayReason::Protocol,
                )
            })?;
            headers.append(name, value);
        }
        Ok(headers)
    }

    fn response_stream(response: &mut WorkerResponse) -> Result<BodyStream, EdgeError> {
        let source = response.stream().map_err(|error| {
            EdgeError::bad_gateway_with_reason(
                format!("upstream response body is unavailable: {error}"),
                BadGatewayReason::Transport,
            )
        })?;
        Ok(source
            .map(|result| {
                result.map(Bytes::from).map_err(|error| {
                    EdgeError::bad_gateway_with_reason(
                        format!("upstream response body failed: {error}"),
                        BadGatewayReason::Transport,
                    )
                })
            })
            .boxed_local())
    }

    fn timeout_error(cause: BudgetSource) -> EdgeError {
        EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
    }

    fn budget_remaining(budget: DispatchBudget) -> Result<Duration, EdgeError> {
        budget
            .deadline
            .remaining()
            .ok_or_else(|| timeout_error(budget.cause))
    }

    fn upload_stream(mut source: BodyStream, maximum: u64, budget: DispatchBudget) -> BodyStream {
        stream! {
        let mut ready_items = 0_u32;
        let mut total = 0_u64;
        loop {
            let remaining = match budget_remaining(budget) {
                Ok(remaining) => remaining,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let next = source.next();
            let timer = Delay::from(remaining);
            futures_util::pin_mut!(next, timer);
            let item = match select(next, timer).await {
                Either::Left((item, _timer)) => item,
                Either::Right(((), _next)) => {
                    yield Err(timeout_error(budget.cause));
                    return;
                }
            };
            if budget_remaining(budget).is_err() {
                yield Err(timeout_error(budget.cause));
                return;
            }
            let Some(item) = item else {
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
            Method::GET => WorkerMethod::Get,
            Method::HEAD => WorkerMethod::Head,
            Method::OPTIONS => WorkerMethod::Options,
            Method::PATCH => WorkerMethod::Patch,
            Method::POST => WorkerMethod::Post,
            Method::PUT => WorkerMethod::Put,
            _ => WorkerMethod::Get,
        }
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use worker_impl::CloudflareOutboundClient;

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
