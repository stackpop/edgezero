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

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod spin_impl {
    use std::future::Future;
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
    use edgezero_core::time::{DispatchBudget, MonotonicInstant, dispatch_budget};
    use futures_util::StreamExt as _;
    use futures_util::future::{Either, join, join_all, select};
    use futures_util::stream::once;
    use spin_sdk::time::sleep;
    use spin_sdk::wasip3::http::client;
    use spin_sdk::wasip3::http::types::{
        ErrorCode, Fields, Method as WasiMethod, Request, RequestOptions, RequestOptionsError,
        Response, Scheme,
    };
    use spin_sdk::wasip3::http_compat::BodyWriter;
    use spin_sdk::wasip3::wit_bindgen::{FutureReader, FutureWriter, StreamResult};
    use spin_sdk::wasip3::wit_future;

    use super::{map_spin_send_error, timeout_error};

    const RESPONSE_READ_BYTES: usize = 16 * 1024;
    const READY_ITEM_YIELD_QUOTA: u32 = 64;

    /// Native outbound HTTP implementation for Spin's WASI HTTP 0.3 host.
    pub struct SpinOutboundClient;

    struct PreparedRequest {
        budget: DispatchBudget,
        parts: OutboundRequestParts,
    }

    enum PreparedSlot {
        Finished(OutboundSlotResult),
        Pending(Box<PreparedRequest>),
    }

    enum UploadCompletion {
        Complete,
        ReaderGone,
    }

    impl SpinOutboundClient {
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

        async fn execute(prepared: PreparedRequest) -> Result<OutboundResponse, EdgeError> {
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
            let options = request_options(budget)?;
            let (writer, contents, trailers) = BodyWriter::new();
            let (request, request_done) =
                Request::new(fields, Some(contents), trailers, Some(options));
            set_request_target(&request, &method, &uri)?;

            let exchange = async move {
                let upload =
                    pump_request_body(body, max_request_body_bytes, writer, request_done, budget);
                let send = client::send(request);
                let (upload_outcome, send_outcome) = join(upload, send).await;
                upload_outcome?;
                send_outcome
                    .map_err(|error| map_spin_send_error(&error, budget.deadline, budget.cause))
            };
            let response = race_deadline(exchange, budget).await??;

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
            )
            .await
        }
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for SpinOutboundClient {
        #[inline]
        async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
            let started_at = MonotonicInstant::now();
            let prepared = Self::prepare(request, started_at)?;
            Self::execute(prepared).await
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
                        let outcome = Self::execute(*prepared).await;
                        finish_slot(batch_started_at, outcome)
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
                .map_err(|error| {
                    EdgeError::bad_request(format!(
                        "Spin rejected outbound request header {name}: {error}"
                    ))
                })?;
        }
        Ok(fields)
    }

    fn request_options(budget: DispatchBudget) -> Result<RequestOptions, EdgeError> {
        let options = RequestOptions::new();
        let remaining = budget_remaining(budget)?;
        let total = duration_nanos(remaining);
        let connect = duration_nanos(remaining.checked_div(4).unwrap_or(Duration::ZERO));
        let first_byte = duration_nanos(remaining.checked_div(2).unwrap_or(Duration::ZERO));
        set_timeout_option("connect", &options.set_connect_timeout(Some(connect)))?;
        set_timeout_option(
            "first-byte",
            &options.set_first_byte_timeout(Some(first_byte)),
        )?;
        set_timeout_option(
            "between-bytes",
            &options.set_between_bytes_timeout(Some(total)),
        )?;
        Ok(options)
    }

    fn duration_nanos(duration: Duration) -> u64 {
        u64::try_from(duration.as_nanos())
            .unwrap_or(u64::MAX)
            .max(1)
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
        request_done: FutureReader<Result<(), ErrorCode>>,
        budget: DispatchBudget,
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
            budget_remaining(budget)?;
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
            budget_remaining(budget)?;
            if !unwritten.is_empty() {
                drop(stream_writer);
                drop(result_writer);
                drop(request_done);
                return Ok(UploadCompletion::ReaderGone);
            }
            total = next_total;
            sleep(Duration::ZERO).await;
        }

        drop(stream_writer);
        if result_writer.write(Ok(None)).await.is_err() {
            drop(request_done);
            return Ok(UploadCompletion::ReaderGone);
        }
        match request_done.await {
            Ok(()) => Ok(UploadCompletion::Complete),
            Err(error) => Err(map_spin_send_error(&error, budget.deadline, budget.cause)),
        }
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
    ) -> Result<OutboundResponse, EdgeError> {
        budget_remaining(budget)?;
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

        let native = response_stream(response, budget);
        if disposition == ResponseBodyDisposition::FramingBodyless {
            drain_response(native, budget).await?;
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
                drain_response(native, budget).await?;
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
        let body = match response_mode {
            ResponseMode::Buffered { max_bytes } => {
                Body::from(collect_response_stream(shaped, max_bytes).await?)
            }
            ResponseMode::Streamed => Body::from_stream(shaped),
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

    fn response_stream(response: Response, budget: DispatchBudget) -> BodyStream {
        let (completion_writer, result_reader) = wit_future::new(default_response_result);
        let (mut body_reader, trailer_reader) = Response::consume_body(response, result_reader);
        stream! {
            let mut ready_items = 0_u32;
            loop {
                let read = body_reader.read(Vec::with_capacity(RESPONSE_READ_BYTES));
                let (result, chunk) = match race_deadline(read, budget).await {
                    Ok(value) => value,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                match result {
                    StreamResult::Complete(_length) => {
                        yield Ok(Bytes::from(chunk));
                        ready_items = ready_items.saturating_add(1);
                        if ready_items >= READY_ITEM_YIELD_QUOTA {
                            sleep(Duration::ZERO).await;
                            ready_items = 0;
                        }
                    }
                    StreamResult::Dropped => {
                        let trailer_result = race_deadline(
                            async move { trailer_reader.await },
                            budget,
                        )
                        .await;
                        match trailer_result {
                            Ok(Ok(_trailers)) => {}
                            Ok(Err(error)) => {
                                yield Err(map_spin_send_error(
                                    &error,
                                    budget.deadline,
                                    budget.cause,
                                ));
                                return;
                            }
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        }
                        let completion = completion_writer.write(Ok(()));
                        match race_deadline(completion, budget).await {
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

    async fn drain_response(mut body: BodyStream, budget: DispatchBudget) -> Result<(), EdgeError> {
        while let Some(item) = body.next().await {
            item?;
            budget_remaining(budget)?;
        }
        Ok(())
    }

    async fn race_deadline<Output>(
        future: impl Future<Output = Output>,
        budget: DispatchBudget,
    ) -> Result<Output, EdgeError> {
        let remaining = budget_remaining(budget)?;
        let timer = sleep(remaining);
        futures_util::pin_mut!(future, timer);
        let output = match select(future, timer).await {
            Either::Left((output, _timer)) => output,
            Either::Right(((), _future)) => return Err(timeout_error(budget.cause)),
        };
        budget_remaining(budget)?;
        Ok(output)
    }

    fn budget_remaining(budget: DispatchBudget) -> Result<Duration, EdgeError> {
        budget
            .deadline
            .remaining()
            .ok_or_else(|| timeout_error(budget.cause))
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
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use spin_impl::SpinOutboundClient;

#[cfg(feature = "spin")]
use edgezero_core::error::{BadGatewayReason, BudgetSource};
#[cfg(feature = "spin")]
use edgezero_core::time::Deadline;
#[cfg(feature = "spin")]
use spin_sdk::wasip3::http::types::ErrorCode;

#[cfg(feature = "spin")]
fn map_spin_send_error(error: &ErrorCode, deadline: Deadline, cause: BudgetSource) -> EdgeError {
    if deadline.is_expired() {
        return timeout_error(cause);
    }

    match error {
        ErrorCode::DnsTimeout
        | ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => timeout_error(BudgetSource::Unspecified),
        ErrorCode::HttpRequestDenied
        | ErrorCode::HttpRequestLengthRequired
        | ErrorCode::HttpRequestBodySize(_)
        | ErrorCode::HttpRequestMethodInvalid
        | ErrorCode::HttpRequestUriInvalid
        | ErrorCode::HttpRequestUriTooLong
        | ErrorCode::HttpRequestHeaderSectionSize(_)
        | ErrorCode::HttpRequestHeaderSize(_)
        | ErrorCode::HttpRequestTrailerSectionSize(_)
        | ErrorCode::HttpRequestTrailerSize(_) => {
            EdgeError::bad_request("outbound request was rejected by the Spin HTTP host")
        }
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
        ErrorCode::ConfigurationError => EdgeError::internal(anyhow::anyhow!(
            "Spin outbound HTTP configuration is invalid"
        )),
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
) -> EdgeError {
    map_spin_send_error(error, deadline, cause)
}
