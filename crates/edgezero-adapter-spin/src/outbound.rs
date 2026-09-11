#![cfg_attr(
    all(feature = "spin", target_arch = "wasm32"),
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "the target-gated implementation groups request preparation before transport execution"
    )
)]
#![cfg_attr(
    all(feature = "spin", target_arch = "wasm32"),
    expect(
        clippy::pub_use,
        reason = "the target-gated implementation keeps Spin SDK imports out of native builds"
    )
)]

use edgezero_core::error::EdgeError;
use edgezero_core::outbound::{OutboundRequest, validate_for_dispatch};

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use async_stream::stream;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::body::{Body, BodyStream};
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use std::future::{Future, IntoFuture};
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use std::task::Poll;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use std::time::Duration;

#[cfg(feature = "spin")]
use edgezero_core::error::{BadGatewayReason, BudgetSource};
#[cfg(feature = "spin")]
use edgezero_core::time::Deadline;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use futures_util::{StreamExt as _, future::poll_fn};
#[cfg(feature = "spin")]
use spin_sdk::wasip3::http::types::ErrorCode;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
const READY_ITEM_YIELD_QUOTA: usize = 64;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadCompletion {
    Complete,
    ReaderGone,
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestTimeouts {
    between_bytes: u64,
    connect: u64,
    first_byte: u64,
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
fn request_timeouts(remaining: Duration) -> RequestTimeouts {
    let full_remaining = duration_nanos(remaining);
    RequestTimeouts {
        between_bytes: full_remaining,
        connect: full_remaining,
        first_byte: full_remaining,
    }
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
fn request_body_limit(body: &Body, configured: u64) -> u64 {
    match body {
        Body::Once(_) => u64::MAX,
        Body::Stream(_) => configured,
    }
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
async fn cooperative_yield_once() {
    let mut yielded = false;
    poll_fn(move |context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
fn cooperative_stream(mut source: BodyStream) -> BodyStream {
    stream! {
        let mut ready_items = 0_usize;
        while let Some(item) = source.next().await {
            yield item;
            ready_items = ready_items.saturating_add(1);
            if ready_items >= READY_ITEM_YIELD_QUOTA {
                cooperative_yield_once().await;
                ready_items = 0;
            }
        }
    }
    .boxed_local()
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
async fn run_exchange<Send, Pump, RequestDone, Output, HostError, MapError>(
    send_future: Send,
    pump_future: Pump,
    request_done_reader: RequestDone,
    map_error: MapError,
) -> Result<Output, EdgeError>
where
    Send: Future<Output = Result<Output, HostError>>,
    Pump: Future<Output = Result<UploadCompletion, EdgeError>>,
    RequestDone: IntoFuture<Output = Result<(), HostError>>,
    MapError: Fn(&HostError) -> EdgeError,
{
    #[derive(Clone, Copy)]
    enum State {
        AwaitingRequestDone,
        ReaderGone,
        RequestComplete,
        Uploading,
    }

    enum Outcome<Output, HostError> {
        PumpError(EdgeError),
        RequestDoneError(HostError),
        Send(Result<Output, HostError>),
    }

    let mut state = State::Uploading;
    let mut retained_send = None;
    let mut send = Box::pin(send_future);
    let mut pump = Box::pin(pump_future);
    let mut request_done = Box::pin(request_done_reader.into_future());

    let outcome = poll_fn(|context| {
        loop {
            match state {
                State::Uploading => match pump.as_mut().poll(context) {
                    Poll::Ready(Err(error)) => return Poll::Ready(Outcome::PumpError(error)),
                    Poll::Ready(Ok(UploadCompletion::Complete)) => {
                        state = State::AwaitingRequestDone;
                    }
                    Poll::Ready(Ok(UploadCompletion::ReaderGone)) => {
                        state = State::ReaderGone;
                    }
                    Poll::Pending => match send.as_mut().poll(context) {
                        Poll::Ready(result) => return Poll::Ready(Outcome::Send(result)),
                        Poll::Pending => return Poll::Pending,
                    },
                },
                State::AwaitingRequestDone => match request_done.as_mut().poll(context) {
                    Poll::Ready(Err(error)) => {
                        return Poll::Ready(Outcome::RequestDoneError(error));
                    }
                    Poll::Ready(Ok(())) => state = State::RequestComplete,
                    Poll::Pending => {
                        if retained_send.is_none()
                            && let Poll::Ready(result) = send.as_mut().poll(context)
                        {
                            retained_send = Some(result);
                        }
                        return Poll::Pending;
                    }
                },
                State::ReaderGone | State::RequestComplete => {
                    if let Some(result) = retained_send.take() {
                        return Poll::Ready(Outcome::Send(result));
                    }
                    match send.as_mut().poll(context) {
                        Poll::Ready(result) => return Poll::Ready(Outcome::Send(result)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    })
    .await;

    drop(request_done);
    drop(pump);
    drop(send);
    match outcome {
        Outcome::PumpError(error) => Err(error),
        Outcome::RequestDoneError(error) | Outcome::Send(Err(error)) => Err(map_error(&error)),
        Outcome::Send(Ok(output)) => Ok(output),
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod spin_impl {
    use std::num::NonZeroU64;
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
    use edgezero_core::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
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
    use futures_util::stream::once;
    use spin_sdk::time::sleep;
    use spin_sdk::wasip3::http::client;
    use spin_sdk::wasip3::http::types::{
        ErrorCode, Fields, Method as WasiMethod, Request, RequestOptions, RequestOptionsError,
        Response, Scheme,
    };
    use spin_sdk::wasip3::http_compat::BodyWriter;
    use spin_sdk::wasip3::wit_bindgen::{FutureWriter, StreamResult};
    use spin_sdk::wasip3::wit_future;

    use super::{
        UploadCompletion, cooperative_yield_once, map_spin_send_error, run_exchange, timeout_error,
    };

    const RESPONSE_READ_BYTES: usize = 16 * 1024;

    /// Native outbound HTTP implementation for Spin's WASI HTTP 0.3 host.
    pub struct SpinOutboundClient {
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

    impl SpinOutboundClient {
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

            let fields = request_fields(&headers)?;
            let options = request_options(budget, &self.clock)?;
            let (writer, contents, trailers) = BodyWriter::new();
            let (request, request_done) =
                Request::new(fields, Some(contents), trailers, Some(options));
            set_request_target(&request, &method, &uri)?;
            let request_body_limit = super::request_body_limit(&body, max_request_body_bytes);

            let exchange = async move {
                let upload =
                    pump_request_body(body, request_body_limit, writer, budget, self.clock.clone());
                let send = client::send(request);
                run_exchange(send, upload, request_done, |error| {
                    map_spin_send_error(error, budget.deadline, budget.cause, self.clock.now())
                })
                .await
            };
            let response = race_deadline(exchange, budget, &self.clock).await??;

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
    }

    impl Default for SpinOutboundClient {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for SpinOutboundClient {
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

    fn request_fields(headers: &HeaderMap) -> Result<Fields, EdgeError> {
        let fields = Fields::new();
        for (name, value) in headers {
            fields
                .append(name.as_str(), value.as_bytes())
                .map_err(|_error| {
                    EdgeError::bad_request("Spin rejected an outbound request header")
                })?;
        }
        Ok(fields)
    }

    fn request_options(
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> Result<RequestOptions, EdgeError> {
        let options = RequestOptions::new();
        let remaining = budget_remaining(budget, clock)?;
        let timeouts = super::request_timeouts(remaining);
        set_timeout_option(
            "connect",
            &options.set_connect_timeout(Some(timeouts.connect)),
        )?;
        set_timeout_option(
            "first-byte",
            &options.set_first_byte_timeout(Some(timeouts.first_byte)),
        )?;
        set_timeout_option(
            "between-bytes",
            &options.set_between_bytes_timeout(Some(timeouts.between_bytes)),
        )?;
        Ok(options)
    }

    fn set_timeout_option(
        name: &str,
        outcome: &Result<(), RequestOptionsError>,
    ) -> Result<(), EdgeError> {
        match outcome {
            Ok(()) => Ok(()),
            Err(RequestOptionsError::NotSupported) => {
                log::debug!("Spin host does not support the outbound {name} timeout option");
                Ok(())
            }
            Err(RequestOptionsError::Immutable) => Err(EdgeError::internal(anyhow::anyhow!(
                "Spin outbound {name} timeout option is immutable"
            ))),
            Err(RequestOptionsError::Other(_)) => Err(EdgeError::internal(anyhow::anyhow!(
                "Spin host rejected the outbound {name} timeout option"
            ))),
        }
    }

    fn set_request_target(request: &Request, method: &Method, uri: &Uri) -> Result<(), EdgeError> {
        request
            .set_method(&wasi_method(method))
            .map_err(|()| EdgeError::bad_request("Spin rejected the outbound request method"))?;
        let scheme = match uri.scheme_str() {
            Some("http") => Scheme::Http,
            Some("https") => Scheme::Https,
            Some(other) => Scheme::Other(other.to_owned()),
            None => return Err(EdgeError::bad_request("outbound request URI has no scheme")),
        };
        request
            .set_scheme(Some(&scheme))
            .map_err(|()| EdgeError::bad_request("Spin rejected the outbound request scheme"))?;
        let authority = uri
            .authority()
            .ok_or_else(|| EdgeError::bad_request("outbound request URI has no authority"))?;
        request
            .set_authority(Some(authority.as_str()))
            .map_err(|()| EdgeError::bad_request("Spin rejected the outbound request authority"))?;
        request
            .set_path_with_query(Some(
                uri.path_and_query()
                    .map_or("/", |path_and_query| path_and_query.as_str()),
            ))
            .map_err(|()| EdgeError::bad_request("Spin rejected the outbound request path"))?;
        Ok(())
    }

    fn wasi_method(method: &Method) -> WasiMethod {
        match *method {
            Method::GET => WasiMethod::Get,
            Method::HEAD => WasiMethod::Head,
            Method::POST => WasiMethod::Post,
            Method::PUT => WasiMethod::Put,
            Method::DELETE => WasiMethod::Delete,
            Method::CONNECT => WasiMethod::Connect,
            Method::OPTIONS => WasiMethod::Options,
            Method::TRACE => WasiMethod::Trace,
            Method::PATCH => WasiMethod::Patch,
            _ => WasiMethod::Other(method.as_str().to_owned()),
        }
    }

    async fn pump_request_body(
        body: Body,
        maximum: u64,
        writer: BodyWriter,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> Result<UploadCompletion, EdgeError> {
        let BodyWriter {
            mut stream_writer,
            result_writer,
            ..
        } = writer;
        let mut total = 0_u64;
        let mut source = match body {
            Body::Once(bytes) => once(async move { Ok(bytes) }).boxed_local(),
            Body::Stream(source) => source,
        };

        while let Some(item) = source.next().await {
            budget_remaining(budget, &clock)?;
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => {
                    drop(stream_writer);
                    signal_upload_failure(result_writer, ErrorCode::InternalError(None)).await;
                    return Err(error);
                }
            };
            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let Some(next_total) = total.checked_add(length) else {
                drop(stream_writer);
                signal_upload_failure(result_writer, ErrorCode::HttpRequestBodySize(None)).await;
                return Err(EdgeError::bad_request(
                    "outbound request body size accounting overflow",
                ));
            };
            if next_total > maximum {
                drop(stream_writer);
                signal_upload_failure(
                    result_writer,
                    ErrorCode::HttpRequestBodySize(Some(next_total)),
                )
                .await;
                return Err(EdgeError::bad_request(
                    "outbound request body exceeded configured limit",
                ));
            }
            let unwritten = stream_writer.write_all(bytes.to_vec()).await;
            budget_remaining(budget, &clock)?;
            if !unwritten.is_empty() {
                drop(stream_writer);
                drop(result_writer.write(Ok(None)).await);
                return Ok(UploadCompletion::ReaderGone);
            }
            total = next_total;
            cooperative_yield_once().await;
        }

        drop(stream_writer);
        if result_writer.write(Ok(None)).await.is_err() {
            return Ok(UploadCompletion::ReaderGone);
        }
        Ok(UploadCompletion::Complete)
    }

    async fn signal_upload_failure(
        writer: FutureWriter<Result<Option<Fields>, ErrorCode>>,
        error: ErrorCode,
    ) {
        match writer.write(Err(error)).await {
            Ok(()) | Err(_) => {}
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the adapter consumes independent response-policy fields"
    )]
    async fn process_response(
        response: Response,
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
        let status = StatusCode::from_u16(response.get_status_code()).map_err(|_error| {
            EdgeError::bad_gateway_with_reason(
                "Spin returned an invalid upstream status code",
                BadGatewayReason::Protocol,
            )
        })?;
        let mut headers = response_headers(&response)?;
        let mut header_limiter =
            ResponseHeaderLimiter::new(max_response_header_bytes, max_response_header_count);
        header_limiter.observe(&headers)?;
        let disposition = normalize_response_headers(&request_method, status, &mut headers)?;
        headers.insert(PROXY_HEADER, HeaderValue::from_static("spin"));

        let native = super::cooperative_stream(response_stream(response, budget, clock.clone()));
        if disposition == ResponseBodyDisposition::FramingBodyless {
            drain_response(native, budget, &clock).await?;
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
        if matches!(disposition, ResponseBodyDisposition::ResetContent { .. }) {
            if !declared_reset_body {
                drain_response(native, budget, &clock).await?;
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
        let deadline_bound = deadline_stream(super::cooperative_stream(shaped), budget, clock);
        let body = match response_mode {
            ResponseMode::Buffered { max_bytes } => {
                Body::from(collect_response_stream(deadline_bound, max_bytes).await?)
            }
            ResponseMode::Streamed => Body::from_stream(deadline_bound),
        };
        Ok(OutboundResponse::new(request_method, status, headers, body))
    }

    fn response_headers(response: &Response) -> Result<HeaderMap, EdgeError> {
        let fields = response.get_headers();
        let mut headers = HeaderMap::new();
        for (raw_name, raw_value) in fields.copy_all() {
            let header_name = HeaderName::from_bytes(raw_name.as_bytes()).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "Spin returned an invalid upstream header name",
                    BadGatewayReason::Protocol,
                )
            })?;
            let header_value = HeaderValue::from_bytes(&raw_value).map_err(|_error| {
                EdgeError::bad_gateway_with_reason(
                    "Spin returned an invalid upstream header value",
                    BadGatewayReason::Protocol,
                )
            })?;
            headers.append(header_name, header_value);
        }
        Ok(headers)
    }

    fn default_response_result() -> Result<(), ErrorCode> {
        Err(ErrorCode::InternalError(Some(
            "response body consumer dropped before completion".to_owned(),
        )))
    }

    fn response_stream(
        response: Response,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> BodyStream {
        let (completion_writer, result_reader) = wit_future::new(default_response_result);
        let (mut body_reader, trailer_reader) = Response::consume_body(response, result_reader);
        stream! {
            loop {
                let read = body_reader.read(Vec::with_capacity(RESPONSE_READ_BYTES));
                let (result, chunk) = match race_deadline(read, budget, &clock).await {
                    Ok(value) => value,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                match result {
                    StreamResult::Complete(_length) => {
                        yield Ok(Bytes::from(chunk));
                    }
                    StreamResult::Dropped => {
                        let trailer_result = race_deadline(
                            async move { trailer_reader.await },
                            budget,
                            &clock,
                        )
                        .await;
                        match trailer_result {
                            Ok(Ok(_trailers)) => {}
                            Ok(Err(error)) => {
                                yield Err(map_spin_send_error(
                                    &error,
                                    budget.deadline,
                                    budget.cause,
                                    clock.now(),
                                ));
                                return;
                            }
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        }
                        let completion = completion_writer.write(Ok(()));
                        match race_deadline(completion, budget, &clock).await {
                            Ok(Ok(())) => return,
                            Ok(Err(_closed)) => {
                                yield Err(EdgeError::bad_gateway_with_reason(
                                    "Spin response completion reader closed",
                                    BadGatewayReason::Protocol,
                                ));
                                return;
                            }
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        }
                    }
                    StreamResult::Cancelled => {
                        yield Err(EdgeError::bad_gateway_with_reason(
                            "Spin cancelled the upstream response body read",
                            BadGatewayReason::Transport,
                        ));
                        return;
                    }
                }
            }
        }
        .boxed_local()
    }

    fn deadline_stream(
        mut source: BodyStream,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> BodyStream {
        stream! {
            loop {
                let next_item = match race_deadline(source.next(), budget, &clock).await {
                    Ok(item) => item,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
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

    async fn drain_response(
        mut body: BodyStream,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> Result<(), EdgeError> {
        while let Some(item) = body.next().await {
            item?;
            budget_remaining(budget, clock)?;
        }
        Ok(())
    }

    async fn race_deadline<Output>(
        future: impl Future<Output = Output>,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> Result<Output, EdgeError> {
        let remaining = budget_remaining(budget, clock)?;
        let timer = sleep(remaining);
        futures_util::pin_mut!(future, timer);
        let output = match select(future, timer).await {
            Either::Left((output, _timer)) => output,
            Either::Right(((), _future)) => return Err(timeout_error(budget.cause)),
        };
        budget_remaining(budget, clock)?;
        Ok(output)
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

    #[cfg(test)]
    mod clock_tests {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use edgezero_core::error::BudgetSource;
        use edgezero_core::time::Deadline;
        use futures::executor::block_on;
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

        #[test]
        fn method_entry_and_preflight_elapsed_use_the_injected_clock() {
            let start = MonotonicInstant::now();
            let completed = start
                .checked_add(Duration::from_millis(9))
                .expect("completed instant");
            let client = SpinOutboundClient::with_clock(scripted_clock(vec![start, completed]));
            let request = OutboundRequest::get("https://example.com/")
                .expect("request")
                .stream_response();

            let results = block_on(client.send_all(vec![request]));

            assert_eq!(results[0].elapsed, Duration::from_millis(9));
            assert!(matches!(
                results[0].outcome,
                Err(EdgeError::BadRequest { .. })
            ));
        }

        #[test]
        fn backwards_clock_fails_slot_without_invalid_elapsed() {
            let start = MonotonicInstant::now();
            let earlier = start
                .checked_sub(Duration::from_millis(1))
                .expect("earlier instant");
            let client = SpinOutboundClient::with_clock(scripted_clock(vec![start, earlier]));
            let request = OutboundRequest::get("https://example.com/")
                .expect("request")
                .stream_response();

            let results = block_on(client.send_all(vec![request]));

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

        #[test]
        fn request_option_preparation_consumes_injected_budget() {
            let start = MonotonicInstant::now();
            let budget = test_budget(start, Duration::from_millis(10));
            let observed = start
                .checked_add(Duration::from_millis(3))
                .expect("observed instant");
            let observations = Arc::new(Mutex::new(VecDeque::from([observed])));
            let clock_observations = Arc::clone(&observations);
            let clock = MonotonicClock::new(move || {
                clock_observations
                    .lock()
                    .expect("clock observations")
                    .pop_front()
                    .expect("clock observation")
            });

            let _options = request_options(budget, &clock).expect("request options");

            assert!(observations.lock().expect("clock observations").is_empty());
        }

        #[test]
        fn response_stream_retains_clock_for_post_ready_expiry() {
            let start = MonotonicInstant::now();
            let budget = test_budget(start, Duration::from_millis(10));
            let clock = scripted_clock(vec![start, budget.deadline.instant()]);
            let source = stream::once(async { Ok(Bytes::from_static(b"body")) }).boxed_local();
            let mut body = deadline_stream(source, budget, clock);

            let error = block_on(body.next())
                .expect("terminal item")
                .expect_err("post-ready expiry");

            assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
        }
    }
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use spin_impl::SpinOutboundClient;

#[cfg(feature = "spin")]
fn map_spin_send_error(
    error: &ErrorCode,
    deadline: Deadline,
    cause: BudgetSource,
    observed_at: edgezero_core::MonotonicInstant,
) -> EdgeError {
    if deadline.is_expired_at(observed_at) {
        return timeout_error(cause);
    }

    match error {
        ErrorCode::DnsTimeout
        | ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => timeout_error(BudgetSource::Unspecified),
        ErrorCode::HttpRequestDenied
        | ErrorCode::HttpRequestBodySize(_)
        | ErrorCode::HttpRequestUriTooLong
        | ErrorCode::HttpRequestHeaderSectionSize(_)
        | ErrorCode::HttpRequestHeaderSize(_) => {
            EdgeError::bad_request("outbound request was rejected by the Spin HTTP host")
        }
        ErrorCode::HttpRequestLengthRequired
        | ErrorCode::HttpRequestMethodInvalid
        | ErrorCode::HttpRequestUriInvalid
        | ErrorCode::HttpRequestTrailerSectionSize(_)
        | ErrorCode::HttpRequestTrailerSize(_)
        | ErrorCode::ConfigurationError => EdgeError::internal(anyhow::anyhow!(
            "Spin rejected an adapter-owned outbound request invariant"
        )),
        ErrorCode::DnsError(_)
        | ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpProhibited
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionLimitReached
        | ErrorCode::TlsProtocolError
        | ErrorCode::TlsCertificateError
        | ErrorCode::TlsAlertReceived(_) => EdgeError::bad_gateway_with_reason(
            "outbound destination could not be reached",
            BadGatewayReason::Unreachable,
        ),
        ErrorCode::ConnectionTerminated => EdgeError::bad_gateway_with_reason(
            "outbound connection failed after dispatch",
            BadGatewayReason::Transport,
        ),
        ErrorCode::HttpResponseIncomplete
        | ErrorCode::HttpResponseHeaderSectionSize(_)
        | ErrorCode::HttpResponseHeaderSize(_)
        | ErrorCode::HttpResponseBodySize(_)
        | ErrorCode::HttpResponseTrailerSectionSize(_)
        | ErrorCode::HttpResponseTrailerSize(_)
        | ErrorCode::HttpResponseTransferCoding(_)
        | ErrorCode::HttpResponseContentCoding(_)
        | ErrorCode::HttpUpgradeFailed
        | ErrorCode::HttpProtocolError
        | ErrorCode::LoopDetected => EdgeError::bad_gateway_with_reason(
            "upstream response violated the HTTP protocol",
            BadGatewayReason::Protocol,
        ),
        ErrorCode::InternalError(_) => EdgeError::bad_gateway_with_reason(
            "Spin outbound HTTP host failed",
            BadGatewayReason::Unspecified,
        ),
    }
}

#[cfg(feature = "spin")]
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

/// Runs the target-neutral Spin batch preflight contract in native tests.
///
/// # Errors
/// Returns the same portable validation or batch-shape error as production dispatch.
#[cfg(feature = "test-utils")]
#[inline]
pub fn validate_batch_request_for_test(request: &OutboundRequest) -> Result<(), EdgeError> {
    validate_batch_request(request)
}

/// Exposes the real pinned SDK classifier to WASI resource contract tests.
#[cfg(all(feature = "spin", feature = "test-utils"))]
#[must_use]
#[inline]
pub fn map_spin_send_error_for_test(
    error: &ErrorCode,
    deadline: Deadline,
    cause: BudgetSource,
    observed_at: edgezero_core::MonotonicInstant,
) -> EdgeError {
    map_spin_send_error(error, deadline, cause, observed_at)
}

#[cfg(test)]
mod exchange_tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures::executor::block_on;

    use super::{
        EdgeError, READY_ITEM_YIELD_QUOTA, UploadCompletion, cooperative_stream,
        cooperative_yield_once, request_body_limit, request_timeouts, run_exchange,
    };

    struct ScriptedFuture<Output> {
        drops: Rc<Cell<u32>>,
        polls: Rc<Cell<u32>>,
        steps: VecDeque<Poll<Output>>,
    }

    impl<Output> Unpin for ScriptedFuture<Output> {}

    impl<Output> ScriptedFuture<Output> {
        fn new(
            steps: impl IntoIterator<Item = Poll<Output>>,
            polls: Rc<Cell<u32>>,
            drops: Rc<Cell<u32>>,
        ) -> Self {
            Self {
                drops,
                polls,
                steps: steps.into_iter().collect(),
            }
        }
    }

    impl<Output> Future for ScriptedFuture<Output> {
        type Output = Output;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.set(self.polls.get().saturating_add(1));
            let step = self.steps.pop_front().expect("scripted future exhausted");
            if step.is_pending() {
                cx.waker().wake_by_ref();
            }
            step
        }
    }

    impl<Output> Drop for ScriptedFuture<Output> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get().saturating_add(1));
        }
    }

    fn counters() -> (Rc<Cell<u32>>, Rc<Cell<u32>>) {
        (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)))
    }

    fn map_host_error(_error: &&'static str) -> EdgeError {
        EdgeError::bad_gateway("scripted host failure")
    }

    #[test]
    fn continuously_ready_chunks_yield_after_each_item() {
        use futures_util::StreamExt as _;
        use futures_util::stream;
        use futures_util::task::noop_waker;

        let processed = Rc::new(Cell::new(0_u32));
        let observed = Rc::clone(&processed);
        let mut source = stream::iter([(), (), ()]);
        let mut pump = Box::pin(async move {
            while source.next().await.is_some() {
                observed.set(observed.get().saturating_add(1));
                cooperative_yield_once().await;
            }
        });
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(pump.as_mut().poll(&mut context).is_pending());
        assert_eq!(processed.get(), 1);
        assert!(pump.as_mut().poll(&mut context).is_pending());
        assert_eq!(processed.get(), 2);
        assert!(pump.as_mut().poll(&mut context).is_pending());
        assert_eq!(processed.get(), 3);
        assert!(pump.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn request_timeouts_use_full_remaining_budget_for_every_phase() {
        let timeouts = request_timeouts(Duration::from_millis(40));
        assert_eq!(timeouts.connect, 40_000_000);
        assert_eq!(timeouts.first_byte, 40_000_000);
        assert_eq!(timeouts.between_bytes, 40_000_000);
    }

    #[test]
    fn request_body_limit_only_applies_to_streamed_bodies() {
        use bytes::Bytes;
        use edgezero_core::body::Body;
        use futures_util::stream;

        assert_eq!(
            request_body_limit(&Body::from(Bytes::from_static(b"buffered")), 8),
            u64::MAX
        );
        assert_eq!(request_body_limit(&Body::stream(stream::empty()), 8), 8);
    }

    #[test]
    fn cooperative_stream_forces_a_pending_boundary_at_its_quota() {
        use std::iter::repeat_with;

        use bytes::Bytes;
        use futures_util::StreamExt as _;
        use futures_util::stream;
        use futures_util::task::noop_waker;

        let mut source = cooperative_stream(
            stream::iter(
                repeat_with(|| Ok::<Bytes, EdgeError>(Bytes::new()))
                    .take(READY_ITEM_YIELD_QUOTA.saturating_add(1)),
            )
            .boxed_local(),
        );
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        for _ in 0..READY_ITEM_YIELD_QUOTA {
            assert!(matches!(
                source.as_mut().poll_next(&mut context),
                Poll::Ready(Some(Ok(_)))
            ));
        }
        assert!(source.as_mut().poll_next(&mut context).is_pending());
        assert!(matches!(
            source.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(_)))
        ));
    }

    #[test]
    fn upload_failure_wins_over_simultaneously_ready_send() {
        let (send_polls, send_drops) = counters();
        let (pump_polls, pump_drops) = counters();
        let (done_polls, done_drops) = counters();
        let send = ScriptedFuture::new(
            [Poll::Ready(Ok(7_u8))],
            Rc::clone(&send_polls),
            Rc::clone(&send_drops),
        );
        let pump = ScriptedFuture::new(
            [Poll::Ready(Err(EdgeError::bad_request("upload failed")))],
            Rc::clone(&pump_polls),
            Rc::clone(&pump_drops),
        );
        let done = ScriptedFuture::new(
            [Poll::Ready(Ok(()))],
            Rc::clone(&done_polls),
            Rc::clone(&done_drops),
        );

        let outcome = block_on(run_exchange(send, pump, done, map_host_error));

        assert!(matches!(outcome, Err(EdgeError::BadRequest { .. })));
        assert_eq!(pump_polls.get(), 1);
        assert_eq!(send_polls.get(), 0);
        assert_eq!(done_polls.get(), 0);
        assert_eq!(send_drops.get(), 1);
        assert_eq!(pump_drops.get(), 1);
        assert_eq!(done_drops.get(), 1);
    }

    #[test]
    fn ready_send_after_pending_upload_is_authoritative() {
        let (send_polls, send_drops) = counters();
        let (pump_polls, pump_drops) = counters();
        let (done_polls, done_drops) = counters();
        let send = ScriptedFuture::new(
            [Poll::Ready(Ok(7_u8))],
            Rc::clone(&send_polls),
            Rc::clone(&send_drops),
        );
        let pump = ScriptedFuture::new(
            [
                Poll::Pending,
                Poll::Ready(Err(EdgeError::bad_request("late upload failure"))),
            ],
            Rc::clone(&pump_polls),
            Rc::clone(&pump_drops),
        );
        let done = ScriptedFuture::new(
            [Poll::Ready(Ok(()))],
            Rc::clone(&done_polls),
            Rc::clone(&done_drops),
        );

        let outcome = block_on(run_exchange(send, pump, done, map_host_error));

        assert_eq!(outcome.expect("send result"), 7);
        assert_eq!(pump_polls.get(), 1);
        assert_eq!(send_polls.get(), 1);
        assert_eq!(done_polls.get(), 0);
        assert_eq!(send_drops.get(), 1);
        assert_eq!(pump_drops.get(), 1);
        assert_eq!(done_drops.get(), 1);
    }

    #[test]
    fn request_done_error_wins_over_retained_send() {
        let (send_polls, send_drops) = counters();
        let (pump_polls, pump_drops) = counters();
        let (done_polls, done_drops) = counters();
        let send = ScriptedFuture::new(
            [Poll::Ready(Ok(7_u8))],
            Rc::clone(&send_polls),
            Rc::clone(&send_drops),
        );
        let pump = ScriptedFuture::new(
            [Poll::Ready(Ok(UploadCompletion::Complete))],
            pump_polls,
            Rc::clone(&pump_drops),
        );
        let done = ScriptedFuture::new(
            [Poll::Pending, Poll::Ready(Err("request completion failed"))],
            Rc::clone(&done_polls),
            Rc::clone(&done_drops),
        );

        let observed_send_drops = Rc::clone(&send_drops);
        let observed_pump_drops = Rc::clone(&pump_drops);
        let observed_done_drops = Rc::clone(&done_drops);
        let outcome = block_on(run_exchange(send, pump, done, move |_error| {
            assert_eq!(observed_send_drops.get(), 1);
            assert_eq!(observed_pump_drops.get(), 1);
            assert_eq!(observed_done_drops.get(), 1);
            EdgeError::bad_gateway("scripted host failure")
        }));

        assert!(matches!(outcome, Err(EdgeError::BadGateway { .. })));
        assert_eq!(send_polls.get(), 1);
        assert_eq!(done_polls.get(), 2);
    }

    #[test]
    fn request_done_success_releases_retained_send_without_repolling_it() {
        let (send_polls, send_drops) = counters();
        let (pump_polls, pump_drops) = counters();
        let (done_polls, done_drops) = counters();
        let send = ScriptedFuture::new([Poll::Ready(Ok(7_u8))], Rc::clone(&send_polls), send_drops);
        let pump = ScriptedFuture::new(
            [Poll::Ready(Ok(UploadCompletion::Complete))],
            pump_polls,
            pump_drops,
        );
        let done = ScriptedFuture::new(
            [Poll::Pending, Poll::Ready(Ok(()))],
            Rc::clone(&done_polls),
            done_drops,
        );

        let outcome = block_on(run_exchange(send, pump, done, map_host_error));

        assert_eq!(outcome.expect("retained send result"), 7);
        assert_eq!(send_polls.get(), 1);
        assert_eq!(done_polls.get(), 2);
    }

    #[test]
    fn reader_gone_never_polls_request_done() {
        let (send_polls, send_drops) = counters();
        let (pump_polls, pump_drops) = counters();
        let (done_polls, done_drops) = counters();
        let send = ScriptedFuture::new(
            [Poll::Pending, Poll::Ready(Ok(7_u8))],
            Rc::clone(&send_polls),
            send_drops,
        );
        let pump = ScriptedFuture::new(
            [Poll::Ready(Ok(UploadCompletion::ReaderGone))],
            pump_polls,
            pump_drops,
        );
        let done = ScriptedFuture::new([Poll::Ready(Ok(()))], Rc::clone(&done_polls), done_drops);

        let outcome = block_on(run_exchange(send, pump, done, map_host_error));

        assert_eq!(outcome.expect("early response"), 7);
        assert_eq!(send_polls.get(), 2);
        assert_eq!(done_polls.get(), 0);
    }
}
