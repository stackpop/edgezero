//! Safe native request observations and managed-lifetime accounting.

#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "Public record fields follow their diagnostic meaning; native identity and lifetime owners remain grouped with their transitions rather than alphabetized"
)]

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use edgezero_core::http::{Method, StatusCode};
use edgezero_core::ingress::{
    CheckedEffectiveHost, CheckedHostSource, NormalizedIngressHeadSummary,
};
use edgezero_core::response_egress::{
    ResponseEgressBodyKind, ResponseEgressCompletion, ResponseEgressFailureClassification,
    ResponseEgressFailureKind, ResponseEgressFallbackDisposition, ResponseEgressOutcome,
    ResponseEgressReport,
};
use edgezero_core::{MonotonicClock, MonotonicInstant};

use crate::proxy::{CheckedProxyMetadata, ForwardingResult, Scheme};

/// Framework-generated correlation value, not a credential or visitor-supplied ID.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RequestId {
    nonce: [u8; 16],
    host: u64,
    request: u64,
}

impl fmt::Display for RequestId {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.nonce {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "{:016x}{:016x}", self.host, self.request)
    }
}

impl fmt::Debug for RequestId {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Immutable checked facts captured before managed admission.
#[derive(Clone)]
pub struct NativeIngressMetadata {
    request_id: RequestId,
    proxy: CheckedProxyMetadata,
    received: NormalizedIngressHeadSummary,
}

impl NativeIngressMetadata {
    pub(crate) fn new(
        request_id: RequestId,
        proxy: CheckedProxyMetadata,
        received: NormalizedIngressHeadSummary,
    ) -> Self {
        Self {
            request_id,
            proxy,
            received,
        }
    }

    #[must_use]
    #[inline]
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    #[must_use]
    #[inline]
    pub fn direct_peer(&self) -> Option<SocketAddr> {
        self.proxy.direct_peer()
    }

    #[must_use]
    #[inline]
    pub fn effective_client(&self) -> Option<IpAddr> {
        self.proxy.effective_client()
    }

    #[must_use]
    #[inline]
    pub fn effective_host(&self) -> &CheckedEffectiveHost {
        self.proxy.effective_host()
    }

    #[must_use]
    #[inline]
    pub fn effective_scheme(&self) -> Scheme {
        self.proxy.effective_scheme()
    }

    #[must_use]
    #[inline]
    pub fn client_source(&self) -> CheckedHostSource {
        self.proxy.client_source()
    }

    #[must_use]
    #[inline]
    pub fn host_source(&self) -> CheckedHostSource {
        self.proxy.effective_host().source()
    }

    #[must_use]
    #[inline]
    pub fn scheme_source(&self) -> CheckedHostSource {
        self.proxy.scheme_source()
    }

    #[must_use]
    #[inline]
    pub fn forwarding_result(&self) -> ForwardingResult {
        self.proxy.forwarding_result()
    }

    #[must_use]
    #[inline]
    pub fn received_normalized_head(&self) -> &NormalizedIngressHeadSummary {
        &self.received
    }
}

/// One coherent, momentary observation of managed guards, not kernel or detached work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeDiagnosticsSnapshot {
    pub active_requests: u64,
    /// Includes idle keep-alive connection tasks.
    pub active_connections: u64,
    pub idle_connections: u64,
}

pub trait NativeRequestObserver: Send + Sync + 'static {
    fn observe(&self, record: &NativeRequestRecord);
}

/// Sharing this handle explicitly aggregates counters across managed runners.
#[derive(Clone, Default)]
pub struct NativeDiagnosticsHandle {
    counters: Arc<Mutex<NativeDiagnosticsSnapshot>>,
    observer: Option<Arc<dyn NativeRequestObserver>>,
}

impl NativeDiagnosticsHandle {
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    #[inline]
    pub fn with_request_observer<O>(mut self, observer: O) -> Self
    where
        O: NativeRequestObserver,
    {
        self.observer = Some(Arc::new(observer));
        self
    }

    #[must_use]
    #[inline]
    pub fn snapshot(&self) -> NativeDiagnosticsSnapshot {
        *lock(&self.counters)
    }
}

impl fmt::Debug for NativeDiagnosticsHandle {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeDiagnosticsHandle")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

/// A no-response disposition, deliberately distinct from response-egress outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressDisposition {
    Aborted,
    Draining,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRequestOutcome {
    Egress(ResponseEgressOutcome),
    Ingress(IngressDisposition),
}

/// Categories captured by owning framework boundaries, never error messages or sources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRequestFailure {
    FrameworkRouting { kind: &'static str },
    PropagatedError { kind: &'static str },
    FallbackBodyExceeded,
    FallbackReadTimedOut,
    AdmissionRefused,
}

/// Allowlisted observation. Bytes and selected status do not establish client receipt.
#[derive(Clone, Debug)]
pub struct NativeRequestRecord {
    pub event: &'static str,
    pub request_id: RequestId,
    pub method: &'static str,
    /// Registered template only, or the fixed `unknown` value.
    pub route: String,
    pub status: Option<StatusCode>,
    /// Absent when the owning monotonic clock moved backwards.
    pub duration: Option<Duration>,
    pub outcome: NativeRequestOutcome,
    pub failure: Option<NativeRequestFailure>,
    pub forwarding_result: ForwardingResult,
    pub bytes_written: Option<u64>,
    pub body_kind: Option<ResponseEgressBodyKind>,
    pub fallback_disposition: Option<ResponseEgressFallbackDisposition>,
}

impl NativeRequestRecord {
    /// Severity follows typed ownership, never status alone.
    #[must_use]
    #[inline]
    pub fn level(&self) -> log::Level {
        if self.duration.is_none() {
            return log::Level::Error;
        }
        let ordinary_failure = matches!(
            self.failure,
            None | Some(NativeRequestFailure::FrameworkRouting { .. })
        );
        let ordinary_outcome = matches!(
            self.outcome,
            NativeRequestOutcome::Egress(
                ResponseEgressOutcome::Completed | ResponseEgressOutcome::HostHandoff
            ) | NativeRequestOutcome::Ingress(IngressDisposition::Draining)
        );
        if ordinary_failure
            && ordinary_outcome
            && self.forwarding_result != ForwardingResult::Malformed
        {
            log::Level::Info
        } else {
            log::Level::Warn
        }
    }
}

#[derive(Clone)]
pub(crate) struct NativeSession {
    identity: Arc<SessionIdentity>,
    handle: NativeDiagnosticsHandle,
    request_records: bool,
}

struct SessionIdentity {
    nonce: [u8; 16],
    host: u64,
    requests: AtomicU64,
}

static PROCESS_NONCE: OnceLock<Result<[u8; 16], IdentityFault>> = OnceLock::new();
static HOST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, thiserror::Error)]
enum IdentityFault {
    #[error("native_identity_entropy_failed")]
    Entropy,
    #[error("native_identity_host_sequence_exhausted")]
    HostSequence,
    #[error("native_identity_request_sequence_exhausted")]
    RequestSequence,
    #[error("native_diagnostics_connection_scope_mismatch")]
    ConnectionScope,
}

impl NativeSession {
    pub(crate) fn new(
        handle: NativeDiagnosticsHandle,
        request_records: bool,
    ) -> anyhow::Result<Self> {
        let nonce = *PROCESS_NONCE.get_or_init(|| nonce_from(getrandom::fill));
        Self::from_sources(handle, request_records, nonce, &HOST_SEQUENCE).map_err(Into::into)
    }

    fn from_sources(
        handle: NativeDiagnosticsHandle,
        request_records: bool,
        nonce: Result<[u8; 16], IdentityFault>,
        hosts: &AtomicU64,
    ) -> Result<Self, IdentityFault> {
        Ok(Self {
            identity: Arc::new(SessionIdentity {
                nonce: nonce?,
                host: next_sequence(hosts).ok_or(IdentityFault::HostSequence)?,
                requests: AtomicU64::new(0),
            }),
            handle,
            request_records,
        })
    }

    /// Construct before submitting the owned task, including tasks never polled.
    pub(crate) fn connection(&self) -> (ConnectionActivity, ConnectionGuard) {
        let activity = ConnectionActivity {
            state: Arc::new(Mutex::new(ConnectionState {
                requests: 0,
                closed: false,
            })),
            counters: Arc::clone(&self.handle.counters),
        };
        let mut counts = lock(&activity.counters);
        // Metrics must not introduce a panic in managed guard ownership.
        counts.active_connections = counts.active_connections.saturating_add(1);
        counts.idle_connections = counts.idle_connections.saturating_add(1);
        drop(counts);
        let guard = ConnectionGuard {
            activity: activity.clone(),
        };
        (activity, guard)
    }

    pub(crate) fn request(
        &self,
        method: &Method,
        start: MonotonicInstant,
        clock: MonotonicClock,
        activity: Option<ConnectionActivity>,
    ) -> anyhow::Result<RequestGuard> {
        if activity
            .as_ref()
            .is_some_and(|link| !Arc::ptr_eq(&link.counters, &self.handle.counters))
        {
            return Err(IdentityFault::ConnectionScope.into());
        }
        let Some(sequence) = next_sequence(&self.identity.requests) else {
            isolated(
                || log::error!(target: "edgezero::native", "lifecycle reason=native_identity_request_sequence_exhausted"),
            );
            return Err(IdentityFault::RequestSequence.into());
        };
        if let Some(link) = &activity {
            let mut connection = lock(&link.state);
            let mut counts = lock(&link.counters);
            if !connection.closed {
                if connection.requests == 0 {
                    counts.idle_connections = counts.idle_connections.saturating_sub(1);
                }
                connection.requests = connection.requests.saturating_add(1);
            }
            counts.active_requests = counts.active_requests.saturating_add(1);
        } else {
            let mut counts = lock(&self.handle.counters);
            counts.active_requests = counts.active_requests.saturating_add(1);
        }
        Ok(RequestGuard {
            session: self.clone(),
            id: RequestId {
                nonce: self.identity.nonce,
                host: self.identity.host,
                request: sequence,
            },
            method: method_category(method),
            start,
            clock,
            activity,
            forwarding_result: ForwardingResult::Absent,
            failure: None,
            released: false,
        })
    }
}

fn nonce_from<E>(fill: impl FnOnce(&mut [u8]) -> Result<(), E>) -> Result<[u8; 16], IdentityFault> {
    let mut nonce = [0; 16];
    fill(&mut nonce).map_err(|_entropy_error| IdentityFault::Entropy)?;
    Ok(nonce)
}

fn next_sequence(sequence: &AtomicU64) -> Option<u64> {
    sequence
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()
}

struct ConnectionState {
    requests: u64,
    closed: bool,
}

#[derive(Clone)]
pub(crate) struct ConnectionActivity {
    state: Arc<Mutex<ConnectionState>>,
    counters: Arc<Mutex<NativeDiagnosticsSnapshot>>,
}

pub(crate) struct ConnectionGuard {
    activity: ConnectionActivity,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut connection = lock(&self.activity.state);
        let mut counts = lock(&self.activity.counters);
        connection.closed = true;
        counts.active_connections = counts.active_connections.saturating_sub(1);
        if connection.requests == 0 {
            counts.idle_connections = counts.idle_connections.saturating_sub(1);
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SelectedStatus(Arc<AtomicU16>);

impl SelectedStatus {
    pub(crate) fn select(&self, status: StatusCode) {
        self.0.store(status.as_u16(), Ordering::Relaxed);
    }

    fn get(&self) -> Option<StatusCode> {
        StatusCode::from_u16(self.0.load(Ordering::Relaxed)).ok()
    }
}

pub(crate) struct RequestGuard {
    session: NativeSession,
    id: RequestId,
    method: &'static str,
    start: MonotonicInstant,
    clock: MonotonicClock,
    activity: Option<ConnectionActivity>,
    forwarding_result: ForwardingResult,
    failure: Option<NativeRequestFailure>,
    released: bool,
}

impl RequestGuard {
    pub(crate) fn request_id(&self) -> RequestId {
        self.id
    }

    pub(crate) fn set_forwarding_result(&mut self, result: ForwardingResult) {
        self.forwarding_result = result;
    }

    pub(crate) fn set_failure(&mut self, failure: Option<ResponseEgressFailureClassification>) {
        self.failure = failure.map(|classification| match classification.kind() {
            ResponseEgressFailureKind::PropagatedError => {
                let kind = classification.error_kind().unwrap_or("unknown");
                if classification.is_framework_route_error() {
                    NativeRequestFailure::FrameworkRouting { kind }
                } else {
                    NativeRequestFailure::PropagatedError { kind }
                }
            }
            ResponseEgressFailureKind::FallbackBodyExceeded => {
                NativeRequestFailure::FallbackBodyExceeded
            }
            ResponseEgressFailureKind::FallbackReadTimedOut => {
                NativeRequestFailure::FallbackReadTimedOut
            }
            _ => NativeRequestFailure::PropagatedError { kind: "unknown" },
        });
    }

    pub(crate) fn set_ingress_error_kind(&mut self, kind: &'static str) {
        self.failure = Some(NativeRequestFailure::PropagatedError { kind });
    }

    pub(crate) fn refused(&mut self) {
        self.failure = Some(NativeRequestFailure::AdmissionRefused);
    }

    pub(crate) fn completion(mut self) -> (ResponseEgressCompletion, SelectedStatus) {
        let selected = SelectedStatus::default();
        let captured = selected.clone();
        let completion =
            ResponseEgressCompletion::new_with_terminal_time(move |report, terminal| {
                self.release();
                let record = self.record(
                    NativeRequestOutcome::Egress(report.outcome),
                    terminal,
                    captured.get(),
                    Some(report),
                );
                self.emit(&record);
            });
        (completion, selected)
    }

    pub(crate) fn finish_ingress(mut self, disposition: IngressDisposition) {
        self.finish_ingress_at(disposition, self.clock.now());
    }

    fn finish_ingress_at(&mut self, disposition: IngressDisposition, terminal: MonotonicInstant) {
        self.release();
        let record = self.record(
            NativeRequestOutcome::Ingress(disposition),
            terminal,
            None,
            None,
        );
        self.emit(&record);
    }

    fn record(
        &self,
        outcome: NativeRequestOutcome,
        terminal: MonotonicInstant,
        status: Option<StatusCode>,
        report: Option<&ResponseEgressReport>,
    ) -> NativeRequestRecord {
        NativeRequestRecord {
            event: if report.is_some() {
                "request_terminal"
            } else {
                "request_ingress"
            },
            request_id: self.id,
            method: self.method,
            route: report
                .and_then(|terminal_report| terminal_report.route.as_ref())
                .map_or_else(|| "unknown".into(), |route| route.pattern().to_owned()),
            status,
            duration: terminal.checked_duration_since(self.start),
            outcome,
            failure: self.failure,
            forwarding_result: self.forwarding_result,
            bytes_written: report.map(|terminal_report| terminal_report.bytes_written),
            body_kind: report.map(|terminal_report| terminal_report.body_kind),
            fallback_disposition: report
                .and_then(|terminal_report| terminal_report.fallback_disposition),
        }
    }

    fn emit(&self, record: &NativeRequestRecord) {
        emit_with(
            record,
            self.session.request_records,
            |safe_record| log::log!(target: "edgezero::native", safe_record.level(), "{}", SafeRecord(safe_record)),
            self.session.handle.observer.as_deref(),
        );
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(activity) = &self.activity {
            let mut connection = lock(&activity.state);
            let mut counts = lock(&activity.counters);
            counts.active_requests = counts.active_requests.saturating_sub(1);
            if !connection.closed {
                connection.requests = connection.requests.saturating_sub(1);
                if connection.requests == 0 {
                    counts.idle_connections = counts.idle_connections.saturating_add(1);
                }
            }
        } else {
            let mut counts = lock(&self.session.handle.counters);
            counts.active_requests = counts.active_requests.saturating_sub(1);
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.released {
            // No report/status for cancellation or an unbegun envelope's abandonment.
            self.release();
            isolated(|| self.finish_ingress_at(IngressDisposition::Abandoned, self.clock.now()));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn isolated(action: impl FnOnce()) {
    drop(catch_unwind(AssertUnwindSafe(action)));
}

fn emit_with(
    record: &NativeRequestRecord,
    enabled: bool,
    log_sink: impl FnOnce(&NativeRequestRecord),
    observer: Option<&dyn NativeRequestObserver>,
) {
    if enabled {
        isolated(|| log_sink(record));
    }
    if let Some(sink) = observer {
        isolated(|| sink.observe(record));
    }
}

fn method_category(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::CONNECT => "CONNECT",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::PATCH => "PATCH",
        _ => "other",
    }
}

struct SafeRecord<'record>(&'record NativeRequestRecord);

impl fmt::Display for SafeRecord<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let record = self.0;
        write!(
            f,
            "{} request_id={} method={} route=",
            record.event, record.request_id, record.method
        )?;
        write!(f, "\"{}\"", record.route.escape_debug())?;
        match record.status {
            Some(status) => write!(f, " status={}", status.as_u16())?,
            None => f.write_str(" status=none")?,
        }
        match record.duration {
            Some(duration) => write!(f, " duration_us={}", duration.as_micros())?,
            None => f.write_str(" duration_us=none clock_fault=backwards")?,
        }
        write!(
            f,
            " outcome={} failure={} forwarding_result={}",
            outcome_name(record.outcome),
            failure_name(record.failure),
            record.forwarding_result.as_str()
        )?;
        if let Some(bytes) = record.bytes_written {
            write!(f, " bytes_written={bytes}")?;
        }
        if let Some(kind) = record.body_kind {
            f.write_str(match kind {
                ResponseEgressBodyKind::Application => " body_kind=application",
                ResponseEgressBodyKind::Fallback => " body_kind=fallback",
                _ => " body_kind=unknown",
            })?;
        }
        if let Some(disposition) = record.fallback_disposition {
            f.write_str(match disposition {
                ResponseEgressFallbackDisposition::Aborted => " fallback_disposition=aborted",
                ResponseEgressFallbackDisposition::Completed => " fallback_disposition=completed",
                _ => " fallback_disposition=unknown",
            })?;
        }
        Ok(())
    }
}

fn outcome_name(outcome: NativeRequestOutcome) -> &'static str {
    match outcome {
        NativeRequestOutcome::Ingress(IngressDisposition::Aborted) => "ingress_aborted",
        NativeRequestOutcome::Ingress(IngressDisposition::Draining) => "ingress_draining",
        NativeRequestOutcome::Ingress(IngressDisposition::Abandoned) => "ingress_abandoned",
        NativeRequestOutcome::Egress(egress_outcome) => match egress_outcome {
            ResponseEgressOutcome::ClientDisconnected => "client_disconnected",
            ResponseEgressOutcome::Completed => "completed",
            ResponseEgressOutcome::ConversionError => "conversion_error",
            ResponseEgressOutcome::DeadlineExceeded => "deadline_exceeded",
            ResponseEgressOutcome::HostHandoff => "host_handoff",
            ResponseEgressOutcome::RequestCancelled => "request_cancelled",
            ResponseEgressOutcome::SourceError => "source_error",
            ResponseEgressOutcome::TransportError => "transport_error",
            ResponseEgressOutcome::Unspecified => "unspecified",
            _ => "unknown",
        },
    }
}

fn failure_name(failure: Option<NativeRequestFailure>) -> &'static str {
    match failure {
        None => "none",
        Some(
            NativeRequestFailure::FrameworkRouting { kind }
            | NativeRequestFailure::PropagatedError { kind },
        ) => kind,
        Some(NativeRequestFailure::FallbackBodyExceeded) => "fallback_body_exceeded",
        Some(NativeRequestFailure::FallbackReadTimedOut) => "fallback_read_timed_out",
        Some(NativeRequestFailure::AdmissionRefused) => "admission_refused",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::iter::repeat_with;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use edgezero_core::http::{HeaderMap, Version};
    use edgezero_core::response_egress::{
        ResponseEgressAttempt, ResponseEgressHead, ResponseEgressObserverHandle,
    };
    use edgezero_core::router::RouteMetadata;

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingObserver {
        records: Arc<Mutex<Vec<NativeRequestRecord>>>,
        snapshots: Arc<Mutex<Vec<NativeDiagnosticsSnapshot>>>,
        handle: NativeDiagnosticsHandle,
        panic: bool,
    }

    impl NativeRequestObserver for RecordingObserver {
        fn observe(&self, record: &NativeRequestRecord) {
            lock(&self.records).push(record.clone());
            lock(&self.snapshots).push(self.handle.snapshot());
            assert!(!self.panic, "observer fixture panic");
        }
    }

    fn session(handle: NativeDiagnosticsHandle, enabled: bool) -> NativeSession {
        NativeSession::from_sources(handle, enabled, Ok([0x19; 16]), &AtomicU64::new(0))
            .expect("test identity")
    }

    fn observed_session(enabled: bool) -> (NativeSession, RecordingObserver) {
        let observer = RecordingObserver::default();
        let handle = observer
            .handle
            .clone()
            .with_request_observer(observer.clone());
        (session(handle, enabled), observer)
    }

    fn request(session: &NativeSession, activity: Option<ConnectionActivity>) -> RequestGuard {
        session
            .request(
                &Method::GET,
                MonotonicInstant::now(),
                MonotonicClock::default(),
                activity,
            )
            .expect("request")
    }

    fn attempt(
        completion: ResponseEgressCompletion,
        start: MonotonicInstant,
        egress_start: MonotonicInstant,
        clock: MonotonicClock,
        route: Option<&RouteMetadata>,
    ) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let head = ResponseEgressHead::new(
            StatusCode::OK,
            Version::HTTP_11,
            &headers,
            None,
            start,
            route,
        );
        ResponseEgressAttempt::new(
            &head,
            egress_start,
            completion,
            ResponseEgressObserverHandle::default(),
            clock,
        )
    }

    #[test]
    fn identity_is_fixed_hex_and_sequences_are_shared_only_with_session_clones() {
        let hosts = AtomicU64::new(0);
        let first = NativeSession::from_sources(
            NativeDiagnosticsHandle::new(),
            false,
            Ok([0xab; 16]),
            &hosts,
        )
        .unwrap();
        let second = NativeSession::from_sources(
            NativeDiagnosticsHandle::new(),
            false,
            Ok([0xab; 16]),
            &hosts,
        )
        .unwrap();
        let ids = [
            request(&first, None).request_id(),
            request(&first.clone(), None).request_id(),
            request(&second, None).request_id(),
        ];
        assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), 3);
        assert_eq!(
            ids[0].to_string(),
            format!("{}{:016x}{:016x}", "ab".repeat(16), 0_u64, 0_u64)
        );
        assert_eq!(ids[1].request, 1);
        assert_eq!(ids[2].host, 1);
        assert_eq!(ids[2].request, 0);
        for id in ids {
            let rendered = id.to_string();
            assert_eq!(rendered.len(), 64);
            assert!(rendered.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(format!("{id:?}"), rendered);
        }
    }

    #[test]
    fn entropy_and_host_exhaustion_fail_without_fallback_or_wrap() {
        let nonce = nonce_from(|bytes| {
            bytes.fill(0x12);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(nonce, [0x12; 16]);
        let failure = nonce_from(|_| Err("secret entropy source"));
        assert_eq!(
            failure.unwrap_err().to_string(),
            "native_identity_entropy_failed"
        );
        let hosts = AtomicU64::new(u64::MAX - 1);
        let first =
            NativeSession::from_sources(NativeDiagnosticsHandle::new(), false, Ok(nonce), &hosts)
                .unwrap();
        assert_eq!(first.identity.host, u64::MAX - 1);
        let error =
            NativeSession::from_sources(NativeDiagnosticsHandle::new(), false, Ok(nonce), &hosts)
                .err()
                .unwrap();
        assert_eq!(error.to_string(), "native_identity_host_sequence_exhausted");
        assert_eq!(hosts.load(Ordering::Relaxed), u64::MAX);
        let unused_hosts = AtomicU64::new(0);
        assert!(
            NativeSession::from_sources(
                NativeDiagnosticsHandle::new(),
                false,
                failure,
                &unused_hosts
            )
            .is_err()
        );
        assert_eq!(unused_hosts.load(Ordering::Relaxed), 0);
        let other = NativeSession::from_sources(
            NativeDiagnosticsHandle::new(),
            false,
            Ok([0x13; 16]),
            &AtomicU64::new(0),
        )
        .unwrap();
        assert_ne!(
            request(&first, None).request_id(),
            request(&other, None).request_id()
        );
    }

    #[test]
    fn request_exhaustion_preserves_counters_and_never_reuses_an_id() {
        let session = session(NativeDiagnosticsHandle::new(), false);
        session
            .identity
            .requests
            .store(u64::MAX - 1, Ordering::Relaxed);
        let guard = request(&session, None);
        assert_eq!(guard.request_id().request, u64::MAX - 1);
        drop(guard);
        assert!(
            session
                .request(
                    &Method::GET,
                    MonotonicInstant::now(),
                    MonotonicClock::default(),
                    None
                )
                .is_err()
        );
        assert_eq!(session.identity.requests.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn concurrent_requests_share_checked_sequence_without_collisions() {
        let session = session(NativeDiagnosticsHandle::new(), false);
        let workers: Vec<_> = repeat_with(|| {
            let worker_session = session.clone();
            thread::spawn(move || {
                repeat_with(|| request(&worker_session, None).request_id())
                    .take(64)
                    .collect::<Vec<_>>()
            })
        })
        .take(8)
        .collect();
        let ids: HashSet<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 512);
        assert_eq!(session.handle.snapshot().active_requests, 0);
    }

    #[test]
    fn connection_counts_exist_before_polling_and_track_two_request_lifetimes() {
        let session = session(NativeDiagnosticsHandle::new(), false);
        let (activity, connection) = session.connection();
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 0,
                active_connections: 1,
                idle_connections: 1
            }
        );
        let first = request(&session, Some(activity.clone()));
        let second = request(&session, Some(activity));
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 2,
                active_connections: 1,
                idle_connections: 0
            }
        );
        drop(first);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 1,
                active_connections: 1,
                idle_connections: 0
            }
        );
        drop(second);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 0,
                active_connections: 1,
                idle_connections: 1
            }
        );
        // No future needs to have been polled for task cancellation to release this guard.
        drop(connection);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn closed_connection_late_release_cannot_recreate_idle_connections() {
        let session = session(NativeDiagnosticsHandle::new(), false);
        let (activity, connection) = session.connection();
        let first = request(&session, Some(activity.clone()));
        drop(connection);
        let late = request(&session, Some(activity));
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 2,
                active_connections: 0,
                idle_connections: 0
            }
        );
        drop(first);
        drop(late);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn explicit_handle_sharing_aggregates_connections_and_requests() {
        let handle = NativeDiagnosticsHandle::new();
        let first = session(handle.clone(), false);
        let second = session(handle.clone(), false);
        let (_, connection) = first.connection();
        let guard = request(&second, None);
        assert_eq!(
            handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 1,
                active_connections: 1,
                idle_connections: 1
            }
        );
        drop((connection, guard));
        assert_eq!(handle.snapshot(), NativeDiagnosticsSnapshot::default());
    }

    #[test]
    fn activity_from_a_different_handle_fails_without_incrementing_either_scope() {
        let first = session(NativeDiagnosticsHandle::new(), false);
        let second = session(NativeDiagnosticsHandle::new(), false);
        let (activity, connection) = first.connection();
        assert!(
            second
                .request(
                    &Method::GET,
                    MonotonicInstant::now(),
                    MonotonicClock::default(),
                    Some(activity)
                )
                .is_err()
        );
        assert_eq!(first.handle.snapshot().active_requests, 0);
        assert_eq!(
            second.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
        drop(connection);
    }

    #[test]
    fn ingress_and_unbegun_completion_abandonment_have_no_status_or_report_fields() {
        let (session, observer) = observed_session(false);
        request(&session, None).finish_ingress(IngressDisposition::Aborted);
        request(&session, None).finish_ingress(IngressDisposition::Draining);
        let (completion, status) = request(&session, None).completion();
        status.select(StatusCode::OK);
        drop(completion);
        let records = lock(&observer.records);
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0].outcome,
            NativeRequestOutcome::Ingress(IngressDisposition::Aborted)
        );
        assert_eq!(
            records[1].outcome,
            NativeRequestOutcome::Ingress(IngressDisposition::Draining)
        );
        assert_eq!(
            records[2].outcome,
            NativeRequestOutcome::Ingress(IngressDisposition::Abandoned)
        );
        for record in records.iter() {
            assert_eq!(record.event, "request_ingress");
            assert_eq!(record.status, None);
            assert_eq!(record.bytes_written, None);
            assert_eq!(record.body_kind, None);
            assert_eq!(record.fallback_disposition, None);
        }
        assert!(
            lock(&observer.snapshots)
                .iter()
                .all(|snapshot| snapshot.active_requests == 0)
        );
    }

    #[test]
    fn authoritative_terminal_time_excludes_prior_app_callback_and_sink_delays() {
        let (session, observer) = observed_session(false);
        let start = MonotonicInstant::now();
        let egress_start = start + Duration::from_secs(7);
        let terminal = start + Duration::from_secs(11);
        let now = Arc::new(Mutex::new(terminal));
        let source = Arc::clone(&now);
        let clock = MonotonicClock::new(move || *lock(&source));
        let guard = session
            .request(&Method::GET, start, clock.clone(), None)
            .unwrap();
        let private_id = guard.request_id();
        let (native, status) = guard.completion();
        status.select(StatusCode::OK);
        let app_source = Arc::clone(&now);
        let app_reports = Arc::new(Mutex::new(Vec::new()));
        let captured_reports = Arc::clone(&app_reports);
        let app = ResponseEgressCompletion::new(move |report| {
            lock(&captured_reports).push(report.clone());
            *lock(&app_source) = terminal + Duration::from_secs(100);
        });
        let route = RouteMetadata::new(Method::GET, "/orders/{id}");
        let mut attempt = attempt(app.join(native), start, egress_start, clock, Some(&route));
        assert!(attempt.begin_writing());
        assert!(attempt.account_bytes(9, terminal));
        assert!(attempt.complete(terminal));
        assert!(!attempt.complete(terminal));
        drop(attempt);
        let records = lock(&observer.records);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.request_id, private_id);
        assert_eq!(record.duration, Some(Duration::from_secs(11)));
        assert_eq!(lock(&app_reports)[0].elapsed, Duration::from_secs(4));
        assert_eq!(record.status, Some(StatusCode::OK));
        assert_eq!(record.route, "/orders/{id}");
        assert_eq!(record.bytes_written, Some(9));
        assert_eq!(
            record.outcome,
            NativeRequestOutcome::Egress(ResponseEgressOutcome::Completed)
        );
        assert_eq!(lock(&observer.snapshots)[0].active_requests, 0);
        emit_with(
            record,
            true,
            |_| *lock(&now) = terminal + Duration::from_secs(200),
            None,
        );
        assert_eq!(record.duration, Some(Duration::from_secs(11)));
    }

    #[test]
    fn failing_attempt_retains_selected_status_and_fallback_updates_use_same_slot() {
        for fallback in [false, true] {
            let (session, observer) = observed_session(false);
            let start = MonotonicInstant::now();
            let (completion, status) = session
                .request(&Method::GET, start, MonotonicClock::default(), None)
                .unwrap()
                .completion();
            status.select(StatusCode::OK);
            let mut attempt = attempt(completion, start, start, MonotonicClock::default(), None);
            if fallback {
                assert!(attempt.begin_fallback(ResponseEgressOutcome::ConversionError));
                status.select(StatusCode::INTERNAL_SERVER_ERROR);
                assert!(attempt.begin_writing());
                assert!(
                    attempt.finish_fallback(ResponseEgressFallbackDisposition::Completed, start)
                );
            } else {
                assert!(attempt.begin_writing());
                assert!(attempt.account_bytes(3, start));
                assert!(attempt.terminate(ResponseEgressOutcome::SourceError, start));
            }
            drop(attempt);
            let records = lock(&observer.records);
            assert_eq!(records.len(), 1);
            assert_eq!(
                records[0].status,
                Some(if fallback {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                })
            );
            assert_eq!(
                records[0].outcome,
                NativeRequestOutcome::Egress(if fallback {
                    ResponseEgressOutcome::ConversionError
                } else {
                    ResponseEgressOutcome::SourceError
                })
            );
            assert_eq!(records[0].level(), log::Level::Warn);
            assert_eq!(session.handle.snapshot().active_requests, 0);
        }
    }

    #[test]
    fn drop_and_completion_release_before_panicking_observer() {
        let observer = RecordingObserver {
            panic: true,
            ..RecordingObserver::default()
        };
        let session = session(
            observer
                .handle
                .clone()
                .with_request_observer(observer.clone()),
            false,
        );
        catch_unwind(AssertUnwindSafe(|| drop(request(&session, None))))
            .expect("observer panic must not escape request Drop");
        let start = MonotonicInstant::now();
        let (completion, status) = request(&session, None).completion();
        status.select(StatusCode::OK);
        let mut attempt = attempt(completion, start, start, MonotonicClock::default(), None);
        attempt.begin_writing();
        assert!(attempt.complete(start + Duration::from_secs(1)));
        drop(attempt);
        assert_eq!(lock(&observer.records).len(), 2);
        assert!(
            lock(&observer.snapshots)
                .iter()
                .all(|snapshot| snapshot.active_requests == 0)
        );
        assert_eq!(session.handle.snapshot().active_requests, 0);
    }

    #[test]
    fn sink_panics_are_independent_and_never_retry_logging() {
        let (session, observer) = observed_session(false);
        drop(request(&session, None));
        let record = lock(&observer.records)[0].clone();
        let logs = AtomicUsize::new(0);
        emit_with(
            &record,
            true,
            |_| {
                logs.fetch_add(1, Ordering::Relaxed);
                panic!("logger fixture panic");
            },
            Some(&observer),
        );
        assert_eq!(logs.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&observer.records).len(), 2);
        let panic_observer = RecordingObserver {
            panic: true,
            ..RecordingObserver::default()
        };
        catch_unwind(AssertUnwindSafe(|| {
            emit_with(
                &record,
                true,
                |_| {
                    logs.fetch_add(1, Ordering::Relaxed);
                    panic!("logger fixture panic");
                },
                Some(&panic_observer),
            );
        }))
        .expect("logger and observer panics must be contained independently");
        assert_eq!(logs.load(Ordering::Relaxed), 2);
        assert_eq!(lock(&panic_observer.records).len(), 1);
    }

    #[test]
    fn opt_out_suppresses_only_log_sink_and_keeps_observers_and_accounting() {
        let (session, observer) = observed_session(false);
        drop(request(&session, None));
        let record = lock(&observer.records)[0].clone();
        emit_with(
            &record,
            false,
            |_| panic!("disabled log sink must not run"),
            Some(&observer),
        );
        assert_eq!(lock(&observer.records).len(), 2);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn severity_is_typed_not_status_and_extension_methods_never_leak() {
        let (session, observer) = observed_session(false);
        let start = MonotonicInstant::now();
        let extension = Method::from_bytes(b"SECRET-METHOD").unwrap();
        let (completion, status) = session
            .request(&extension, start, MonotonicClock::default(), None)
            .unwrap()
            .completion();
        status.select(StatusCode::PAYLOAD_TOO_LARGE);
        let mut attempt = attempt(completion, start, start, MonotonicClock::default(), None);
        attempt.begin_writing();
        attempt.complete(start);
        let ordinary = lock(&observer.records)[0].clone();
        assert_eq!(ordinary.method, "other");
        assert_eq!(ordinary.level(), log::Level::Info);
        assert!(!SafeRecord(&ordinary).to_string().contains("SECRET"));
        let mut classified = ordinary.clone();
        classified.failure = Some(NativeRequestFailure::AdmissionRefused);
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = Some(NativeRequestFailure::FrameworkRouting { kind: "not_found" });
        assert_eq!(classified.level(), log::Level::Info);
        classified.failure = Some(NativeRequestFailure::PropagatedError { kind: "not_found" });
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = Some(NativeRequestFailure::FrameworkRouting {
            kind: "method_not_allowed",
        });
        assert_eq!(classified.level(), log::Level::Info);
        classified.failure = Some(NativeRequestFailure::PropagatedError {
            kind: "method_not_allowed",
        });
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = Some(NativeRequestFailure::PropagatedError { kind: "internal" });
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = Some(NativeRequestFailure::FallbackBodyExceeded);
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = Some(NativeRequestFailure::FallbackReadTimedOut);
        assert_eq!(classified.level(), log::Level::Warn);
        classified.failure = None;
        classified.forwarding_result = ForwardingResult::Malformed;
        assert_eq!(classified.level(), log::Level::Warn);
        classified.duration = None;
        assert_eq!(classified.level(), log::Level::Error);
    }

    #[test]
    fn backwards_clock_is_an_explicit_safe_fault_not_wall_time_or_zero_duration() {
        let (session, observer) = observed_session(false);
        let start = MonotonicInstant::now();
        let clock = MonotonicClock::new(move || start);
        let guard = session
            .request(&Method::GET, start + Duration::from_secs(1), clock, None)
            .unwrap();
        guard.finish_ingress(IngressDisposition::Aborted);
        let record = lock(&observer.records)[0].clone();
        assert_eq!(record.duration, None);
        assert_eq!(record.level(), log::Level::Error);
        assert!(
            SafeRecord(&record)
                .to_string()
                .contains("clock_fault=backwards")
        );
        assert_eq!(session.handle.snapshot().active_requests, 0);
    }

    #[test]
    fn metadata_preserves_original_summary_and_public_replacement_cannot_change_private_identity() {
        use crate::proxy::{TrustedProxyPolicy, normalize, strip_forwarding_headers};
        use edgezero_core::http::{HeaderValue, Request};
        use edgezero_core::ingress::{
            IngressHeadLimits, validate_normalized_ingress_parts_with_summary,
        };

        let (session, observer) = observed_session(false);
        let guard = request(&session, None);
        let authoritative = guard.request_id();
        let other_guard = request(&session, None);
        let mut received = Request::new(edgezero_core::Body::empty());
        *received.uri_mut() = "/orders/SECRET-PATH?token=SECRET-QUERY".parse().unwrap();
        received
            .headers_mut()
            .insert("host", HeaderValue::from_static("direct.example:8080"));
        received.headers_mut().insert(
            "forwarded",
            HeaderValue::from_static("for=192.0.2.99;host=spoof.example;proto=https"),
        );
        received.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_static("SECRET-VISITOR-ID"),
        );
        let (mut parts, _) = received.into_parts();
        let summary =
            validate_normalized_ingress_parts_with_summary(&parts, IngressHeadLimits::default())
                .unwrap();
        let peer: SocketAddr = "192.0.2.1:4567".parse().unwrap();
        let checked = normalize(&TrustedProxyPolicy::no_trust(), Some(peer), &parts);
        let metadata = NativeIngressMetadata::new(authoritative, checked.clone(), summary);
        assert_eq!(metadata.request_id(), authoritative);
        assert_eq!(metadata.direct_peer(), Some(peer));
        assert_eq!(metadata.effective_client(), Some(peer.ip()));
        assert_eq!(
            metadata.effective_host().authority(),
            Some("direct.example:8080")
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Http);
        assert_eq!(metadata.client_source(), CheckedHostSource::Direct);
        assert_eq!(metadata.host_source(), CheckedHostSource::Direct);
        assert_eq!(metadata.scheme_source(), CheckedHostSource::Direct);
        assert_eq!(
            metadata.forwarding_result(),
            ForwardingResult::UntrustedPeer
        );
        assert_eq!(*metadata.received_normalized_head(), summary);
        strip_forwarding_headers(&mut parts.headers);
        let stripped =
            validate_normalized_ingress_parts_with_summary(&parts, IngressHeadLimits::default())
                .unwrap();
        assert!(summary.header_bytes() > stripped.header_bytes());
        assert_eq!(summary.header_count(), stripped.header_count() + 1);
        assert_eq!(*metadata.received_normalized_head(), summary);
        parts.extensions.insert(metadata);
        // A caller can replace its public extension with another request's copy.
        parts.extensions.insert(NativeIngressMetadata::new(
            other_guard.request_id(),
            checked,
            summary,
        ));
        assert_ne!(
            parts
                .extensions
                .get::<NativeIngressMetadata>()
                .unwrap()
                .request_id(),
            authoritative
        );
        guard.finish_ingress(IngressDisposition::Aborted);
        let records = lock(&observer.records);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request_id, authoritative);
        let safe = SafeRecord(&records[0]).to_string();
        assert!(!safe.contains("SECRET"));
        assert!(!safe.contains("example"));
        drop(records);
        drop(other_guard);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn cancellation_unwind_releases_counts_even_if_observer_also_panics() {
        let observer = RecordingObserver {
            panic: true,
            ..RecordingObserver::default()
        };
        let session = session(
            observer
                .handle
                .clone()
                .with_request_observer(observer.clone()),
            false,
        );
        let (activity, connection) = session.connection();
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _guard = request(&session, Some(activity));
            panic!("cancel managed request fixture");
        }));
        assert!(unwind.is_err());
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot {
                active_requests: 0,
                active_connections: 1,
                idle_connections: 1
            }
        );
        assert_eq!(lock(&observer.records).len(), 1);
        drop(connection);
        assert_eq!(
            session.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn snapshots_remain_coherent_during_connection_request_updates() {
        let handle = NativeDiagnosticsHandle::new();
        let session = session(handle.clone(), false);
        let finished = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = repeat_with(|| {
            let worker_session = session.clone();
            let worker_finished = Arc::clone(&finished);
            thread::spawn(move || {
                for _ in 0_usize..128 {
                    let (activity, connection) = worker_session.connection();
                    let first = request(&worker_session, Some(activity.clone()));
                    let second = request(&worker_session, Some(activity));
                    drop(first);
                    drop(connection);
                    drop(second);
                }
                worker_finished.fetch_add(1, Ordering::Release);
            })
        })
        .take(4)
        .collect();
        while finished.load(Ordering::Acquire) < 4 {
            let snapshot = handle.snapshot();
            assert!(snapshot.idle_connections <= snapshot.active_connections);
            if snapshot.active_requests == 0 {
                assert_eq!(snapshot.idle_connections, snapshot.active_connections);
            }
            thread::yield_now();
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(handle.snapshot(), NativeDiagnosticsSnapshot::default());
    }

    #[test]
    fn metric_saturation_and_underflow_cannot_panic_during_guard_ownership() {
        let saturated = session(NativeDiagnosticsHandle::new(), false);
        // Inject impossible-scale telemetry to exercise arithmetic boundaries,
        // without changing the checked identity sequence or real guard count.
        *lock(&saturated.handle.counters) = NativeDiagnosticsSnapshot {
            active_requests: u64::MAX,
            active_connections: u64::MAX,
            idle_connections: u64::MAX,
        };
        let (_, saturated_connection) = saturated.connection();
        let saturated_request = request(&saturated, None);
        assert_eq!(saturated.handle.snapshot().active_requests, u64::MAX);
        assert_eq!(saturated.handle.snapshot().active_connections, u64::MAX);
        assert_eq!(saturated.handle.snapshot().idle_connections, u64::MAX);
        drop(saturated_request);
        drop(saturated_connection);
        assert_eq!(saturated.handle.snapshot().active_requests, u64::MAX - 1);
        assert_eq!(saturated.handle.snapshot().active_connections, u64::MAX - 1);
        assert_eq!(saturated.handle.snapshot().idle_connections, u64::MAX - 1);

        let underflow = session(NativeDiagnosticsHandle::new(), false);
        let (activity, connection) = underflow.connection();
        let guard = request(&underflow, Some(activity));
        *lock(&underflow.handle.counters) = NativeDiagnosticsSnapshot::default();
        drop(guard);
        drop(connection);
        assert_eq!(
            underflow.handle.snapshot(),
            NativeDiagnosticsSnapshot::default()
        );
    }

    #[test]
    fn route_template_output_is_escaped_for_one_line() {
        let (session, observer) = observed_session(false);
        drop(request(&session, None));
        let mut record = lock(&observer.records)[0].clone();
        record.route = "/orders/{id}\n\r\t\"\\".into();
        let rendered = SafeRecord(&record).to_string();
        assert!(!rendered.contains(['\n', '\r', '\t']));
        assert!(rendered.contains("/orders/{id}\\n\\r\\t\\\"\\\\"));
    }
}
