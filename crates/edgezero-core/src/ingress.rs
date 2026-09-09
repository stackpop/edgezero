use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::config_store::ConfigExtractionLimits;
use crate::error::EdgeError;
use crate::http::{Extensions, HeaderMap, Method, Request, Response, Uri, Version};
use crate::router::{ResolvedDispatch, RouteResolution};
use crate::time::{DEADLINE_FAR_FUTURE, Deadline, MonotonicInstant};

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
    Refuse(Response),
}

/// Validated admission result consumed by an adapter.
#[non_exhaustive]
pub enum IngressAdmissionOutcome {
    Admitted(AdmittedIngress),
    Refused(Response),
}

/// Result of route resolution plus application admission before native body ownership moves.
#[non_exhaustive]
pub enum IngressBeginOutcome {
    Admitted(PreparedIngress),
    Refused(Response),
}

/// Proof that the application admitted one request with a finite read deadline.
pub struct AdmittedIngress {
    config_extraction_limits: ConfigExtractionLimits,
    grant: IngressGrant,
    read_deadline: Deadline,
    request_start: MonotonicInstant,
}

impl AdmittedIngress {
    pub(crate) fn into_parts(
        self,
    ) -> (
        MonotonicInstant,
        Deadline,
        IngressGrant,
        ConfigExtractionLimits,
    ) {
        (
            self.request_start,
            self.read_deadline,
            self.grant,
            self.config_extraction_limits,
        )
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
        read_deadline: Deadline::at_instant(
            head.request_start()
                .checked_add(DEFAULT_INBOUND_READ_BUDGET)
                .unwrap_or(head.request_start()),
        ),
    })
}

pub(crate) fn apply_admission_policy(
    policy: &IngressAdmissionPolicy,
    head: &IngressHead,
) -> Result<IngressAdmissionOutcome, EdgeError> {
    match policy(head) {
        AdmissionDecision::Refuse(response) => Ok(IngressAdmissionOutcome::Refused(response)),
        AdmissionDecision::Admit {
            grant,
            read_deadline,
        } => {
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
                grant,
                read_deadline: deadline,
                request_start: head.request_start(),
            }))
        }
    }
}

fn require_nonzero(name: &str, value: u64) -> Result<(), EdgeError> {
    if value == 0 {
        return Err(EdgeError::internal(anyhow::anyhow!(
            "ingress head limit `{name}` must be nonzero"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::{StatusCode, request_builder, response_builder};

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
            apply_admission_policy(&admit_policy, &head).expect("admission")
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
            apply_admission_policy(&refuse_policy, &head).expect("refusal")
        else {
            panic!("expected refusal");
        };
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
