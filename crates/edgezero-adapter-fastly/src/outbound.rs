#![cfg_attr(
    feature = "fastly",
    expect(
        clippy::arbitrary_source_item_ordering,
        reason = "the target-gated implementation module is kept before shared test seams"
    )
)]
#![cfg_attr(
    feature = "fastly",
    expect(
        clippy::pub_use,
        reason = "the target-gated implementation keeps Fastly imports out of native builds"
    )
)]

#[cfg(test)]
use std::time::Duration;

use edgezero_core::error::EdgeError;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::error::{BadGatewayReason, BudgetSource};
#[cfg(any(feature = "fastly", test))]
use edgezero_core::outbound::OutboundBatchDriverEvent;
#[cfg(any(feature = "fastly", feature = "test-utils"))]
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
    use std::io::{Result as IoResult, Write};
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
        OutboundBatch, OutboundBatchDriverEvent, OutboundCachePolicy, OutboundHttpClient,
        OutboundRequest, OutboundRequestParts, OutboundResponse, ResponseBodyDisposition,
        ResponseHeaderLimiter, ResponseMode, collect_response_stream,
        enforce_payload_content_length, finish_batch_item, insert_proxy_header,
        limit_decoded_stream, limit_encoded_stream, normalize_for_dispatch,
        normalize_response_headers, rechunk_stream, validate_for_dispatch,
    };
    use edgezero_core::time::{
        BATCH_DISPATCH_SLACK_MAX, Deadline, DispatchBudget, MonotonicClock, MonotonicInstant,
        dispatch_budget,
    };
    use fastly::backend::BackendCreationError;
    use fastly::handle::{BodyHandle, PendingRequestHandle, ResponseHandle, select_handles};
    use fastly::http::body::StreamingBody;
    use fastly::http::request::{PendingRequest, SendError, SendErrorCause};
    use fastly::{
        Backend, Body as FastlyBody, Request as FastlyRequest, Response as FastlyResponse,
    };
    use futures_util::StreamExt as _;
    use sha2::{Digest as _, Sha256};

    use super::{
        driver_selection_failure, reassociate_selection, timeout_error, validate_batch_request,
    };

    pub const DYNAMIC_BACKENDS_DISABLED_MESSAGE: &str = "Fastly dynamic backends are not enabled on this service; enable them in the service configuration";
    const RESPONSE_READ_BYTES: usize = 16 * 1024;
    const DISPATCH_SLACK_MESSAGE: &str = "Fastly batch adapter overhead between batch_now and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration";

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
        wire_authority: String,
    }

    struct PreparedRequest {
        backend: Backend,
        budget: DispatchBudget,
        parts: OutboundRequestParts,
        started_at: MonotonicInstant,
    }

    struct PendingSlot {
        metadata: PendingSlotMetadata,
        pending: PendingRequestHandle,
    }

    struct PendingSlotMetadata {
        index: usize,
        budget: DispatchBudget,
        max_brotli_decoder_bytes: u64,
        max_brotli_window_bits: u8,
        max_chunk_bytes: Option<NonZeroU64>,
        max_decoded_response_bytes: Option<u64>,
        max_encoded_response_bytes: Option<u64>,
        max_response_header_bytes: Option<u64>,
        max_response_header_count: Option<u64>,
        request_method: Method,
        response_mode: ResponseMode,
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
            batch_started_at: Option<MonotonicInstant>,
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
            let creation =
                register_backend_with_batch_guard(batch_started_at, budget, &self.clock, || {
                    builder.finish()
                })?;
            finish_backend_creation(creation, budget, &self.clock, |backend| {
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
            self.prepare_validated(request, started_at, None, false)
        }

        fn prepare_batch(
            &self,
            request: OutboundRequest,
            started_at: MonotonicInstant,
            cutoff: Deadline,
        ) -> Result<PreparedRequest, EdgeError> {
            validate_batch_request(&request)?;
            self.prepare_validated(request, started_at, Some(cutoff), true)
        }

        fn prepare_validated(
            &self,
            mut request: OutboundRequest,
            started_at: MonotonicInstant,
            batch_cutoff: Option<Deadline>,
            enforce_batch_slack: bool,
        ) -> Result<PreparedRequest, EdgeError> {
            let budget = dispatch_budget(&request, started_at, batch_cutoff)?;
            normalize_fastly_request(&mut request)?;
            budget_remaining(budget, &self.clock)?;
            let backend =
                self.ensure_backend(&request, budget, enforce_batch_slack.then_some(started_at))?;
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
                cache_policy,
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
            let fastly_request = build_fastly_request(&method, &uri, &headers, cache_policy);
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

        fn dispatch_batch_slot(
            &self,
            index: usize,
            prepared: PreparedRequest,
        ) -> Result<PendingSlot, EdgeError> {
            let PreparedRequest {
                backend,
                budget,
                parts,
                started_at,
            } = prepared;
            let OutboundRequestParts {
                body,
                cache_policy,
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
            let mut request = build_fastly_request(&method, &uri, &headers, cache_policy);
            request.set_body(bytes.to_vec());
            dispatch_guard(started_at, budget, &self.clock)?;
            let (request_handle, optional_body_handle) = request.into_handles();
            let body_handle = optional_body_handle.ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "Fastly buffered batch request did not produce a body handle"
                ))
            })?;
            let pending = request_handle
                .send_async_to_backend(body_handle, &backend)
                .map_err(|cause| map_send_cause(&cause, budget, &self.clock))?;
            Ok(PendingSlot {
                metadata: PendingSlotMetadata {
                    index,
                    budget,
                    max_brotli_decoder_bytes,
                    max_brotli_window_bits,
                    max_chunk_bytes,
                    max_decoded_response_bytes,
                    max_encoded_response_bytes,
                    max_response_header_bytes,
                    max_response_header_count,
                    request_method: method,
                    response_mode,
                },
                pending,
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
        request.host_authority().to_owned()
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
        fn start_batch_until(
            &self,
            requests: Vec<OutboundRequest>,
            cutoff: Deadline,
        ) -> OutboundBatch {
            let batch_started_at = self.clock.now();
            let slot_count = requests.len();
            if cutoff.is_expired_at(batch_started_at) {
                return OutboundBatch::cutoff(slot_count);
            }

            let mut completed = Vec::new();
            let mut pending = Vec::new();
            let mut cutoff_reached = false;
            for (index, request) in requests.into_iter().enumerate() {
                let prepared = self
                    .prepare_batch(request, batch_started_at, cutoff)
                    .and_then(|prepared| self.dispatch_batch_slot(index, prepared));
                match prepared {
                    Ok(slot) => pending.push(slot),
                    Err(error) => {
                        let completed_at = self.clock.now();
                        let Some(item) = finish_batch_item(
                            index,
                            batch_started_at,
                            completed_at,
                            cutoff,
                            Err(error),
                        ) else {
                            cutoff_reached = true;
                            break;
                        };
                        completed.push(item);
                    }
                }
            }

            let clock = self.clock.clone();
            let completions = stream! {
                for item in completed {
                    yield OutboundBatchDriverEvent::Item(item);
                }

                if cutoff_reached {
                    yield OutboundBatchDriverEvent::Cutoff;
                    return;
                }

                while !pending.is_empty() && !cutoff.is_expired_at(clock.now()) {
                    let (selection, metadata) = match select_pending_slot(&mut pending) {
                        Ok(selected) => selected,
                        Err(error) => {
                            yield driver_selection_failure(error);
                            return;
                        }
                    };
                    let index = metadata.index;
                    let outcome = finish_selected(selection, metadata, &clock).await;
                    let completed_at = clock.now();
                    let Some(item) = finish_batch_item(
                        index,
                        batch_started_at,
                        completed_at,
                        cutoff,
                        outcome,
                    ) else {
                        yield OutboundBatchDriverEvent::Cutoff;
                        return;
                    };
                    yield OutboundBatchDriverEvent::Item(item);
                }

                if !pending.is_empty() {
                    yield OutboundBatchDriverEvent::Cutoff;
                }
            };
            OutboundBatch::from_driver(slot_count, completions)
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
        let budget_ms = ceil_millis(budget.duration);
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
            wire_authority: request.host_authority().to_owned(),
        })
    }

    fn backend_name(identity: &BackendIdentity) -> String {
        let tls_mode = if identity.tls { "tls" } else { "plain" };
        let canonical = format!(
            "{}:{}:{}:{}:{}:{}",
            identity.scheme,
            identity.host,
            identity.port,
            tls_mode,
            identity.budget_ms,
            identity.wire_authority
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

    fn build_fastly_request(
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        cache_policy: OutboundCachePolicy,
    ) -> FastlyRequest {
        let mut request = FastlyRequest::new(method.clone(), uri.to_string());
        request.set_method(method.clone());
        for (name, value) in headers {
            request.append_header(name.as_str(), value.as_bytes());
        }
        if cache_policy == OutboundCachePolicy::Bypass {
            request.set_pass(true);
        }
        request
    }

    fn normalize_fastly_request(request: &mut OutboundRequest) -> Result<(), EdgeError> {
        normalize_for_dispatch(request)?;
        if !request.headers().contains_key(ACCEPT_ENCODING) {
            request
                .headers_mut()
                .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
        Ok(())
    }

    fn ceil_millis(duration: Duration) -> u64 {
        let rounded = duration.as_nanos().div_ceil(1_000_000);
        u64::try_from(rounded).unwrap_or(u64::MAX).max(1)
    }

    fn register_backend_with_batch_guard<BackendValue, CreationError, Register>(
        batch_started_at: Option<MonotonicInstant>,
        budget: DispatchBudget,
        clock: &MonotonicClock,
        register: Register,
    ) -> Result<Result<BackendValue, CreationError>, EdgeError>
    where
        Register: FnOnce() -> Result<BackendValue, CreationError>,
    {
        if let Some(started_at) = batch_started_at {
            dispatch_guard(started_at, budget, clock)?;
        }
        Ok(register())
    }

    type SelectedResponse = Result<(ResponseHandle, BodyHandle), SendErrorCause>;

    fn select_pending_slot(
        slots: &mut Vec<PendingSlot>,
    ) -> Result<(SelectedResponse, PendingSlotMetadata), EdgeError> {
        let maximum = usize::try_from(fastly_shared::MAX_PENDING_REQS).unwrap_or(usize::MAX);
        let selection_count = slots.len().min(maximum);
        if selection_count == 0 {
            return Err(EdgeError::internal(anyhow::anyhow!(
                "Fastly batch selection requires a pending slot"
            )));
        }

        let selected_group = slots.drain(..selection_count).collect::<Vec<_>>();
        let mut handles = Vec::with_capacity(selection_count);
        let mut records = Vec::with_capacity(selection_count);
        for PendingSlot {
            metadata,
            pending: handle,
        } in selected_group
        {
            records.push((pending_handle_id(&handle), Some(metadata)));
            handles.push(handle);
        }

        let (selection, selected_position, remaining_handles) = select_handles(handles);
        let remaining_ids = remaining_handles
            .iter()
            .map(pending_handle_id)
            .collect::<Vec<_>>();
        let (selected_metadata, remaining_metadata) =
            reassociate_selection(records, selected_position, &remaining_ids)?;
        let mut restored = remaining_handles
            .into_iter()
            .zip(remaining_metadata)
            .map(|(handle, metadata)| PendingSlot {
                metadata,
                pending: handle,
            })
            .collect::<Vec<_>>();
        restored.append(slots);
        *slots = restored;
        Ok((selection, selected_metadata))
    }

    #[cfg_attr(
        not(target_env = "p1"),
        expect(
            deprecated,
            reason = "Fastly exposes only the raw handle identity needed to restore select order"
        )
    )]
    fn pending_handle_id(handle: &PendingRequestHandle) -> u32 {
        handle.as_u32()
    }

    async fn finish_selected(
        selection: SelectedResponse,
        metadata: PendingSlotMetadata,
        clock: &MonotonicClock,
    ) -> Result<OutboundResponse, EdgeError> {
        let PendingSlotMetadata {
            budget,
            max_brotli_decoder_bytes,
            max_brotli_window_bits,
            max_chunk_bytes,
            max_decoded_response_bytes,
            max_encoded_response_bytes,
            max_response_header_bytes,
            max_response_header_count,
            request_method,
            response_mode,
            ..
        } = metadata;
        let response = match selection {
            Ok((response, body)) => FastlyResponse::from_handles(response, body),
            Err(cause) => return Err(map_send_cause(&cause, budget, clock)),
        };
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
            | BackendCreationError::InvalidHealthcheckValue(_)
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
        clock: &MonotonicClock,
        retain_success: RetainSuccess,
    ) -> Result<BackendValue, EdgeError>
    where
        RetainSuccess: FnOnce(BackendValue) -> Result<BackendValue, EdgeError>,
    {
        match result {
            Ok(backend) => {
                let retained = retain_success(backend);
                if budget.deadline.is_expired_at(clock.now()) {
                    Err(timeout_error(budget.cause))
                } else {
                    retained
                }
            }
            Err(error) => {
                if budget.deadline.is_expired_at(clock.now()) {
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
            SendErrorCause::InternalError(_) => super::SendFailure::PlatformInternal,
            SendErrorCause::FanoutNotEnabled
            | SendErrorCause::ImageOptimizerUnsupported
            | SendErrorCause::RequestCollapse
            | SendErrorCause::Custom(_)
            | _ => super::SendFailure::Unknown,
        }
    }

    fn map_send_error(
        error: &SendError,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> EdgeError {
        map_send_cause(error.root_cause(), budget, clock)
    }

    fn map_send_cause(
        cause: &SendErrorCause,
        budget: DispatchBudget,
        clock: &MonotonicClock,
    ) -> EdgeError {
        super::classify_send_failure(cause_to_failure(cause), budget, clock.now())
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
        let response_clock = clock.clone();
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
        insert_proxy_header(
            &mut headers,
            &mut header_limiter,
            HeaderValue::from_static("fastly"),
        )?;
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
        let native = fastly_body_stream(response.take_body(), budget, clock.clone());
        if matches!(disposition, ResponseBodyDisposition::ResetContent { .. }) {
            if !declared_reset_body {
                let mut reset_stream = native;
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

    fn response_headers(response: &FastlyResponse) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for name in response.get_header_names() {
            for value in response.get_header_all(name) {
                headers.append(name.clone(), value.clone());
            }
        }
        headers
    }

    trait StreamedUploadWriter {
        fn finish(self) -> IoResult<()>;
        fn flush(&mut self) -> IoResult<()>;
        fn write_all(&mut self, bytes: &[u8]) -> IoResult<()>;
    }

    impl StreamedUploadWriter for StreamingBody {
        fn finish(self) -> IoResult<()> {
            StreamingBody::finish(self)
        }

        fn flush(&mut self) -> IoResult<()> {
            Write::flush(self)
        }

        fn write_all(&mut self, bytes: &[u8]) -> IoResult<()> {
            Write::write_all(self, bytes)
        }
    }

    async fn complete_streamed_exchange<Writer, Wait, Output>(
        mut writer: Writer,
        mut source: BodyStream,
        maximum: u64,
        budget: DispatchBudget,
        clock: &MonotonicClock,
        wait: Wait,
    ) -> Result<Output, EdgeError>
    where
        Writer: StreamedUploadWriter,
        Wait: FnOnce() -> Result<Output, EdgeError>,
    {
        let mut total = 0_u64;
        loop {
            budget_remaining(budget, clock)?;
            let next_item = source.next().await;
            budget_remaining(budget, clock)?;
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
            resolve_stream_io_result(
                writer.write_all(&bytes),
                budget,
                clock.now(),
                "Fastly outbound request body write failed",
            )?;
            resolve_stream_io_result(
                writer.flush(),
                budget,
                clock.now(),
                "Fastly outbound request body flush failed",
            )?;
            total = next_total;
        }
        resolve_stream_io_result(
            writer.finish(),
            budget,
            clock.now(),
            "Fastly outbound request completion failed",
        )?;
        wait()
    }

    async fn send_streamed(
        request: FastlyRequest,
        backend: Backend,
        source: BodyStream,
        maximum: u64,
        budget: DispatchBudget,
        started_at: MonotonicInstant,
        clock: MonotonicClock,
    ) -> Result<FastlyResponse, EdgeError> {
        dispatch_guard(started_at, budget, &clock)?;
        let (writer, pending) = request
            .send_async_streaming(backend)
            .map_err(|error| map_send_error(&error, budget, &clock))?;
        complete_streamed_exchange(writer, source, maximum, budget, &clock, || {
            wait_pending(pending, budget, &clock)
        })
        .await
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

    fn deadline_stream(
        mut source: BodyStream,
        budget: DispatchBudget,
        clock: MonotonicClock,
    ) -> BodyStream {
        stream! {
            loop {
                if let Err(error) = budget_remaining(budget, &clock) {
                    yield Err(error);
                    return;
                }
                let next_item = source.next().await;
                if let Err(error) = budget_remaining(budget, &clock) {
                    yield Err(error);
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
        use std::io::{Error, Write as _};
        use std::mem::discriminant;
        use std::num::NonZeroU64;
        use std::sync::{Arc, Mutex};
        use std::thread;

        use edgezero_core::error::{BudgetSource, ResponseLimitReason};
        use edgezero_core::http::header::CONNECTION;
        use edgezero_core::time::Deadline;
        use flate2::{Compression, write::GzEncoder};
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

        fn constant_clock(now: MonotonicInstant) -> MonotonicClock {
            MonotonicClock::new(move || now)
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

            let results = block_on(
                client
                    .start_batch_until(
                        vec![request],
                        Deadline::at_instant(
                            start
                                .checked_add(Duration::from_secs(1))
                                .expect("batch cutoff"),
                        ),
                    )
                    .collect(),
            )
            .expect("valid batch driver");
            let slot = results.slots[0].as_ref().expect("resolved slot");

            assert_eq!(slot.elapsed, Duration::from_millis(9));
            assert!(matches!(slot.outcome, Err(EdgeError::BadRequest { .. })));
        }

        #[test]
        fn batch_reports_per_slot_elapsed() {
            let start = MonotonicInstant::now();
            let first = start
                .checked_add(Duration::from_millis(3))
                .expect("first completion");
            let second = start
                .checked_add(Duration::from_millis(8))
                .expect("second completion");
            let client =
                FastlyOutboundClient::with_clock(scripted_clock(vec![start, first, second]));
            let requests = vec![
                OutboundRequest::get("https://example.com/")
                    .expect("request")
                    .body("invalid GET body"),
                OutboundRequest::get("https://example.com/")
                    .expect("request")
                    .stream_response(),
            ];

            let results = block_on(
                client
                    .start_batch_until(
                        requests,
                        Deadline::at_instant(
                            start
                                .checked_add(Duration::from_secs(1))
                                .expect("batch cutoff"),
                        ),
                    )
                    .collect(),
            )
            .expect("valid batch driver");
            let first_slot = results.slots[0].as_ref().expect("first resolved slot");
            let second_slot = results.slots[1].as_ref().expect("second resolved slot");

            assert_eq!(first_slot.elapsed, Duration::from_millis(3));
            assert_eq!(second_slot.elapsed, Duration::from_millis(8));
            assert!(
                results
                    .slots
                    .iter()
                    .flatten()
                    .all(|slot| matches!(slot.outcome, Err(EdgeError::BadRequest { .. })))
            );
        }

        #[test]
        fn one_slot_batch_matches_send() {
            let now = MonotonicInstant::now();
            let invalid = || {
                OutboundRequest::get("https://example.com/")
                    .expect("request")
                    .body("invalid GET body")
            };
            let single_outcome =
                block_on(FastlyOutboundClient::with_clock(constant_clock(now)).send(invalid()));
            let batch = block_on(
                FastlyOutboundClient::with_clock(constant_clock(now))
                    .start_batch_until(
                        vec![invalid()],
                        Deadline::at_instant(
                            now.checked_add(Duration::from_secs(1))
                                .expect("batch cutoff"),
                        ),
                    )
                    .collect(),
            )
            .expect("valid batch driver");
            let batched_outcome = batch
                .slots
                .into_iter()
                .next()
                .flatten()
                .expect("slot")
                .outcome;

            let (Err(single_error), Err(batched_error)) = (single_outcome, batched_outcome) else {
                panic!("single and batch paths must return errors");
            };
            assert_eq!(discriminant(&single_error), discriminant(&batched_error));
            assert_eq!(single_error.status(), batched_error.status());
            assert_eq!(single_error.message(), batched_error.message());
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

            let results = block_on(
                client
                    .start_batch_until(
                        vec![request],
                        Deadline::at_instant(
                            start
                                .checked_add(Duration::from_secs(1))
                                .expect("batch cutoff"),
                        ),
                    )
                    .collect(),
            )
            .expect("valid batch driver");
            let slot = results.slots[0].as_ref().expect("resolved slot");

            assert_eq!(slot.elapsed, Duration::ZERO);
            assert!(matches!(slot.outcome, Err(EdgeError::Internal { .. })));
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
        fn backend_identity_uses_the_method_entry_budget_snapshot() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(10));
            let request = OutboundRequest::get("https://example.com/").expect("request");

            let identity = backend_identity(&request, budget).expect("backend identity");

            assert_eq!(identity.budget_ms, 10);
        }

        #[test]
        fn adapter_final_dispatch_reapplies_request_normalization() {
            let mut request = OutboundRequest::get("https://example.com/").expect("request");
            request
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("accept-encoding"));
            request
                .headers_mut()
                .insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

            normalize_fastly_request(&mut request).expect("normalized request");

            assert!(!request.headers().contains_key(CONNECTION));
            assert_eq!(
                request.headers().get(ACCEPT_ENCODING),
                Some(&HeaderValue::from_static("identity"))
            );
        }

        #[test]
        fn canonical_uri_wire_serialization_table() {
            for raw in [
                "https://example.com",
                "https://example.com/a/../b?x=%2F",
                "http://127.0.0.1:8080/path?empty=",
                "https://[::1]:8443/",
                "https://xn--bcher-kva.example/catalog",
            ] {
                let request = OutboundRequest::get(raw).expect("canonical request");
                let native = build_fastly_request(
                    request.method(),
                    request.uri(),
                    request.headers(),
                    OutboundCachePolicy::PlatformDefault,
                );

                assert_eq!(native.get_url_str(), request.uri().to_string(), "{raw}");
            }
        }

        #[test]
        fn backend_builder_keeps_pooling_for_identical_identity_and_settings() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(100));
            let first = OutboundRequest::get("https://example.com/path").expect("first request");
            let second = OutboundRequest::get("https://example.com/other").expect("second request");
            let first_identity = backend_identity(&first, budget).expect("first identity");
            let second_identity = backend_identity(&second, budget).expect("second identity");

            assert_eq!(first_identity, second_identity);
            assert_eq!(
                backend_name(&first_identity),
                backend_name(&second_identity)
            );
            let first_timers = backend_timers(first_identity.budget_ms);
            let second_timers = backend_timers(second_identity.budget_ms);
            assert_eq!(first_timers.connect, second_timers.connect);
            assert_eq!(first_timers.first_byte, second_timers.first_byte);
            assert_eq!(first_timers.between_bytes, second_timers.between_bytes);
        }

        #[test]
        fn authority_override_has_a_distinct_backend_identity() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(100));
            let original = OutboundRequest::get("https://origin.example/path").expect("request");
            let overridden = OutboundRequest::get("https://origin.example/path")
                .expect("request")
                .host_authority_override("tenant.example:8443")
                .expect("authority override");

            let original_identity = backend_identity(&original, budget).expect("identity");
            let overridden_identity = backend_identity(&overridden, budget).expect("identity");

            assert_eq!(overridden_identity.host, "origin.example");
            assert_eq!(overridden_identity.port, 443);
            assert_ne!(original_identity, overridden_identity);
            assert_ne!(
                backend_name(&original_identity),
                backend_name(&overridden_identity)
            );
        }

        #[test]
        fn ceil_millis_rounds_up_and_saturates_without_wrapping() {
            assert_eq!(ceil_millis(Duration::ZERO), 1);
            assert_eq!(ceil_millis(Duration::from_nanos(1)), 1);
            assert_eq!(ceil_millis(Duration::from_micros(999)), 1);
            assert_eq!(ceil_millis(Duration::from_millis(1)), 1);
            assert_eq!(ceil_millis(Duration::from_nanos(1_000_001)), 2);
            assert_eq!(ceil_millis(Duration::from_micros(1_001)), 2);
            assert_eq!(ceil_millis(Duration::MAX), u64::MAX);
        }

        #[test]
        fn heterogeneous_hosts_and_exact_budgets_have_distinct_backend_identities() {
            let started_at = MonotonicInstant::now();
            let first_budget = clock_budget(started_at, Duration::from_millis(1));
            let second_budget = clock_budget(started_at, Duration::from_nanos(1_000_001));
            let first_request =
                OutboundRequest::get("https://first.example/path").expect("first request");
            let second_request =
                OutboundRequest::get("https://second.example/path").expect("second request");

            let first = backend_identity(&first_request, first_budget).expect("first identity");
            let second = backend_identity(&second_request, second_budget).expect("second identity");

            assert_eq!(first.budget_ms, 1);
            assert_eq!(second.budget_ms, 2);
            assert_ne!(first, second);
            assert_ne!(backend_name(&first), backend_name(&second));
        }

        #[test]
        fn constant_clock_can_be_sampled_without_exhaustion() {
            let now = MonotonicInstant::now();
            let clock = constant_clock(now);

            assert_eq!(clock.now(), now);
            assert_eq!(clock.now(), now);
            assert_eq!(clock.now(), now);
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
        fn response_read_checks_deadline_after_eof() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_millis(10));
            let clock = scripted_clock(vec![start, budget.deadline.instant()]);
            let mut body = fastly_body_stream(FastlyBody::new(), budget, clock);

            assert!(matches!(
                block_on(body.next()),
                Some(Err(EdgeError::GatewayTimeout {
                    cause: BudgetSource::PerCallTimeout,
                    ..
                }))
            ));
        }

        #[test]
        fn decoder_stalls_timeout_at_all_completion_boundaries() {
            let started_at = MonotonicInstant::now();
            let budget = clock_budget(started_at, Duration::from_millis(100));
            let mut gzip_encoder = GzEncoder::new(Vec::new(), Compression::default());
            gzip_encoder.write_all(b"ab").expect("gzip input");
            let gzip_bytes = gzip_encoder.finish().expect("gzip output");
            let mut native = FastlyResponse::from_status(StatusCode::OK.as_u16());
            native.set_header(CONTENT_ENCODING, "gzip");
            native.set_body(gzip_bytes);

            let response = block_on(process_response(
                native,
                Method::GET,
                ResponseMode::Streamed,
                budget,
                32 * 1024 * 1024,
                24,
                NonZeroU64::new(1),
                None,
                None,
                None,
                None,
                MonotonicClock::default(),
            ))
            .expect("response head");
            let mut body = response.into_body().into_stream().expect("streamed body");

            assert_eq!(
                block_on(body.next())
                    .expect("first item")
                    .expect("first byte"),
                Bytes::from_static(b"a")
            );
            thread::sleep(Duration::from_millis(125));
            assert!(matches!(
                block_on(body.next()),
                Some(Err(EdgeError::GatewayTimeout {
                    cause: BudgetSource::PerCallTimeout,
                    ..
                }))
            ));
        }

        #[test]
        fn response_content_length_rejects_before_body_poll() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_secs(1));
            let mut native = FastlyResponse::from_status(StatusCode::OK.as_u16());
            native.set_header(CONTENT_LENGTH, "2");
            native.set_body("xx");

            let error = block_on(process_response(
                native,
                Method::GET,
                ResponseMode::Buffered { max_bytes: 1 },
                budget,
                32 * 1024 * 1024,
                24,
                None,
                None,
                None,
                None,
                None,
                constant_clock(start),
            ))
            .expect_err("content length exceeds final buffer cap");

            assert!(matches!(
                error,
                EdgeError::ResponseTooLarge {
                    reason: ResponseLimitReason::BufferedBody,
                    ..
                }
            ));
        }

        #[test]
        fn response_content_length_explicit_identity_rejects_before_body_poll() {
            let start = MonotonicInstant::now();
            let budget = clock_budget(start, Duration::from_secs(1));
            let mut native = FastlyResponse::from_status(StatusCode::OK.as_u16());
            native.set_header(CONTENT_ENCODING, "identity");
            native.set_header(CONTENT_LENGTH, "2");
            native.set_body("xx");

            let error = block_on(process_response(
                native,
                Method::GET,
                ResponseMode::Buffered { max_bytes: 16 },
                budget,
                32 * 1024 * 1024,
                24,
                None,
                Some(1),
                None,
                None,
                None,
                constant_clock(start),
            ))
            .expect_err("content length exceeds decoded cap");

            assert!(matches!(
                error,
                EdgeError::ResponseTooLarge {
                    reason: ResponseLimitReason::DecodedBody,
                    ..
                }
            ));
        }

        #[derive(Default)]
        struct UploadEvents {
            finishes: usize,
            flushes: usize,
            waits: usize,
            writes: Vec<Bytes>,
        }

        struct TestUploadWriter {
            events: Arc<Mutex<UploadEvents>>,
        }

        impl StreamedUploadWriter for TestUploadWriter {
            fn finish(self) -> IoResult<()> {
                let mut events = self
                    .events
                    .lock()
                    .map_err(|_poisoned| Error::other("upload events lock poisoned"))?;
                events.finishes = events.finishes.saturating_add(1);
                Ok(())
            }

            fn flush(&mut self) -> IoResult<()> {
                let mut events = self
                    .events
                    .lock()
                    .map_err(|_poisoned| Error::other("upload events lock poisoned"))?;
                events.flushes = events.flushes.saturating_add(1);
                Ok(())
            }

            fn write_all(&mut self, bytes: &[u8]) -> IoResult<()> {
                self.events
                    .lock()
                    .map_err(|_poisoned| Error::other("upload events lock poisoned"))?
                    .writes
                    .push(Bytes::copy_from_slice(bytes));
                Ok(())
            }
        }

        #[test]
        fn streamed_upload_finishes_exactly_once() {
            let started_at = MonotonicInstant::now();
            let budget = clock_budget(started_at, Duration::from_secs(1));
            let clock = constant_clock(started_at);
            let events = Arc::new(Mutex::new(UploadEvents::default()));
            let writer = TestUploadWriter {
                events: Arc::clone(&events),
            };
            let source = stream::iter([
                Ok(Bytes::from_static(b"first")),
                Ok(Bytes::from_static(b"second")),
            ])
            .boxed_local();
            let wait_events = Arc::clone(&events);

            let outcome = block_on(complete_streamed_exchange(
                writer,
                source,
                32,
                budget,
                &clock,
                move || {
                    wait_events.lock().expect("upload events").waits += 1;
                    Ok(7_u8)
                },
            ));
            let observed = events.lock().expect("upload events");

            assert_eq!(outcome.expect("exchange"), 7);
            assert_eq!(
                observed.writes,
                [Bytes::from_static(b"first"), Bytes::from_static(b"second")]
            );
            assert_eq!(observed.flushes, 2);
            assert_eq!(observed.finishes, 1);
            assert_eq!(observed.waits, 1);
        }

        #[test]
        fn streamed_upload_failure_never_waits() {
            let started_at = MonotonicInstant::now();
            let budget = clock_budget(started_at, Duration::from_secs(1));
            let clock = constant_clock(started_at);
            let events = Arc::new(Mutex::new(UploadEvents::default()));
            let writer = TestUploadWriter {
                events: Arc::clone(&events),
            };
            let source = stream::iter([Err(EdgeError::bad_gateway_with_reason(
                "source failed",
                BadGatewayReason::Transport,
            ))])
            .boxed_local();
            let wait_events = Arc::clone(&events);

            let outcome = block_on(complete_streamed_exchange(
                writer,
                source,
                32,
                budget,
                &clock,
                move || {
                    wait_events.lock().expect("upload events").waits += 1;
                    Ok(7_u8)
                },
            ));
            let observed = events.lock().expect("upload events");

            assert!(matches!(
                outcome,
                Err(EdgeError::BadGateway {
                    reason: BadGatewayReason::Transport,
                    ..
                })
            ));
            assert!(observed.writes.is_empty());
            assert_eq!(observed.finishes, 0);
            assert_eq!(observed.waits, 0);
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
                BackendCreationError::InvalidHealthcheckValue("invalid".to_owned()),
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
            let budget = super::super::test_budget(started_at, BudgetSource::BatchCutoff);
            let retained = Cell::new(false);
            let clock = scripted_clock(vec![budget.deadline.instant()]);

            let error = finish_backend_creation::<(), _>(
                Err(BackendCreationError::Disallowed),
                budget,
                &clock,
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
                    cause: BudgetSource::BatchCutoff,
                    ..
                }
            ));
        }

        #[test]
        fn successful_backend_creation_is_retained_before_late_timeout() {
            let started_at = MonotonicInstant::now();
            let budget = super::super::test_budget(started_at, BudgetSource::BatchCutoff);
            let retained = Cell::new(false);
            let observed_at = Arc::new(Mutex::new(started_at));
            let clock_observation = Arc::clone(&observed_at);
            let clock =
                MonotonicClock::new(move || *clock_observation.lock().expect("clock observation"));

            let error = finish_backend_creation(Ok(7_u8), budget, &clock, |backend| {
                retained.set(true);
                *observed_at.lock().expect("clock observation") = budget.deadline.instant();
                Ok(backend)
            })
            .expect_err("late success must still return the attributed timeout");

            assert!(retained.get());
            assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::BatchCutoff,
                    ..
                }
            ));
        }

        #[test]
        fn late_timeout_outranks_retention_error_after_success_is_retained() {
            let started_at = MonotonicInstant::now();
            let budget = super::super::test_budget(started_at, BudgetSource::BatchCutoff);
            let retained = Cell::new(false);
            let observed_at = Arc::new(Mutex::new(started_at));
            let clock_observation = Arc::clone(&observed_at);
            let clock =
                MonotonicClock::new(move || *clock_observation.lock().expect("clock observation"));

            let error = finish_backend_creation(Ok(7_u8), budget, &clock, |_backend| {
                retained.set(true);
                *observed_at.lock().expect("clock observation") = budget.deadline.instant();
                Err(EdgeError::internal(anyhow::anyhow!("retention failed")))
            })
            .expect_err("late timeout must outrank retention failure");

            assert!(retained.get());
            assert!(matches!(
                error,
                EdgeError::GatewayTimeout {
                    cause: BudgetSource::BatchCutoff,
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
                wire_authority: "example.com".to_owned(),
            };
            assert_eq!(backend_name(&first), "ez_1e145c6e3d9e9dd28342de004f8b6954");
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
        fn dispatch_guard_checks_expiry_before_slack() {
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
        fn cold_backend_registration_is_refused_before_more_synchronous_work() {
            use edgezero_core::time::{BATCH_DISPATCH_SLACK_MAX, Deadline};

            let started_at = MonotonicInstant::now();
            let over_limit = started_at
                .checked_add(BATCH_DISPATCH_SLACK_MAX + Duration::from_nanos(1))
                .expect("past slack boundary");
            let budget = DispatchBudget {
                cause: BudgetSource::PerCallTimeout,
                deadline: Deadline::at_instant(
                    started_at
                        .checked_add(Duration::from_secs(1))
                        .expect("deadline instant"),
                ),
                duration: Duration::from_secs(1),
            };
            let clock = scripted_clock(vec![started_at, over_limit]);
            let registrations = Cell::new(0_u8);
            let register = || {
                registrations.set(registrations.get().saturating_add(1));
                Ok::<_, ()>(())
            };

            let first =
                register_backend_with_batch_guard(Some(started_at), budget, &clock, register);
            assert!(matches!(first, Ok(Ok(()))));
            let second =
                register_backend_with_batch_guard(Some(started_at), budget, &clock, register);
            assert!(matches!(
                second,
                Err(EdgeError::Internal { source })
                    if source.to_string() == DISPATCH_SLACK_MESSAGE
            ));
            assert_eq!(registrations.get(), 1);
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "the pinned SDK cause table intentionally constructs every known variant"
        )]
        fn send_error_cause_table_preserves_timeout_source() {
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
                    SendErrorCause::FanoutNotEnabled,
                    super::super::SendFailure::Unknown,
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
                    SendErrorCause::RequestCollapse,
                    super::super::SendFailure::Unknown,
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

#[cfg(any(feature = "fastly", test))]
fn reassociate_selection<Metadata>(
    mut records: Vec<(u32, Option<Metadata>)>,
    selected_position: usize,
    remaining_ids: &[u32],
) -> Result<(Metadata, Vec<Metadata>), EdgeError> {
    let selected = records
        .get_mut(selected_position)
        .and_then(|(_id, metadata)| metadata.take())
        .ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "Fastly returned an invalid selected handle index"
            ))
        })?;
    let mut remaining = Vec::with_capacity(remaining_ids.len());
    for id in remaining_ids {
        let metadata = records
            .iter_mut()
            .find_map(|(candidate, metadata)| {
                (*candidate == *id).then(|| metadata.take()).flatten()
            })
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "Fastly returned an unknown or duplicate pending handle"
                ))
            })?;
        remaining.push(metadata);
    }
    if records.iter().any(|(_id, metadata)| metadata.is_some()) {
        return Err(EdgeError::internal(anyhow::anyhow!(
            "Fastly omitted a pending handle from selection"
        )));
    }
    Ok((selected, remaining))
}

#[cfg(any(feature = "fastly", feature = "test-utils"))]
fn validate_batch_request(request: &OutboundRequest) -> Result<(), EdgeError> {
    validate_for_dispatch(request)?;
    if request.is_stream_body() {
        return Err(EdgeError::bad_request(
            "outbound batches require buffered request bodies; use send for a streamed upload",
        ));
    }
    if request.is_stream_response() {
        return Err(EdgeError::bad_request(
            "outbound batches require buffered responses; use send for a streamed response",
        ));
    }
    Ok(())
}

#[cfg(any(feature = "fastly", test))]
fn driver_selection_failure(error: EdgeError) -> OutboundBatchDriverEvent {
    OutboundBatchDriverEvent::Failed(error)
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

#[cfg(any(feature = "fastly", test))]
fn timeout_error(cause: BudgetSource) -> EdgeError {
    EdgeError::gateway_timeout_caused("outbound request deadline expired", cause)
}

#[cfg(test)]
mod send_failure_policy_tests {
    use edgezero_core::outbound::{OutboundBatch, OutboundBatchItem, OutboundSlotResult};
    use futures::executor::block_on;
    use futures_util::stream;

    use super::*;

    #[test]
    fn selection_metadata_follows_host_returned_handle_order() {
        let (selected, remaining) = reassociate_selection(
            vec![
                (11_u32, Some("first")),
                (22, Some("second")),
                (33, Some("third")),
            ],
            1,
            &[33, 11],
        )
        .expect("valid selection");

        assert_eq!(selected, "second");
        assert_eq!(remaining, ["third", "first"]);
    }

    #[test]
    fn selection_rejects_an_invalid_selected_handle_index() {
        let error = reassociate_selection(vec![(11_u32, Some("first"))], 1, &[])
            .expect_err("selected index must identify an input handle");

        assert_eq!(
            error.to_string(),
            "internal error: Fastly returned an invalid selected handle index"
        );
    }

    #[test]
    fn selection_rejects_an_unknown_or_duplicate_pending_handle() {
        let unknown = reassociate_selection(
            vec![(11_u32, Some("first")), (22, Some("second"))],
            0,
            &[99],
        )
        .expect_err("remaining handle must come from the input group");
        let duplicate = reassociate_selection(
            vec![(11_u32, Some("first")), (22, Some("second"))],
            0,
            &[22, 22],
        )
        .expect_err("remaining handle must not be duplicated");

        for error in [unknown, duplicate] {
            assert_eq!(
                error.to_string(),
                "internal error: Fastly returned an unknown or duplicate pending handle"
            );
        }
    }

    #[test]
    fn selection_rejects_an_omitted_pending_handle() {
        let error = reassociate_selection(
            vec![
                (11_u32, Some("first")),
                (22, Some("second")),
                (33, Some("third")),
            ],
            0,
            &[22],
        )
        .expect_err("every unselected handle must be returned");

        assert_eq!(
            error.to_string(),
            "internal error: Fastly omitted a pending handle from selection"
        );
    }

    #[test]
    fn selection_failure_preserves_its_diagnostic_and_previously_collected_slots() {
        let event = driver_selection_failure(EdgeError::internal(anyhow::anyhow!(
            "Fastly omitted a pending handle from selection"
        )));
        let OutboundBatchDriverEvent::Failed(_) = &event else {
            panic!("selection failure must become a driver failure event");
        };
        let completed = OutboundBatchItem::new(
            0,
            OutboundSlotResult::new(
                Duration::from_millis(1),
                Err(EdgeError::bad_gateway("completed slot failure")),
            ),
        );
        let batch = OutboundBatch::from_driver(
            2,
            stream::iter([OutboundBatchDriverEvent::Item(completed), event]),
        );
        let failure =
            block_on(batch.collect()).expect_err("selection failure must fail collection");

        assert_eq!(
            failure.error.to_string(),
            "internal error: Fastly omitted a pending handle from selection"
        );
        assert!(matches!(failure.slots.first(), Some(Some(_))));
        assert!(matches!(failure.slots.get(1), Some(None)));
    }

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
            BudgetSource::BatchCutoff,
            BudgetSource::RequestDeadline,
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
            BudgetSource::BatchCutoff,
            BudgetSource::RequestDeadline,
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
