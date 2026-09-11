#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "the target-gated implementation module is kept before shared test seams"
)]
#![expect(
    clippy::pub_use,
    reason = "the target-gated implementation keeps Fastly imports out of native builds"
)]

#[cfg(test)]
use std::time::Duration;

use edgezero_core::error::EdgeError;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::error::{BadGatewayReason, BudgetSource};
use edgezero_core::outbound::{OutboundRequest, validate_for_dispatch};
#[cfg(test)]
use edgezero_core::time::Deadline;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::time::{DispatchBudget, MonotonicInstant};

#[cfg(any(feature = "fastly", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendFailure {
    BudgetedTimeout,
    LocalInvariant,
    PlatformInternal,
    ProviderTimeout,
    Transport,
    Unknown,
    Unreachable,
    UpstreamProtocol,
}

#[cfg(any(feature = "fastly", test))]
fn classify_send_failure(
    failure: SendFailure,
    budget: DispatchBudget,
    observed_at: MonotonicInstant,
) -> EdgeError {
    if budget.deadline.is_expired_at(observed_at) {
        return timeout_error(budget.cause);
    }
    match failure {
        SendFailure::BudgetedTimeout => timeout_error(budget.cause),
        SendFailure::ProviderTimeout => timeout_error(BudgetSource::Unspecified),
        SendFailure::Unreachable => EdgeError::bad_gateway_with_reason(
            "outbound destination could not be reached",
            BadGatewayReason::Unreachable,
        ),
        SendFailure::Transport => EdgeError::bad_gateway_with_reason(
            "outbound connection failed after dispatch",
            BadGatewayReason::Transport,
        ),
        SendFailure::UpstreamProtocol => EdgeError::bad_gateway_with_reason(
            "upstream response violated the HTTP protocol",
            BadGatewayReason::Protocol,
        ),
        SendFailure::Unknown => EdgeError::bad_gateway_with_reason(
            "Fastly outbound request failed",
            BadGatewayReason::Unspecified,
        ),
        SendFailure::LocalInvariant | SendFailure::PlatformInternal => EdgeError::internal(
            anyhow::anyhow!("Fastly rejected an adapter-owned outbound operation"),
        ),
    }
}

#[cfg(test)]
fn test_budget(started_at: MonotonicInstant, cause: BudgetSource) -> DispatchBudget {
    let duration = Duration::from_secs(1);
    DispatchBudget {
        cause,
        deadline: Deadline::at_instant(started_at.checked_add(duration).expect("deadline instant")),
        duration,
    }
}

#[cfg(test)]
fn assert_send_failure_error(failure: SendFailure, error: &EdgeError, selected: BudgetSource) {
    match failure {
        SendFailure::BudgetedTimeout => {
            if let EdgeError::GatewayTimeout { cause, .. } = error {
                assert_eq!(*cause, selected);
            } else {
                panic!("expected selected timeout");
            }
        }
        SendFailure::ProviderTimeout => assert!(matches!(
            error,
            EdgeError::GatewayTimeout {
                cause: BudgetSource::Unspecified,
                ..
            }
        )),
        SendFailure::LocalInvariant | SendFailure::PlatformInternal => {
            assert!(matches!(error, EdgeError::Internal { .. }));
        }
        SendFailure::Transport => assert!(matches!(
            error,
            EdgeError::BadGateway {
                reason: BadGatewayReason::Transport,
                ..
            }
        )),
        SendFailure::Unknown => assert!(matches!(
            error,
            EdgeError::BadGateway {
                reason: BadGatewayReason::Unspecified,
                ..
            }
        )),
        SendFailure::Unreachable => assert!(matches!(
            error,
            EdgeError::BadGateway {
                reason: BadGatewayReason::Unreachable,
                ..
            }
        )),
        SendFailure::UpstreamProtocol => assert!(matches!(
            error,
            EdgeError::BadGateway {
                reason: BadGatewayReason::Protocol,
                ..
            }
        )),
    }
}

#[cfg(feature = "fastly")]
mod fastly_impl {
    #[cfg(feature = "test-utils")]
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::io::{Result as IoResult, Write as _};
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
    use edgezero_core::error::{BadGatewayReason, EdgeError};
    use edgezero_core::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH};
    use edgezero_core::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
    use edgezero_core::outbound::{
        OutboundHttpClient, OutboundRequest, OutboundRequestParts, OutboundResponse,
        OutboundSlotResult, PROXY_HEADER, ResponseBodyDisposition, ResponseHeaderLimiter,
        ResponseMode, collect_response_stream, enforce_payload_content_length,
        limit_decoded_stream, limit_encoded_stream, normalize_for_dispatch,
        normalize_response_headers, rechunk_stream, validate_for_dispatch,
    };
    use edgezero_core::time::{
        BATCH_DISPATCH_SLACK_MAX, DispatchBudget, MonotonicClock, MonotonicInstant, dispatch_budget,
    };
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
    const DISPATCH_SLACK_MESSAGE: &str = "Fastly send_all adapter overhead between batch_now and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration";

    #[cfg(feature = "test-utils")]
    std::thread_local! {
        static DISPATCH_SLACK_INJECTION: Cell<Option<Duration>> = const { Cell::new(None) };
    }

    #[cfg(feature = "test-utils")]
    struct DispatchSlackInjection {
        previous: Option<Duration>,
    }

    #[cfg(feature = "test-utils")]
    impl Drop for DispatchSlackInjection {
        fn drop(&mut self) {
            DISPATCH_SLACK_INJECTION.set(self.previous);
        }
    }

    /// Overrides the dispatch-guard clock offset for one target-runtime contract test.
    #[cfg(feature = "test-utils")]
    #[inline]
    #[must_use]
    pub fn inject_dispatch_slack_for_test(delay: Duration) -> impl Drop {
        let previous = DISPATCH_SLACK_INJECTION.replace(Some(delay));
        DispatchSlackInjection { previous }
    }

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
        started_at: MonotonicInstant,
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
        clock: MonotonicClock,
    }

    impl FastlyOutboundClient {
        #[must_use]
        #[inline]
        pub fn new() -> Self {
            Self::with_clock(MonotonicClock::default())
        }

        /// Builds a client that evaluates every outbound lifetime against `clock`.
        #[must_use]
        #[inline]
        pub fn with_clock(clock: MonotonicClock) -> Self {
            Self {
                backends: Mutex::new(HashMap::new()),
                clock,
            }
        }

        fn ensure_backend(
            &self,
            request: &OutboundRequest,
            budget: DispatchBudget,
        ) -> Result<Backend, EdgeError> {
            let identity = backend_identity(request, budget, &self.clock)?;
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
            let host_override = backend_host_override(request);
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
            finish_backend_creation(builder.finish(), budget, self.clock.now(), |backend| {
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
            })
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
            budget_remaining(budget, &self.clock)?;
            let backend = self.ensure_backend(&request, budget)?;
            budget_remaining(budget, &self.clock)?;
            Ok(PreparedRequest {
                backend,
                budget,
                parts: request.into_parts(),
                started_at,
            })
        }

        async fn execute(&self, prepared: PreparedRequest) -> Result<OutboundResponse, EdgeError> {
            let PreparedRequest {
                backend,
                budget,
                parts,
                started_at,
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
                    dispatch_guard(started_at, budget, &self.clock)?;
                    let pending = buffered_request
                        .send_async(backend)
                        .map_err(|error| map_send_error(&error, budget, &self.clock))?;
                    wait_pending(pending, budget, &self.clock)?
                }
                Body::Stream(source) => {
                    send_streamed(
                        fastly_request,
                        backend,
                        source,
                        max_request_body_bytes,
                        budget,
                        started_at,
                        self.clock.clone(),
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
                self.clock.clone(),
            )
            .await
        }

        fn dispatch_batch_slot(&self, prepared: PreparedRequest) -> Result<PendingSlot, EdgeError> {
            let PreparedRequest {
                backend,
                budget,
                parts,
                started_at,
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
            dispatch_guard(started_at, budget, &self.clock)?;
            let pending = request
                .send_async(backend)
                .map_err(|error| map_send_error(&error, budget, &self.clock))?;
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

    fn backend_host_override(request: &OutboundRequest) -> String {
        request.host_authority()
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for FastlyOutboundClient {
        #[inline]
        async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
            let started_at = self.clock.now();
            let prepared = self.prepare(request, started_at)?;
            self.execute(prepared).await
        }

        #[inline]
        async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
            let batch_started_at = self.clock.now();
            let mut slots: Vec<Slot> = dispatch_all_before_wait(requests, |request| {
                let prepared = self.prepare_batch(request, batch_started_at)?;
                self.dispatch_batch_slot(prepared)
            })
            .into_iter()
            .map(|result| {
                result.map_or_else(
                    |error| Slot::Done(finish_slot(batch_started_at, Err(error), &self.clock)),
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
                    let outcome = finish_pending(*pending, &self.clock).await;
                    Slot::Done(finish_slot(batch_started_at, outcome, &self.clock))
                } else {
                    current
                };

                for later in slots.iter_mut().skip(index.saturating_add(1)) {
                    poll_slot(later, batch_started_at, &self.clock).await;
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
                        &self.clock,
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
        clock: &MonotonicClock,
    ) -> Result<BackendIdentity, EdgeError> {
        let remaining = budget_remaining(budget, clock)?;
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

    async fn finish_pending(
        slot: PendingSlot,
        clock: &MonotonicClock,
    ) -> Result<OutboundResponse, EdgeError> {
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
        let response = wait_pending(pending, budget, clock)?;
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
            clock.clone(),
        )
        .await
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

    fn finish_backend_creation<BackendValue, RetainSuccess>(
        result: Result<BackendValue, BackendCreationError>,
        budget: DispatchBudget,
        observed_at: MonotonicInstant,
        retain_success: RetainSuccess,
    ) -> Result<BackendValue, EdgeError>
    where
        RetainSuccess: FnOnce(BackendValue) -> Result<BackendValue, EdgeError>,
    {
        match result {
            Ok(backend) => {
                let retained = retain_success(backend);
                if budget.deadline.is_expired_at(observed_at) {
                    Err(timeout_error(budget.cause))
                } else {
                    retained
                }
            }
            Err(error) => {
                if budget.deadline.is_expired_at(observed_at) {
                    Err(timeout_error(budget.cause))
                } else {
                    Err(map_backend_creation_error(&error))
                }
            }
        }
    }

    fn cause_to_failure(cause: &SendErrorCause) -> super::SendFailure {
        match cause {
            SendErrorCause::ConnectionTimeout | SendErrorCause::HttpResponseTimeout => {
                super::SendFailure::BudgetedTimeout
            }
            SendErrorCause::DnsTimeout => super::SendFailure::ProviderTimeout,
            SendErrorCause::ConnectionLimitReached
            | SendErrorCause::ConnectionRefused
            | SendErrorCause::DestinationIpUnroutable
            | SendErrorCause::DestinationNotFound
            | SendErrorCause::DestinationUnavailable
            | SendErrorCause::DnsError { .. }
            | SendErrorCause::TlsAlertReceived { .. }
            | SendErrorCause::TlsCertificateError
            | SendErrorCause::TlsConfigurationError
            | SendErrorCause::TlsProtocolError => super::SendFailure::Unreachable,
            SendErrorCause::ConnectionTerminated | SendErrorCause::IoError(_) => {
                super::SendFailure::Transport
            }
            SendErrorCause::Http2StreamError { .. }
            | SendErrorCause::HttpIncompleteResponse
            | SendErrorCause::HttpProtocolError
            | SendErrorCause::HttpResponseBodyTooLarge
            | SendErrorCause::HttpResponseHeaderSectionTooLarge
            | SendErrorCause::HttpResponseStatusInvalid
            | SendErrorCause::HttpUpgradeFailed => super::SendFailure::UpstreamProtocol,
            SendErrorCause::HttpCacheApiUnsupported
            | SendErrorCause::HttpCacheLimitExceeded
            | SendErrorCause::HttpRequestCacheKeyInvalid
            | SendErrorCause::HttpRequestUriInvalid => super::SendFailure::LocalInvariant,
            SendErrorCause::ImageOptimizerUnsupported | SendErrorCause::Custom(_) => {
                super::SendFailure::Unknown
            }
            SendErrorCause::InternalError(_) => super::SendFailure::PlatformInternal,
            _ => super::SendFailure::Unknown,
        }
    }

    fn map_send_error(
        error: &SendError,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> EdgeError {
        super::classify_send_failure(cause_to_failure(error.root_cause()), budget, clock.now())
    }

    async fn poll_slot(slot: &mut Slot, started_at: MonotonicInstant, clock: &MonotonicClock) {
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
                            clock.clone(),
                        )
                        .await
                    }
                    Err(error) => Err(map_send_error(&error, budget, clock)),
                };
                *slot = Slot::Done(finish_slot(started_at, outcome, clock));
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
        clock: MonotonicClock,
    ) -> Result<OutboundResponse, EdgeError> {
        budget_remaining(budget, &clock)?;
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
        let native = fastly_body_stream(response.take_body(), budget, clock);
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
        started_at: MonotonicInstant,
        clock: MonotonicClock,
    ) -> Result<FastlyResponse, EdgeError> {
        dispatch_guard(started_at, budget, &clock)?;
        let (mut writer, pending) = request
            .send_async_streaming(backend)
            .map_err(|error| map_send_error(&error, budget, &clock))?;
        let mut total = 0_u64;
        loop {
            budget_remaining(budget, &clock)?;
            let next_item = source.next().await;
            budget_remaining(budget, &clock)?;
            let Some(item) = next_item else {
                break;
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
            let write_result = writer.write_all(&bytes);
            resolve_stream_io_result(
                write_result,
                budget,
                clock.now(),
                "Fastly outbound request body write failed",
            )?;
            let flush_result = writer.flush();
            resolve_stream_io_result(
                flush_result,
                budget,
                clock.now(),
                "Fastly outbound request body flush failed",
            )?;
            total = next_total;
        }
        let finish_result = writer.finish();
        resolve_stream_io_result(
            finish_result,
            budget,
            clock.now(),
            "Fastly outbound request completion failed",
        )?;
        wait_pending(pending, budget, &clock)
    }

    fn resolve_stream_io_result(
        result: IoResult<()>,
        budget: DispatchBudget,
        observed_at: MonotonicInstant,
        failure_message: &'static str,
    ) -> Result<(), EdgeError> {
        if budget.deadline.is_expired_at(observed_at) {
            return Err(timeout_error(budget.cause));
        }
        result.map_err(|_error| {
            EdgeError::bad_gateway_with_reason(failure_message, BadGatewayReason::Transport)
        })
    }

    fn fastly_body_stream(
        mut body: FastlyBody,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> BodyStream {
        stream! {
            loop {
                budget_remaining(budget, &clock)?;
                let mut chunks = body.read_chunks(RESPONSE_READ_BYTES);
                let item = chunks.next();
                drop(chunks);
                match item {
                    Some(Ok(chunk)) => {
                        budget_remaining(budget, &clock)?;
                        yield Ok(Bytes::from(chunk));
                    }
                    Some(Err(_error)) => {
                        if budget.deadline.is_expired_at(clock.now()) {
                            yield Err(timeout_error(budget.cause));
                        } else {
                            yield Err(EdgeError::bad_gateway_with_reason(
                                "Fastly upstream response body failed",
                                BadGatewayReason::Transport,
                            ));
                        }
                        return;
                    }
                    None => {
                        budget_remaining(budget, &clock)?;
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
        clock: &MonotonicClock,
    ) -> Result<FastlyResponse, EdgeError> {
        budget_remaining(budget, clock)?;
        let outcome = pending
            .wait()
            .map_err(|error| map_send_error(&error, budget, clock));
        budget_remaining(budget, clock)?;
        outcome
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

    fn dispatch_guard(
        started_at: MonotonicInstant,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> Result<(), EdgeError> {
        dispatch_guard_at(started_at, budget, dispatch_observed_at(started_at, clock)?)
    }

    #[cfg(feature = "test-utils")]
    fn dispatch_observed_at(
        started_at: MonotonicInstant,
        clock: &MonotonicClock,
    ) -> Result<MonotonicInstant, EdgeError> {
        if let Some(delay) = DISPATCH_SLACK_INJECTION.get() {
            return started_at.checked_add(delay).ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "Fastly dispatch test clock exceeded the monotonic instant range"
                ))
            });
        }
        Ok(clock.now())
    }

    #[cfg(not(feature = "test-utils"))]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the test-utils build can reject an injected monotonic overflow"
    )]
    fn dispatch_observed_at(
        _started_at: MonotonicInstant,
        clock: &MonotonicClock,
    ) -> Result<MonotonicInstant, EdgeError> {
        Ok(clock.now())
    }

    fn dispatch_guard_at(
        started_at: MonotonicInstant,
        budget: DispatchBudget,
        observed_at: MonotonicInstant,
    ) -> Result<(), EdgeError> {
        if budget.deadline.is_expired_at(observed_at) {
            return Err(timeout_error(budget.cause));
        }
        let elapsed = observed_at
            .checked_duration_since(started_at)
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "monotonic clock moved backwards before Fastly SDK dispatch"
                ))
            })?;
        if elapsed > BATCH_DISPATCH_SLACK_MAX {
            return Err(EdgeError::internal(anyhow::anyhow!(DISPATCH_SLACK_MESSAGE)));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::cell::Cell;
        use std::collections::VecDeque;
        use std::io::Error;
        use std::sync::{Arc, Mutex};

        use edgezero_core::error::BudgetSource;
        use edgezero_core::time::Deadline;
        use futures::executor::block_on;

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

        fn clock_budget(start: MonotonicInstant, duration: Duration) -> DispatchBudget {
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
            let client = FastlyOutboundClient::with_clock(scripted_clock(vec![start, completed]));
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
            let client = FastlyOutboundClient::with_clock(scripted_clock(vec![start, earlier]));
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
            let budget = clock_budget(start, Duration::from_millis(10));
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
        fn backend_preparation_consumes_budget_in_the_injected_clock_domain() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(10));
            let observed = start
                .checked_add(Duration::from_millis(3))
                .expect("observed instant");
            let clock = scripted_clock(vec![observed]);
            let request = OutboundRequest::get("https://example.com/").expect("request");

            let identity = backend_identity(&request, budget, &clock).expect("backend identity");

            assert_eq!(identity.budget_ms, 7);
        }

        #[test]
        fn response_stream_retains_clock_for_post_ready_expiry() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(10));
            let clock = scripted_clock(vec![start, budget.deadline.instant()]);
            let mut body = fastly_body_stream(FastlyBody::from("body"), budget, clock);

            let error = block_on(body.next())
                .expect("terminal item")
                .expect_err("post-ready expiry");

            assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
        }

        #[test]
        fn backend_creation_error_table_is_exhaustive() {
            use fastly_shared::FastlyStatus;

            let mapped = map_backend_creation_error(&BackendCreationError::Disallowed);
            assert_eq!(mapped.status(), StatusCode::BAD_GATEWAY);
            assert!(matches!(
                mapped,
                EdgeError::BadGateway {
                    message,
                    reason: BadGatewayReason::Unspecified,
                } if message == DYNAMIC_BACKENDS_DISABLED_MESSAGE
            ));

            for local_invariant in [
                BackendCreationError::BetweenBytesTimeoutTooLarge(Duration::from_secs(1)),
                BackendCreationError::ConnectTimeoutTooLarge(Duration::from_secs(1)),
                BackendCreationError::EncodingError(
                    String::from_utf8(vec![0xff]).expect_err("invalid UTF-8"),
                ),
                BackendCreationError::FirstByteTimeoutTooLarge(Duration::from_secs(1)),
                BackendCreationError::NameTooLong("x".to_owned()),
                BackendCreationError::NameInUse,
            ] {
                assert!(matches!(
                    map_backend_creation_error(&local_invariant),
                    EdgeError::Internal { .. }
                ));
            }

            assert!(matches!(
                map_backend_creation_error(&BackendCreationError::HostError(FastlyStatus::ERROR)),
                EdgeError::BadGateway {
                    reason: BadGatewayReason::Unspecified,
                    ..
                }
            ));
        }

        #[test]
        fn backend_creation_failure_observed_at_deadline_is_a_timeout() {
            let started_at = MonotonicInstant::now();
            let budget = super::super::test_budget(started_at, BudgetSource::BatchDeadline);
            let retained = Cell::new(false);

            let error = finish_backend_creation::<(), _>(
                Err(BackendCreationError::Disallowed),
                budget,
                budget.deadline.instant(),
                |_backend| {
                    retained.set(true);
                    Ok(())
                },
            )
            .expect_err("deadline must outrank backend failure");

            assert!(!retained.get());
            assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::BatchDeadline,
                    ..
                }
            ));
        }

        #[test]
        fn successful_backend_creation_is_retained_before_late_timeout() {
            let started_at = MonotonicInstant::now();
            let budget = super::super::test_budget(started_at, BudgetSource::BatchDeadline);
            let retained = Cell::new(false);

            let error =
                finish_backend_creation(Ok(7_u8), budget, budget.deadline.instant(), |backend| {
                    retained.set(true);
                    Ok(backend)
                })
                .expect_err("late success must still return the attributed timeout");

            assert!(retained.get());
            assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::BatchDeadline,
                    ..
                }
            ));
        }

        #[test]
        fn streamed_write_failure_observed_at_expiry_is_a_timeout() {
            let started_at = MonotonicInstant::now();
            let budget = super::super::test_budget(started_at, BudgetSource::PerCallTimeout);

            let error = resolve_stream_io_result(
                Err(Error::other("write failed")),
                budget,
                budget.deadline.instant(),
                "Fastly outbound request body write failed",
            )
            .expect_err("deadline must outrank transport failure");

            assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::PerCallTimeout,
                    ..
                }
            ));
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
        fn backend_host_override_preserves_explicit_port() {
            let mut request =
                OutboundRequest::get("https://api.example.com:8443/resource").expect("request");
            normalize_for_dispatch(&mut request).expect("normalize");

            assert_eq!(backend_host_override(&request), "api.example.com:8443");
        }

        #[test]
        fn backend_host_override_preserves_bracketed_ipv6() {
            let mut request = OutboundRequest::get("http://[::1]:8443/resource").expect("request");
            normalize_for_dispatch(&mut request).expect("normalize");

            assert_eq!(backend_host_override(&request), "[::1]:8443");
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

        #[test]
        fn dispatch_guard_enforces_slack_after_deadline_precedence() {
            use edgezero_core::time::{BATCH_DISPATCH_SLACK_MAX, Deadline};

            let started_at = MonotonicInstant::now();
            let duration = Duration::from_secs(1);
            let budget = DispatchBudget {
                cause: BudgetSource::PerCallTimeout,
                deadline: Deadline::at_instant(
                    started_at.checked_add(duration).expect("deadline instant"),
                ),
                duration,
            };
            let at_limit = started_at
                .checked_add(BATCH_DISPATCH_SLACK_MAX)
                .expect("slack boundary");
            dispatch_guard_at(started_at, budget, at_limit).expect("dispatch at slack boundary");

            let over_limit = at_limit
                .checked_add(Duration::from_nanos(1))
                .expect("past slack boundary");
            assert!(matches!(
                dispatch_guard_at(started_at, budget, over_limit),
                Err(EdgeError::Internal { source })
                    if source.to_string() == DISPATCH_SLACK_MESSAGE
            ));

            let expired_budget = DispatchBudget {
                deadline: Deadline::at_instant(over_limit),
                ..budget
            };
            assert!(matches!(
                dispatch_guard_at(started_at, expired_budget, over_limit),
                Err(EdgeError::GatewayTimeout {
                    cause: BudgetSource::PerCallTimeout,
                    ..
                })
            ));
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "the pinned SDK cause table intentionally constructs every known variant"
        )]
        fn send_error_cause_table_is_exhaustive() {
            let cases = [
                (
                    SendErrorCause::DnsTimeout,
                    super::super::SendFailure::ProviderTimeout,
                ),
                (
                    SendErrorCause::DnsError {
                        rcode: None,
                        info_code: None,
                    },
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::DestinationNotFound,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::DestinationUnavailable,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::DestinationIpUnroutable,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::ConnectionRefused,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::ConnectionTerminated,
                    super::super::SendFailure::Transport,
                ),
                (
                    SendErrorCause::ConnectionTimeout,
                    super::super::SendFailure::BudgetedTimeout,
                ),
                (
                    SendErrorCause::ConnectionLimitReached,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::TlsProtocolError,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::TlsCertificateError,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::TlsAlertReceived { alert_id: None },
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::TlsConfigurationError,
                    super::super::SendFailure::Unreachable,
                ),
                (
                    SendErrorCause::HttpIncompleteResponse,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpResponseHeaderSectionTooLarge,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpResponseBodyTooLarge,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpResponseTimeout,
                    super::super::SendFailure::BudgetedTimeout,
                ),
                (
                    SendErrorCause::HttpResponseStatusInvalid,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpUpgradeFailed,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::Http2StreamError {
                        frame_type: 0,
                        error_code: 0,
                    },
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpProtocolError,
                    super::super::SendFailure::UpstreamProtocol,
                ),
                (
                    SendErrorCause::HttpRequestCacheKeyInvalid,
                    super::super::SendFailure::LocalInvariant,
                ),
                (
                    SendErrorCause::HttpRequestUriInvalid,
                    super::super::SendFailure::LocalInvariant,
                ),
                (
                    SendErrorCause::HttpCacheLimitExceeded,
                    super::super::SendFailure::LocalInvariant,
                ),
                (
                    SendErrorCause::HttpCacheApiUnsupported,
                    super::super::SendFailure::LocalInvariant,
                ),
                (
                    SendErrorCause::IoError(Error::other("test")),
                    super::super::SendFailure::Transport,
                ),
                (
                    SendErrorCause::ImageOptimizerUnsupported,
                    super::super::SendFailure::Unknown,
                ),
                (
                    SendErrorCause::InternalError(None),
                    super::super::SendFailure::PlatformInternal,
                ),
                (
                    SendErrorCause::Custom(anyhow::anyhow!("test")),
                    super::super::SendFailure::Unknown,
                ),
            ];

            let observed_at = MonotonicInstant::now();
            let selected = BudgetSource::PerCallTimeout;
            let budget = super::super::test_budget(observed_at, selected);
            for (cause, expected) in cases {
                let failure = cause_to_failure(&cause);
                assert_eq!(failure, expected, "{cause:?}");
                super::super::assert_send_failure_error(
                    failure,
                    &super::super::classify_send_failure(failure, budget, observed_at),
                    selected,
                );
            }
        }
    }
}

#[cfg(all(feature = "fastly", feature = "test-utils"))]
pub use fastly_impl::inject_dispatch_slack_for_test;
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

#[cfg(any(feature = "fastly", test))]
fn timeout_error(cause: BudgetSource) -> EdgeError {
    EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
}

#[cfg(test)]
mod send_failure_policy_tests {
    use super::*;

    fn failures() -> [SendFailure; 8] {
        [
            SendFailure::BudgetedTimeout,
            SendFailure::LocalInvariant,
            SendFailure::PlatformInternal,
            SendFailure::ProviderTimeout,
            SendFailure::Transport,
            SendFailure::Unknown,
            SendFailure::Unreachable,
            SendFailure::UpstreamProtocol,
        ]
    }

    #[test]
    fn send_failure_policy_maps_every_known_category() {
        let observed_at = MonotonicInstant::now();
        for selected in [
            BudgetSource::PerCallTimeout,
            BudgetSource::BatchDeadline,
            BudgetSource::Default,
        ] {
            let budget = test_budget(observed_at, selected);
            for failure in failures() {
                assert_send_failure_error(
                    failure,
                    &classify_send_failure(failure, budget, observed_at),
                    selected,
                );
            }
        }
    }

    #[test]
    fn absolute_deadline_wins_every_send_failure() {
        let started_at = MonotonicInstant::now();
        for selected in [
            BudgetSource::PerCallTimeout,
            BudgetSource::BatchDeadline,
            BudgetSource::Default,
        ] {
            let budget = test_budget(started_at, selected);
            let after_deadline = budget
                .deadline
                .instant()
                .checked_add(Duration::from_nanos(1))
                .expect("after deadline");
            for observed_at in [budget.deadline.instant(), after_deadline] {
                for failure in failures() {
                    let error = classify_send_failure(failure, budget, observed_at);
                    if let EdgeError::GatewayTimeout { cause, .. } = error {
                        assert_eq!(cause, selected);
                    } else {
                        panic!("deadline did not win {failure:?} for {selected:?}");
                    }
                }
            }
        }
    }
}
