#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "the target-gated implementation module is kept before shared test seams"
)]
#![expect(
    clippy::pub_use,
    reason = "the target-gated implementation keeps Fastly imports out of native builds"
)]

#[cfg(feature = "fastly")]
use edgezero_core::error::BudgetSource;
use edgezero_core::error::EdgeError;
use edgezero_core::outbound::{OutboundRequest, validate_for_dispatch};

#[cfg(feature = "fastly")]
mod fastly_impl {
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::mem::replace;
    use std::num::NonZeroU64;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_stream::stream;
    use async_trait::async_trait;
    use bytes::Bytes;
    use edgezero_core::body::{Body, BodyStream};
    use edgezero_core::compression::{
        ContentEncoding, classify_content_encoding, decode_brotli_stream, decode_gzip_stream,
    };
    use edgezero_core::error::{BadGatewayReason, BudgetSource, EdgeError};
    use edgezero_core::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, HOST};
    use edgezero_core::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
    use edgezero_core::outbound::{
        OutboundHttpClient, OutboundRequest, OutboundRequestParts, OutboundResponse,
        OutboundSlotResult, PROXY_HEADER, ResponseBodyDisposition, ResponseHeaderLimiter,
        ResponseMode, collect_response_stream, enforce_payload_content_length,
        limit_decoded_stream, limit_encoded_stream, normalize_for_dispatch,
        normalize_response_headers, rechunk_stream, validate_for_dispatch,
    };
    use edgezero_core::time::{DispatchBudget, MonotonicInstant, dispatch_budget};
    use fastly::backend::BackendCreationError;
    use fastly::http::request::{PendingRequest, PollResult, SendError, SendErrorCause};
    use fastly::{
        Backend, Body as FastlyBody, Request as FastlyRequest, Response as FastlyResponse,
    };
    use futures_util::StreamExt as _;
    use sha2::{Digest as _, Sha256};

    use super::{dispatch_all_before_wait, timeout_error, validate_batch_request};

    pub const DYNAMIC_BACKENDS_DISABLED_MESSAGE: &str = "Fastly dynamic backends are not enabled on this service; enable them in the service configuration";
    const RESPONSE_READ_BYTES: usize = 16 * 1024;

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct BackendIdentity {
        budget_ms: u64,
        host: String,
        port: u16,
        scheme: String,
        tls: bool,
    }

    struct PreparedRequest {
        backend: Backend,
        budget: DispatchBudget,
        parts: OutboundRequestParts,
    }

    struct PendingSlot {
        budget: DispatchBudget,
        max_brotli_decoder_bytes: u64,
        max_brotli_window_bits: u8,
        max_chunk_bytes: Option<NonZeroU64>,
        max_decoded_response_bytes: Option<u64>,
        max_encoded_response_bytes: Option<u64>,
        max_response_header_bytes: Option<u64>,
        max_response_header_count: Option<u64>,
        pending: PendingRequest,
        request_method: Method,
        response_mode: ResponseMode,
    }

    enum Slot {
        Done(OutboundSlotResult),
        Pending(Box<PendingSlot>),
        Taken,
    }

    /// Native outbound HTTP implementation for Fastly Compute.
    pub struct FastlyOutboundClient {
        backends: Mutex<HashMap<String, (BackendIdentity, Backend)>>,
    }

    impl FastlyOutboundClient {
        #[must_use]
        #[inline]
        pub fn new() -> Self {
            Self {
                backends: Mutex::new(HashMap::new()),
            }
        }

        fn ensure_backend(
            &self,
            request: &OutboundRequest,
            budget: DispatchBudget,
        ) -> Result<Backend, EdgeError> {
            let identity = backend_identity(request, budget)?;
            let name = backend_name(&identity);
            {
                let cache = self.backends.lock().map_err(|_poisoned| {
                    EdgeError::internal(anyhow::anyhow!("Fastly backend cache was poisoned"))
                })?;
                if let Some((cached_identity, backend)) = cache.get(&name) {
                    if cached_identity != &identity {
                        return Err(EdgeError::internal(anyhow::anyhow!(
                            "dynamic backend name collision; refusing to reuse"
                        )));
                    }
                    return Ok(backend.clone());
                }
            }

            let target = request.backend_target();
            let host_override = request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_else(|| request.host_name());
            let timers = backend_timers(identity.budget_ms);
            let mut builder = Backend::builder(&name, &target)
                .override_host(host_override)
                .connect_timeout(timers.connect)
                .first_byte_timeout(timers.first_byte)
                .between_bytes_timeout(timers.between_bytes);
            if identity.tls {
                builder = builder.enable_ssl();
                if let Some(sni) = request.sni_hostname() {
                    builder = builder.sni_hostname(sni);
                }
                if let Some(cert_host) = request.cert_host() {
                    builder = builder.check_certificate(cert_host);
                }
            }
            let backend = builder
                .finish()
                .map_err(|error| map_backend_creation_error(&error))?;

            let mut cache = self.backends.lock().map_err(|_poisoned| {
                EdgeError::internal(anyhow::anyhow!("Fastly backend cache was poisoned"))
            })?;
            if let Some((cached_identity, cached_backend)) = cache.get(&name) {
                if cached_identity != &identity {
                    return Err(EdgeError::internal(anyhow::anyhow!(
                        "dynamic backend name collision; refusing to reuse"
                    )));
                }
                return Ok(cached_backend.clone());
            }
            cache.insert(name, (identity, backend.clone()));
            Ok(backend)
        }

        fn prepare(
            &self,
            request: OutboundRequest,
            started_at: MonotonicInstant,
        ) -> Result<PreparedRequest, EdgeError> {
            validate_for_dispatch(&request)?;
            self.prepare_validated(request, started_at)
        }

        fn prepare_batch(
            &self,
            request: OutboundRequest,
            started_at: MonotonicInstant,
        ) -> Result<PreparedRequest, EdgeError> {
            validate_batch_request(&request)?;
            self.prepare_validated(request, started_at)
        }

        fn prepare_validated(
            &self,
            mut request: OutboundRequest,
            started_at: MonotonicInstant,
        ) -> Result<PreparedRequest, EdgeError> {
            let budget = dispatch_budget(&request, started_at)?;
            if !request.headers().contains_key(ACCEPT_ENCODING) {
                request
                    .headers_mut()
                    .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
            }
            normalize_for_dispatch(&mut request)?;
            budget_remaining(budget)?;
            let backend = self.ensure_backend(&request, budget)?;
            budget_remaining(budget)?;
            Ok(PreparedRequest {
                backend,
                budget,
                parts: request.into_parts(),
            })
        }

        async fn execute(&self, prepared: PreparedRequest) -> Result<OutboundResponse, EdgeError> {
            let PreparedRequest {
                backend,
                budget,
                parts,
            } = prepared;
            let OutboundRequestParts {
                body,
                headers,
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
            let fastly_request = build_fastly_request(&method, &uri, &headers);
            let response = match body {
                Body::Once(bytes) => {
                    validate_request_body_length(&bytes, max_request_body_bytes)?;
                    let mut buffered_request = fastly_request;
                    buffered_request.set_body(bytes.to_vec());
                    budget_remaining(budget)?;
                    let pending = buffered_request
                        .send_async(backend)
                        .map_err(|error| map_send_error(&error, budget))?;
                    wait_pending(pending, budget)?
                }
                Body::Stream(source) => {
                    send_streamed(
                        fastly_request,
                        backend,
                        source,
                        max_request_body_bytes,
                        budget,
                    )
                    .await?
                }
            };
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

        fn dispatch_batch_slot(prepared: PreparedRequest) -> Result<PendingSlot, EdgeError> {
            let PreparedRequest {
                backend,
                budget,
                parts,
            } = prepared;
            let OutboundRequestParts {
                body,
                headers,
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
            let Body::Once(bytes) = body else {
                return Err(EdgeError::internal(anyhow::anyhow!(
                    "Fastly batch preflight admitted a streamed upload"
                )));
            };
            validate_request_body_length(&bytes, max_request_body_bytes)?;
            let mut request = build_fastly_request(&method, &uri, &headers);
            request.set_body(bytes.to_vec());
            budget_remaining(budget)?;
            let pending = request
                .send_async(backend)
                .map_err(|error| map_send_error(&error, budget))?;
            Ok(PendingSlot {
                budget,
                max_brotli_decoder_bytes,
                max_brotli_window_bits,
                max_chunk_bytes,
                max_decoded_response_bytes,
                max_encoded_response_bytes,
                max_response_header_bytes,
                max_response_header_count,
                pending,
                request_method: method,
                response_mode,
            })
        }
    }

    impl Default for FastlyOutboundClient {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for FastlyOutboundClient {
        #[inline]
        async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
            let started_at = MonotonicInstant::now();
            let prepared = self.prepare(request, started_at)?;
            self.execute(prepared).await
        }

        #[inline]
        async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
            let batch_started_at = MonotonicInstant::now();
            let mut slots: Vec<Slot> = dispatch_all_before_wait(requests, |request| {
                self.prepare_batch(request, batch_started_at)
                    .and_then(Self::dispatch_batch_slot)
            })
            .into_iter()
            .map(|result| {
                result.map_or_else(
                    |error| Slot::Done(finish_slot(batch_started_at, Err(error))),
                    |pending| Slot::Pending(Box::new(pending)),
                )
            })
            .collect();

            for index in 0..slots.len() {
                let Some(slot) = slots.get_mut(index) else {
                    continue;
                };
                let current = replace(slot, Slot::Taken);
                *slot = if let Slot::Pending(pending) = current {
                    let outcome = finish_pending(*pending).await;
                    Slot::Done(finish_slot(batch_started_at, outcome))
                } else {
                    current
                };

                for later in slots.iter_mut().skip(index.saturating_add(1)) {
                    poll_slot(later, batch_started_at).await;
                }
            }

            slots
                .into_iter()
                .map(|slot| match slot {
                    Slot::Done(done) => done,
                    Slot::Pending(_) | Slot::Taken => finish_slot(
                        batch_started_at,
                        Err(EdgeError::internal(anyhow::anyhow!(
                            "Fastly batch harvest left an unresolved slot"
                        ))),
                    ),
                })
                .collect()
        }
    }

    #[derive(Clone, Copy)]
    struct BackendTimers {
        between_bytes: Duration,
        connect: Duration,
        first_byte: Duration,
    }

    fn backend_identity(
        request: &OutboundRequest,
        budget: DispatchBudget,
    ) -> Result<BackendIdentity, EdgeError> {
        let remaining = budget_remaining(budget)?;
        let budget_ms = ceil_millis(remaining);
        let scheme = request
            .uri()
            .scheme_str()
            .ok_or_else(|| EdgeError::bad_request("outbound request URI has no scheme"))?;
        let port = request
            .uri()
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        Ok(BackendIdentity {
            budget_ms,
            host: request.host_name().to_owned(),
            port,
            scheme: scheme.to_owned(),
            tls: scheme == "https",
        })
    }

    fn backend_name(identity: &BackendIdentity) -> String {
        let tls_mode = if identity.tls { "tls" } else { "plain" };
        let canonical = format!(
            "{}:{}:{}:{}:{}",
            identity.scheme, identity.host, identity.port, tls_mode, identity.budget_ms
        );
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let mut name = String::from("ez_");
        let Some(prefix) = digest.get(..16) else {
            return name;
        };
        for byte in prefix {
            let high = u32::from(*byte >> 4_u8);
            let low = u32::from(*byte & 0x0f);
            if let Some(character) = char::from_digit(high, 16) {
                name.push(character);
            }
            if let Some(character) = char::from_digit(low, 16) {
                name.push(character);
            }
        }
        name
    }

    fn backend_timers(total_ms: u64) -> BackendTimers {
        let total = Duration::from_millis(total_ms.max(1));
        if total_ms < 4 {
            return BackendTimers {
                between_bytes: total,
                connect: total,
                first_byte: total,
            };
        }
        let connect = total.checked_div(4).unwrap_or(total);
        BackendTimers {
            between_bytes: total,
            connect,
            first_byte: total.saturating_sub(connect),
        }
    }

    fn build_fastly_request(method: &Method, uri: &Uri, headers: &HeaderMap) -> FastlyRequest {
        let mut request = FastlyRequest::new(method.clone(), uri.to_string());
        request.set_method(method.clone());
        for (name, value) in headers {
            request.append_header(name.as_str(), value.as_bytes());
        }
        request
    }

    fn ceil_millis(duration: Duration) -> u64 {
        let rounded = duration
            .checked_add(Duration::from_micros(999))
            .unwrap_or(Duration::MAX);
        u64::try_from(rounded.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    async fn finish_pending(slot: PendingSlot) -> Result<OutboundResponse, EdgeError> {
        let PendingSlot {
            budget,
            max_brotli_decoder_bytes,
            max_brotli_window_bits,
            max_chunk_bytes,
            max_decoded_response_bytes,
            max_encoded_response_bytes,
            max_response_header_bytes,
            max_response_header_count,
            pending,
            request_method,
            response_mode,
        } = slot;
        let response = wait_pending(pending, budget)?;
        process_response(
            response,
            request_method,
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

    fn map_backend_creation_error(error: &BackendCreationError) -> EdgeError {
        match error {
            BackendCreationError::Disallowed => EdgeError::bad_gateway_with_reason(
                DYNAMIC_BACKENDS_DISABLED_MESSAGE,
                BadGatewayReason::Unspecified,
            ),
            BackendCreationError::NameInUse => EdgeError::internal(anyhow::anyhow!(
                "dynamic backend name collision; refusing to reuse"
            )),
            BackendCreationError::BetweenBytesTimeoutTooLarge(_)
            | BackendCreationError::ConnectTimeoutTooLarge(_)
            | BackendCreationError::EncodingError(_)
            | BackendCreationError::FirstByteTimeoutTooLarge(_)
            | BackendCreationError::NameTooLong(_) => EdgeError::internal(anyhow::anyhow!(
                "Fastly rejected deterministic dynamic backend configuration"
            )),
            BackendCreationError::HostError(_) => EdgeError::bad_gateway_with_reason(
                "Fastly failed to register a dynamic backend",
                BadGatewayReason::Unspecified,
            ),
        }
    }

    fn map_send_error(error: &SendError, budget: DispatchBudget) -> EdgeError {
        if budget.deadline.is_expired() {
            return timeout_error(budget.cause);
        }
        match error.root_cause() {
            SendErrorCause::ConnectionTimeout | SendErrorCause::HttpResponseTimeout => {
                timeout_error(budget.cause)
            }
            SendErrorCause::DnsTimeout => timeout_error(BudgetSource::Unspecified),
            SendErrorCause::ConnectionLimitReached
            | SendErrorCause::ConnectionRefused
            | SendErrorCause::DestinationIpUnroutable
            | SendErrorCause::DestinationNotFound
            | SendErrorCause::DestinationUnavailable
            | SendErrorCause::DnsError { .. }
            | SendErrorCause::TlsAlertReceived { .. }
            | SendErrorCause::TlsCertificateError
            | SendErrorCause::TlsConfigurationError
            | SendErrorCause::TlsProtocolError => EdgeError::bad_gateway_with_reason(
                "outbound destination could not be reached",
                BadGatewayReason::Unreachable,
            ),
            SendErrorCause::ConnectionTerminated | SendErrorCause::IoError(_) => {
                EdgeError::bad_gateway_with_reason(
                    "outbound connection failed after dispatch",
                    BadGatewayReason::Transport,
                )
            }
            SendErrorCause::Http2StreamError { .. }
            | SendErrorCause::HttpIncompleteResponse
            | SendErrorCause::HttpProtocolError
            | SendErrorCause::HttpResponseBodyTooLarge
            | SendErrorCause::HttpResponseHeaderSectionTooLarge
            | SendErrorCause::HttpResponseStatusInvalid
            | SendErrorCause::HttpUpgradeFailed => EdgeError::bad_gateway_with_reason(
                "upstream response violated the HTTP protocol",
                BadGatewayReason::Protocol,
            ),
            SendErrorCause::HttpCacheApiUnsupported
            | SendErrorCause::HttpCacheLimitExceeded
            | SendErrorCause::HttpRequestCacheKeyInvalid
            | SendErrorCause::HttpRequestUriInvalid
            | SendErrorCause::ImageOptimizerUnsupported
            | SendErrorCause::InternalError(_)
            | SendErrorCause::Custom(_) => EdgeError::internal(anyhow::anyhow!(
                "Fastly rejected an adapter-owned outbound operation"
            )),
            _ => EdgeError::bad_gateway_with_reason(
                "Fastly outbound request failed",
                BadGatewayReason::Unspecified,
            ),
        }
    }

    async fn poll_slot(slot: &mut Slot, started_at: MonotonicInstant) {
        let current = replace(slot, Slot::Taken);
        let Slot::Pending(pending_slot) = current else {
            *slot = current;
            return;
        };
        let PendingSlot {
            budget,
            max_brotli_decoder_bytes,
            max_brotli_window_bits,
            max_chunk_bytes,
            max_decoded_response_bytes,
            max_encoded_response_bytes,
            max_response_header_bytes,
            max_response_header_count,
            pending: pending_request,
            request_method,
            response_mode,
        } = *pending_slot;
        match pending_request.poll() {
            PollResult::Pending(still_pending) => {
                *slot = Slot::Pending(Box::new(PendingSlot {
                    budget,
                    max_brotli_decoder_bytes,
                    max_brotli_window_bits,
                    max_chunk_bytes,
                    max_decoded_response_bytes,
                    max_encoded_response_bytes,
                    max_response_header_bytes,
                    max_response_header_count,
                    pending: still_pending,
                    request_method,
                    response_mode,
                }));
            }
            PollResult::Done(result) => {
                let outcome = match result {
                    Ok(response) => {
                        process_response(
                            response,
                            request_method,
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
                    Err(error) => Err(map_send_error(&error, budget)),
                };
                *slot = Slot::Done(finish_slot(started_at, outcome));
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the adapter consumes independent response-policy fields"
    )]
    async fn process_response(
        mut response: FastlyResponse,
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
        let status = StatusCode::from_u16(response.get_status().as_u16()).map_err(|_error| {
            EdgeError::bad_gateway_with_reason(
                "Fastly returned an invalid upstream status code",
                BadGatewayReason::Protocol,
            )
        })?;
        let mut headers = response_headers(&response);
        let mut header_limiter =
            ResponseHeaderLimiter::new(max_response_header_bytes, max_response_header_count);
        header_limiter.observe(&headers)?;
        let disposition = normalize_response_headers(&request_method, status, &mut headers)?;
        headers.insert(PROXY_HEADER, HeaderValue::from_static("fastly"));
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
        let native = fastly_body_stream(response.take_body(), budget);
        if matches!(disposition, ResponseBodyDisposition::ResetContent { .. }) {
            if !declared_reset_body {
                let mut reset_stream = native;
                if let Some(item) = reset_stream.next().await {
                    item?;
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
        let body = match response_mode {
            ResponseMode::Buffered { max_bytes } => {
                Body::from(collect_response_stream(shaped, max_bytes).await?)
            }
            ResponseMode::Streamed => Body::from_stream(shaped),
        };
        Ok(OutboundResponse::new(request_method, status, headers, body))
    }

    fn response_headers(response: &FastlyResponse) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for name in response.get_header_names() {
            for value in response.get_header_all(name) {
                headers.append(name.clone(), value.clone());
            }
        }
        headers
    }

    async fn send_streamed(
        request: FastlyRequest,
        backend: Backend,
        mut source: BodyStream,
        maximum: u64,
        budget: DispatchBudget,
    ) -> Result<FastlyResponse, EdgeError> {
        budget_remaining(budget)?;
        let (mut writer, pending) = request
            .send_async_streaming(backend)
            .map_err(|error| map_send_error(&error, budget))?;
        let mut total = 0_u64;
        while let Some(item) = source.next().await {
            budget_remaining(budget)?;
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
            writer.write_all(&bytes).map_err(|error| {
                EdgeError::bad_gateway_with_reason(
                    format!("Fastly outbound request body write failed: {error}"),
                    BadGatewayReason::Transport,
                )
            })?;
            writer.flush().map_err(|error| {
                EdgeError::bad_gateway_with_reason(
                    format!("Fastly outbound request body flush failed: {error}"),
                    BadGatewayReason::Transport,
                )
            })?;
            budget_remaining(budget)?;
            total = next_total;
        }
        writer.finish().map_err(|error| {
            EdgeError::bad_gateway_with_reason(
                format!("Fastly outbound request completion failed: {error}"),
                BadGatewayReason::Transport,
            )
        })?;
        budget_remaining(budget)?;
        wait_pending(pending, budget)
    }

    fn fastly_body_stream(mut body: FastlyBody, budget: DispatchBudget) -> BodyStream {
        stream! {
            loop {
                budget_remaining(budget)?;
                let mut chunks = body.read_chunks(RESPONSE_READ_BYTES);
                let item = chunks.next();
                drop(chunks);
                match item {
                    Some(Ok(chunk)) => {
                        budget_remaining(budget)?;
                        yield Ok(Bytes::from(chunk));
                    }
                    Some(Err(error)) => {
                        if budget.deadline.is_expired() {
                            yield Err(timeout_error(budget.cause));
                        } else {
                            yield Err(EdgeError::bad_gateway_with_reason(
                                format!("Fastly upstream response body failed: {error}"),
                                BadGatewayReason::Transport,
                            ));
                        }
                        return;
                    }
                    None => {
                        budget_remaining(budget)?;
                        return;
                    }
                }
            }
        }
        .boxed_local()
    }

    fn validate_request_body_length(bytes: &Bytes, maximum: u64) -> Result<(), EdgeError> {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if length > maximum {
            return Err(EdgeError::bad_request(
                "outbound request body exceeded configured limit",
            ));
        }
        Ok(())
    }

    fn wait_pending(
        pending: PendingRequest,
        budget: DispatchBudget,
    ) -> Result<FastlyResponse, EdgeError> {
        budget_remaining(budget)?;
        let outcome = pending
            .wait()
            .map_err(|error| map_send_error(&error, budget));
        budget_remaining(budget)?;
        outcome
    }

    fn budget_remaining(budget: DispatchBudget) -> Result<Duration, EdgeError> {
        budget
            .deadline
            .remaining()
            .ok_or_else(|| timeout_error(budget.cause))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn backend_creation_error_table_is_exhaustive() {
            let mapped = map_backend_creation_error(&BackendCreationError::Disallowed);
            assert_eq!(mapped.status(), StatusCode::BAD_GATEWAY);
            assert!(matches!(
                mapped,
                EdgeError::BadGateway {
                    message,
                    reason: BadGatewayReason::Unspecified,
                } if message == DYNAMIC_BACKENDS_DISABLED_MESSAGE
            ));

            let collision = map_backend_creation_error(&BackendCreationError::NameInUse);
            assert!(matches!(collision, EdgeError::Internal { .. }));
        }

        #[test]
        fn backend_identity_uses_every_canonical_property() {
            let first = BackendIdentity {
                budget_ms: 100,
                host: "example.com".to_owned(),
                port: 443,
                scheme: "https".to_owned(),
                tls: true,
            };
            assert_eq!(backend_name(&first), "ez_ac691b3a5fb10e3bd0923dbbf2dddbde");
            let mut second = first.clone();
            second.budget_ms = 101;
            assert_ne!(backend_name(&first), backend_name(&second));
            second = first.clone();
            second.port = 8443;
            assert_ne!(backend_name(&first), backend_name(&second));
            second = first.clone();
            second.host = "other.example".to_owned();
            assert_ne!(backend_name(&first), backend_name(&second));
        }

        #[test]
        fn backend_timer_partition_is_bounded() {
            let timers = backend_timers(100);
            assert_eq!(timers.connect, Duration::from_millis(25));
            assert_eq!(timers.first_byte, Duration::from_millis(75));
            assert_eq!(timers.between_bytes, Duration::from_millis(100));
            let short = backend_timers(1);
            assert_eq!(short.connect, Duration::from_millis(1));
            assert_eq!(short.first_byte, Duration::from_millis(1));
            assert_eq!(short.between_bytes, Duration::from_millis(1));
        }
    }
}

#[cfg(feature = "fastly")]
pub use fastly_impl::{DYNAMIC_BACKENDS_DISABLED_MESSAGE, FastlyOutboundClient};

fn dispatch_all_before_wait<Item, Pending, Failure>(
    items: impl IntoIterator<Item = Item>,
    dispatch: impl FnMut(Item) -> Result<Pending, Failure>,
) -> Vec<Result<Pending, Failure>> {
    items.into_iter().map(dispatch).collect()
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

/// Runs the target-neutral Fastly batch preflight contract in native tests.
///
/// # Errors
/// Returns the same portable validation or batch-shape error as production dispatch.
#[cfg(feature = "test-utils")]
#[inline]
pub fn validate_batch_request_for_test(request: &OutboundRequest) -> Result<(), EdgeError> {
    validate_batch_request(request)
}

/// Runs the same eager dispatch phase used by Fastly production batching.
#[cfg(feature = "test-utils")]
#[inline]
pub fn dispatch_all_before_wait_for_test<Items, Dispatch, Item, Pending, Failure>(
    items: Items,
    dispatch: Dispatch,
) -> Vec<Result<Pending, Failure>>
where
    Items: IntoIterator<Item = Item>,
    Dispatch: FnMut(Item) -> Result<Pending, Failure>,
{
    dispatch_all_before_wait(items, dispatch)
}

#[cfg(feature = "fastly")]
fn timeout_error(cause: BudgetSource) -> EdgeError {
    EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
}
