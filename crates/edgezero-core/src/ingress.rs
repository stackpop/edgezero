use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::config_store::ConfigExtractionLimits;
use crate::error::EdgeError;
use crate::http::{
    Extensions, HeaderMap, Method, Request, RequestParts, Response, Uri, Version,
    header::{CONTENT_LENGTH, TRANSFER_ENCODING},
};
use crate::response_egress::ResponseEgressEnvelope;
use crate::router::{ResolvedDispatch, RouteMetadata, RouteResolution};
use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicClock, MonotonicInstant};

pub const DEFAULT_INBOUND_READ_BUDGET: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_REQUEST_HEADER_BYTES: u64 = 0x0001_0000;
pub const DEFAULT_MAX_REQUEST_HEADER_COUNT: u64 = 100;
pub const DEFAULT_MAX_REQUEST_TARGET_BYTES: u64 = 8_192;

/// Opaque application-owned lease transferred to one admitted request.
pub struct IngressGrant {
    value: Option<Box<dyn Any + Send + Sync>>,
}

impl IngressGrant {
    /// Extracts the application-owned value without exposing its type metadata.
    ///
    /// # Errors
    /// Returns the unchanged grant when it is empty or stores a different type.
    #[inline]
    pub fn downcast<T>(mut self) -> Result<T, Self>
    where
        T: Send + Sync + 'static,
    {
        let Some(boxed) = self.value.take() else {
            return Err(self);
        };
        match boxed.downcast::<T>() {
            Ok(typed) => Ok(*typed),
            Err(original) => {
                self.value = Some(original);
                Err(self)
            }
        }
    }

    #[must_use]
    #[inline]
    pub fn empty() -> Self {
        Self { value: None }
    }

    #[must_use]
    #[inline]
    pub fn new<T>(value: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            value: Some(Box::new(value)),
        }
    }
}

impl fmt::Debug for IngressGrant {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngressGrant")
            .field("occupied", &self.value.is_some())
            .finish()
    }
}

/// Application decision made before an inbound body can be consumed.
#[non_exhaustive]
pub enum AdmissionDecision {
    Admit {
        grant: IngressGrant,
        read_deadline: Deadline,
    },
    /// Drains an unmatched or wrong-method request body before returning the canonical
    /// pre-resolved 404/405 response.
    ReadBodyBeforeFallback {
        max_body_bytes: usize,
        read_deadline: Deadline,
    },
    Refuse(Response),
}

enum IngressDispatchDisposition {
    Dispatch,
    ReadBodyBeforeFallback { max_body_bytes: usize },
}

/// Validated policy outcome consumed by an adapter before native body ownership moves.
///
/// `Admitted` includes both normal routed dispatch and an opt-in bounded fallback drain.
#[non_exhaustive]
pub enum IngressAdmissionOutcome {
    Admitted(AdmittedIngress),
    Refused(Response),
}

/// Result of route resolution plus application admission before native body ownership moves.
#[non_exhaustive]
pub enum IngressBeginOutcome {
    Admitted(PreparedIngress),
    Refused(ResponseEgressEnvelope),
}

/// Proof that the application selected one request disposition with a finite read deadline.
pub struct AdmittedIngress {
    config_extraction_limits: ConfigExtractionLimits,
    dispatch_disposition: IngressDispatchDisposition,
    grant: IngressGrant,
    monotonic_clock: MonotonicClock,
    read_deadline: Deadline,
    request_start: MonotonicInstant,
}

impl AdmittedIngress {
    pub(crate) fn fallback_body_limit(&self) -> Option<usize> {
        match self.dispatch_disposition {
            IngressDispatchDisposition::Dispatch => None,
            IngressDispatchDisposition::ReadBodyBeforeFallback { max_body_bytes } => {
                Some(max_body_bytes)
            }
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        MonotonicInstant,
        Deadline,
        IngressGrant,
        ConfigExtractionLimits,
        MonotonicClock,
    ) {
        (
            self.request_start,
            self.read_deadline,
            self.grant,
            self.config_extraction_limits,
            self.monotonic_clock,
        )
    }

    #[must_use]
    #[inline]
    pub fn monotonic_clock(&self) -> MonotonicClock {
        self.monotonic_clock.clone()
    }

    #[must_use]
    #[inline]
    pub fn read_deadline(&self) -> Deadline {
        self.read_deadline
    }

    #[must_use]
    #[inline]
    pub fn request_start(&self) -> MonotonicInstant {
        self.request_start
    }

    pub(crate) fn with_config_extraction_limits(mut self, limits: ConfigExtractionLimits) -> Self {
        self.config_extraction_limits = limits;
        self
    }
}

/// Whether request-head accounting was enforced before normalized request construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IngressHeadAccounting {
    HostManaged,
    RawValidated {
        request_header_bytes: u64,
        request_header_count: u64,
        request_target_bytes: u64,
    },
}

/// Trusted framing result, or an explicit marker that the host owns validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IngressFraming {
    Chunked,
    ContentLength(u64),
    HostManaged,
    NoBody,
    ProtocolManaged,
}

/// Finite request-head limits installed before an adapter begins serving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IngressHeadLimits {
    header_bytes: u64,
    header_count: u64,
    target_bytes: u64,
}

impl IngressHeadLimits {
    #[must_use]
    #[inline]
    pub fn max_request_header_bytes(&self) -> u64 {
        self.header_bytes
    }

    #[must_use]
    #[inline]
    pub fn max_request_header_count(&self) -> u64 {
        self.header_count
    }

    #[must_use]
    #[inline]
    pub fn max_request_target_bytes(&self) -> u64 {
        self.target_bytes
    }

    /// # Errors
    /// Returns an internal startup-policy error when `max` is zero.
    #[inline]
    pub fn with_max_request_header_bytes(mut self, max: u64) -> Result<Self, EdgeError> {
        require_nonzero("max_request_header_bytes", max)?;
        self.header_bytes = max;
        Ok(self)
    }

    /// # Errors
    /// Returns an internal startup-policy error when `max` is zero.
    #[inline]
    pub fn with_max_request_header_count(mut self, max: u64) -> Result<Self, EdgeError> {
        require_nonzero("max_request_header_count", max)?;
        self.header_count = max;
        Ok(self)
    }

    /// # Errors
    /// Returns an internal startup-policy error when `max` is zero.
    #[inline]
    pub fn with_max_request_target_bytes(mut self, max: u64) -> Result<Self, EdgeError> {
        require_nonzero("max_request_target_bytes", max)?;
        self.target_bytes = max;
        Ok(self)
    }
}

impl Default for IngressHeadLimits {
    #[inline]
    fn default() -> Self {
        Self {
            header_bytes: DEFAULT_MAX_REQUEST_HEADER_BYTES,
            header_count: DEFAULT_MAX_REQUEST_HEADER_COUNT,
            target_bytes: DEFAULT_MAX_REQUEST_TARGET_BYTES,
        }
    }
}

/// Normalized, body-free request metadata supplied by an adapter before admission.
pub struct IngressHeadParts {
    extensions: Extensions,
    framing: IngressFraming,
    head_accounting: IngressHeadAccounting,
    headers: HeaderMap,
    method: Method,
    target: Uri,
    version: Version,
}

impl IngressHeadParts {
    /// Copies normalized metadata from body-free request parts.
    #[must_use]
    #[inline]
    pub fn from_parts(
        parts: &RequestParts,
        head_accounting: IngressHeadAccounting,
        framing: IngressFraming,
    ) -> Self {
        Self {
            extensions: parts.extensions.clone(),
            framing,
            head_accounting,
            headers: parts.headers.clone(),
            method: parts.method.clone(),
            target: parts.uri.clone(),
            version: parts.version,
        }
    }

    /// Copies normalized metadata from a core request without inspecting its body.
    #[must_use]
    #[inline]
    pub fn from_request(
        request: &Request,
        head_accounting: IngressHeadAccounting,
        framing: IngressFraming,
    ) -> Self {
        Self {
            extensions: request.extensions().clone(),
            framing,
            head_accounting,
            headers: request.headers().clone(),
            method: request.method().clone(),
            target: request.uri().clone(),
            version: request.version(),
        }
    }

    pub(crate) fn into_head(
        self,
        request_start: MonotonicInstant,
        route_resolution: RouteResolution,
    ) -> IngressHead {
        IngressHead {
            extensions: self.extensions,
            framing: self.framing,
            head_accounting: self.head_accounting,
            headers: self.headers,
            method: self.method,
            request_start,
            route_resolution,
            target: self.target,
            version: self.version,
        }
    }

    #[must_use]
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Creates host-managed request-head metadata. Adapters with a raw parser boundary replace
    /// the accounting and framing markers through the builder methods below.
    #[must_use]
    #[inline]
    pub fn new(method: Method, target: Uri, version: Version, headers: HeaderMap) -> Self {
        Self {
            extensions: Extensions::new(),
            framing: IngressFraming::HostManaged,
            head_accounting: IngressHeadAccounting::HostManaged,
            headers,
            method,
            target,
            version,
        }
    }

    #[must_use]
    #[inline]
    pub fn target(&self) -> &Uri {
        &self.target
    }

    /// Applies normalized defense-in-depth checks without making a raw-boundary claim.
    ///
    /// # Errors
    /// Returns 414/431 for normalized head overages and 400 for visible ambiguous framing.
    #[inline]
    pub fn validate_normalized(&self, limits: IngressHeadLimits) -> Result<(), EdgeError> {
        validate_normalized_head(&self.target, self.version, &self.headers, limits)
    }

    #[must_use]
    #[inline]
    pub fn with_extension<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.extensions.insert(value);
        self
    }

    #[must_use]
    #[inline]
    pub fn with_framing(mut self, framing: IngressFraming) -> Self {
        self.framing = framing;
        self
    }

    #[must_use]
    #[inline]
    pub fn with_head_accounting(mut self, head_accounting: IngressHeadAccounting) -> Self {
        self.head_accounting = head_accounting;
        self
    }
}

/// Immutable request-head view supplied to the application admission policy.
pub struct IngressHead {
    extensions: Extensions,
    framing: IngressFraming,
    head_accounting: IngressHeadAccounting,
    headers: HeaderMap,
    method: Method,
    request_start: MonotonicInstant,
    route_resolution: RouteResolution,
    target: Uri,
    version: Version,
}

impl IngressHead {
    #[must_use]
    #[inline]
    pub fn extension<T>(&self) -> Option<&T>
    where
        T: Send + Sync + 'static,
    {
        self.extensions.get::<T>()
    }

    #[must_use]
    #[inline]
    pub fn framing(&self) -> IngressFraming {
        self.framing
    }

    /// Builds the body-blind admission view from a normalized core request.
    #[must_use]
    #[inline]
    pub fn from_request(
        request: &Request,
        request_start: MonotonicInstant,
        route_resolution: RouteResolution,
        head_accounting: IngressHeadAccounting,
        framing: IngressFraming,
    ) -> Self {
        IngressHeadParts::from_request(request, head_accounting, framing)
            .into_head(request_start, route_resolution)
    }

    #[must_use]
    #[inline]
    pub fn head_accounting(&self) -> IngressHeadAccounting {
        self.head_accounting
    }

    #[must_use]
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[must_use]
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Builds a finite deadline relative to this head's captured request start.
    ///
    /// The duration is clamped to the configured far-future bound. Arithmetic
    /// overflow fails closed by returning a deadline at the request start.
    #[must_use]
    #[inline]
    pub fn read_deadline_after(&self, duration: Duration) -> Deadline {
        let bounded = duration.min(DEADLINE_FAR_FUTURE);
        Deadline::at_instant(
            self.request_start
                .checked_add(bounded)
                .unwrap_or(self.request_start),
        )
    }

    #[must_use]
    #[inline]
    pub fn request_start(&self) -> MonotonicInstant {
        self.request_start
    }

    #[must_use]
    #[inline]
    pub fn route_resolution(&self) -> &RouteResolution {
        &self.route_resolution
    }

    #[must_use]
    #[inline]
    pub fn target(&self) -> &Uri {
        &self.target
    }

    #[must_use]
    #[inline]
    pub fn version(&self) -> Version {
        self.version
    }

    #[must_use]
    #[inline]
    pub fn with_extension<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.extensions.insert(value);
        self
    }
}

/// Opaque, single-use admission proof paired with the exact resolved dispatch token.
pub struct PreparedIngress {
    admitted: AdmittedIngress,
    resolved: ResolvedDispatch,
}

impl PreparedIngress {
    pub(crate) fn into_parts(self) -> (ResolvedDispatch, AdmittedIngress) {
        (self.resolved, self.admitted)
    }

    #[must_use]
    #[inline]
    pub fn monotonic_clock(&self) -> MonotonicClock {
        self.admitted.monotonic_clock()
    }

    pub(crate) fn new(resolved: ResolvedDispatch, admitted: AdmittedIngress) -> Self {
        Self { admitted, resolved }
    }

    #[must_use]
    #[inline]
    pub fn read_deadline(&self) -> Deadline {
        self.admitted.read_deadline()
    }

    #[must_use]
    #[inline]
    pub fn request_start(&self) -> MonotonicInstant {
        self.admitted.request_start()
    }

    /// Canonical metadata for the admitted matched route, if any.
    #[must_use]
    #[inline]
    pub fn route_metadata(&self) -> Option<&RouteMetadata> {
        match self.resolved.resolution() {
            RouteResolution::Matched(route) => Some(route),
            RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound => None,
        }
    }

    #[must_use]
    #[inline]
    pub fn route_resolution(&self) -> &RouteResolution {
        self.resolved.resolution()
    }
}

pub(crate) type IngressAdmissionPolicy =
    Arc<dyn Fn(&IngressHead) -> AdmissionDecision + Send + Sync>;

pub(crate) fn default_admission_policy() -> IngressAdmissionPolicy {
    Arc::new(|head| AdmissionDecision::Admit {
        grant: IngressGrant::empty(),
        read_deadline: head.read_deadline_after(DEFAULT_INBOUND_READ_BUDGET),
    })
}

pub(crate) fn apply_admission_policy(
    policy: &IngressAdmissionPolicy,
    head: &IngressHead,
    monotonic_clock: MonotonicClock,
) -> Result<IngressAdmissionOutcome, EdgeError> {
    let (grant, read_deadline, dispatch_disposition) = match policy(head) {
        AdmissionDecision::Refuse(response) => {
            return Ok(IngressAdmissionOutcome::Refused(response));
        }
        AdmissionDecision::Admit {
            grant,
            read_deadline,
        } => (grant, read_deadline, IngressDispatchDisposition::Dispatch),
        AdmissionDecision::ReadBodyBeforeFallback {
            max_body_bytes,
            read_deadline,
        } => {
            match head.route_resolution() {
                RouteResolution::MethodNotAllowed { .. } | RouteResolution::NotFound => {}
                RouteResolution::Matched(_) => {
                    return Err(EdgeError::internal(anyhow::anyhow!(
                        "fallback body policy requires an unmatched or wrong-method route"
                    )));
                }
            }
            (
                IngressGrant::empty(),
                read_deadline,
                IngressDispatchDisposition::ReadBodyBeforeFallback { max_body_bytes },
            )
        }
    };
    let maximum = head
        .request_start()
        .checked_add(DEADLINE_FAR_FUTURE)
        .ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "ingress admission deadline arithmetic overflow"
            ))
        })?;
    let deadline = Deadline::at_instant(read_deadline.instant().min(maximum));
    Ok(IngressAdmissionOutcome::Admitted(AdmittedIngress {
        config_extraction_limits: ConfigExtractionLimits::default(),
        dispatch_disposition,
        grant,
        monotonic_clock,
        read_deadline: deadline,
        request_start: head.request_start(),
    }))
}

fn require_nonzero(name: &str, value: u64) -> Result<(), EdgeError> {
    if value == 0 {
        return Err(EdgeError::internal(anyhow::anyhow!(
            "ingress head limit `{name}` must be nonzero"
        )));
    }
    Ok(())
}

/// Applies finite defense-in-depth limits and framing checks to a normalized head.
///
/// This cannot recover raw HTTP field-line or request-target octets and therefore never
/// upgrades [`IngressHeadAccounting::HostManaged`] or [`IngressFraming::HostManaged`].
///
/// # Errors
/// Returns 414/431 for normalized head overages and 400 for visible ambiguous framing.
#[inline]
pub fn validate_normalized_ingress_parts(
    parts: &RequestParts,
    limits: IngressHeadLimits,
) -> Result<(), EdgeError> {
    validate_normalized_head(&parts.uri, parts.version, &parts.headers, limits)
}

fn validate_normalized_head(
    target: &Uri,
    version: Version,
    headers: &HeaderMap,
    limits: IngressHeadLimits,
) -> Result<(), EdgeError> {
    let target_bytes = u64::try_from(target.to_string().len()).map_err(|_length_error| {
        EdgeError::uri_too_long("normalized request target is too large")
    })?;
    if target_bytes > limits.max_request_target_bytes() {
        return Err(EdgeError::uri_too_long(
            "normalized request target exceeds configured limit",
        ));
    }

    let header_count = u64::try_from(headers.len()).map_err(|_length_error| {
        EdgeError::request_header_fields_too_large("normalized request has too many headers")
    })?;
    if header_count > limits.max_request_header_count() {
        return Err(EdgeError::request_header_fields_too_large(
            "normalized request header count exceeds configured limit",
        ));
    }

    let mut header_bytes = 2_u64;
    for (name, value) in headers {
        let line_bytes = name
            .as_str()
            .len()
            .checked_add(value.as_bytes().len())
            .and_then(|bytes| bytes.checked_add(4))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| {
                EdgeError::request_header_fields_too_large(
                    "normalized request header size accounting overflow",
                )
            })?;
        header_bytes = header_bytes.checked_add(line_bytes).ok_or_else(|| {
            EdgeError::request_header_fields_too_large(
                "normalized request header size accounting overflow",
            )
        })?;
        if header_bytes > limits.max_request_header_bytes() {
            return Err(EdgeError::request_header_fields_too_large(
                "normalized request headers exceed configured limit",
            ));
        }
    }

    validate_normalized_framing(version, headers)
}

fn validate_normalized_framing(version: Version, headers: &HeaderMap) -> Result<(), EdgeError> {
    let content_lengths = headers.get_all(CONTENT_LENGTH).iter().collect::<Vec<_>>();
    let transfer_encodings = headers
        .get_all(TRANSFER_ENCODING)
        .iter()
        .collect::<Vec<_>>();

    if !content_lengths.is_empty() && !transfer_encodings.is_empty() {
        return Err(EdgeError::bad_request(
            "ambiguous request framing is not accepted",
        ));
    }
    if content_lengths.len() > 1 {
        return Err(EdgeError::bad_request(
            "duplicate content-length is not accepted",
        ));
    }
    if let Some(value) = content_lengths.first() {
        let raw = value
            .to_str()
            .map_err(|_utf8_error| EdgeError::bad_request("malformed content-length"))?
            .trim();
        if raw.is_empty()
            || raw.contains(',')
            || raw.starts_with(['+', '-'])
            || raw.parse::<u64>().is_err()
        {
            return Err(EdgeError::bad_request("malformed content-length"));
        }
    }

    if transfer_encodings.len() > 1 {
        return Err(EdgeError::bad_request(
            "duplicate transfer-encoding is not accepted",
        ));
    }
    if let Some(value) = transfer_encodings.first() {
        if version != Version::HTTP_11 {
            return Err(EdgeError::bad_request(
                "transfer-encoding is not accepted for this HTTP version",
            ));
        }
        let raw = value
            .to_str()
            .map_err(|_utf8_error| EdgeError::bad_request("malformed transfer-encoding"))?;
        if raw.contains(',') || !raw.trim().eq_ignore_ascii_case("chunked") {
            return Err(EdgeError::bad_request(
                "unsupported or malformed transfer-encoding",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::{HeaderName, HeaderValue, StatusCode, request_builder, response_builder};

    fn parts_with_headers(version: Version, headers: &[(&str, &str)]) -> RequestParts {
        let mut request = request_builder()
            .uri("/")
            .version(version)
            .body(Body::empty())
            .expect("request");
        for (name, value) in headers {
            request.headers_mut().append(
                name.parse::<HeaderName>().expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        request.into_parts().0
    }

    #[test]
    fn ingress_grant_downcasts_once_without_exposing_type_metadata() {
        let string_grant = IngressGrant::new(String::from("lease"));
        let retained_grant = string_grant.downcast::<u64>().expect_err("wrong type");
        assert_eq!(
            retained_grant.downcast::<String>().expect("right type"),
            "lease"
        );
        IngressGrant::empty()
            .downcast::<String>()
            .expect_err("empty grant");
    }

    #[test]
    fn ingress_head_limits_are_finite_nonzero_and_independently_configurable() {
        let limits = IngressHeadLimits::default();
        assert_eq!(limits.max_request_header_bytes(), 0x0001_0000);
        assert_eq!(limits.max_request_header_count(), 100);
        assert_eq!(limits.max_request_target_bytes(), 8_192);
        limits
            .with_max_request_header_bytes(0)
            .expect_err("zero header bytes");
        limits
            .with_max_request_header_count(0)
            .expect_err("zero header count");
        limits
            .with_max_request_target_bytes(0)
            .expect_err("zero target bytes");
    }

    #[test]
    fn normalized_head_limits_reject_before_admission_without_raw_claims() {
        let target_request = request_builder()
            .uri("/1234")
            .body(Body::empty())
            .expect("request");
        let target_parts = target_request.into_parts().0;
        let target_limits = IngressHeadLimits::default()
            .with_max_request_target_bytes(4)
            .expect("target limit");
        assert_eq!(
            validate_normalized_ingress_parts(&target_parts, target_limits)
                .expect_err("target over limit")
                .status(),
            StatusCode::URI_TOO_LONG
        );

        let count_parts = parts_with_headers(Version::HTTP_11, &[("x-a", "1"), ("x-b", "2")]);
        let count_limits = IngressHeadLimits::default()
            .with_max_request_header_count(1)
            .expect("count limit");
        assert_eq!(
            validate_normalized_ingress_parts(&count_parts, count_limits)
                .expect_err("header count over limit")
                .status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );

        let bytes_parts = parts_with_headers(Version::HTTP_11, &[("x", "1")]);
        let bytes_limits = IngressHeadLimits::default()
            .with_max_request_header_bytes(7)
            .expect("bytes limit");
        assert_eq!(
            validate_normalized_ingress_parts(&bytes_parts, bytes_limits)
                .expect_err("header bytes over limit")
                .status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
    }

    #[test]
    fn normalized_framing_defense_rejects_visible_ambiguous_shapes() {
        let cases = [
            parts_with_headers(
                Version::HTTP_11,
                &[("content-length", "1"), ("transfer-encoding", "chunked")],
            ),
            parts_with_headers(
                Version::HTTP_11,
                &[("content-length", "1"), ("content-length", "1")],
            ),
            parts_with_headers(Version::HTTP_11, &[("content-length", "1, 1")]),
            parts_with_headers(Version::HTTP_11, &[("content-length", "+1")]),
            parts_with_headers(
                Version::HTTP_11,
                &[("content-length", "18446744073709551616")],
            ),
            parts_with_headers(Version::HTTP_11, &[("transfer-encoding", "gzip, chunked")]),
            parts_with_headers(Version::HTTP_2, &[("transfer-encoding", "chunked")]),
        ];
        for parts in cases {
            assert_eq!(
                validate_normalized_ingress_parts(&parts, IngressHeadLimits::default())
                    .expect_err("ambiguous framing")
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }

        for parts in [
            parts_with_headers(Version::HTTP_11, &[("content-length", "1")]),
            parts_with_headers(Version::HTTP_11, &[("transfer-encoding", "chunked")]),
        ] {
            validate_normalized_ingress_parts(&parts, IngressHeadLimits::default())
                .expect("unambiguous visible framing");
        }
    }

    #[test]
    fn admission_clamps_deadline_and_preserves_refusal() {
        let start = MonotonicInstant::now();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let head = IngressHead::from_request(
            &request,
            start,
            RouteResolution::NotFound,
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        );
        let too_late = Deadline::at_instant(
            start
                .checked_add(DEADLINE_FAR_FUTURE + Duration::from_secs(1))
                .expect("deadline"),
        );
        let admit_policy: IngressAdmissionPolicy = Arc::new(move |_| AdmissionDecision::Admit {
            grant: IngressGrant::empty(),
            read_deadline: too_late,
        });
        let IngressAdmissionOutcome::Admitted(admitted) =
            apply_admission_policy(&admit_policy, &head, MonotonicClock::default())
                .expect("admission")
        else {
            panic!("expected admission");
        };
        assert_eq!(
            admitted.read_deadline().instant(),
            start.checked_add(DEADLINE_FAR_FUTURE).expect("maximum")
        );

        let refuse_policy: IngressAdmissionPolicy = Arc::new(|_| {
            AdmissionDecision::Refuse(
                response_builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::empty())
                    .expect("response"),
            )
        });
        let IngressAdmissionOutcome::Refused(response) =
            apply_admission_policy(&refuse_policy, &head, MonotonicClock::default())
                .expect("refusal")
        else {
            panic!("expected refusal");
        };
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn ingress_head_builds_relative_deadlines_in_its_start_clock_domain() {
        let global = MonotonicInstant::now();
        let request_start = global
            .checked_add(Duration::from_hours(24))
            .expect("offset request start");
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let head = IngressHead::from_request(
            &request,
            request_start,
            RouteResolution::NotFound,
            IngressHeadAccounting::HostManaged,
            IngressFraming::HostManaged,
        );

        assert_eq!(
            head.read_deadline_after(Duration::from_secs(2)).instant(),
            request_start
                .checked_add(Duration::from_secs(2))
                .expect("relative deadline")
        );
    }
}
