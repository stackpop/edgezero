//! Confidential adapter-owned ingress facts and explicit preservation limits.
//!
//! Metadata describes what a runtime exposes; it cannot recover information
//! erased before conversion. It is intentionally neither serializable nor
//! printable because captured targets and authorities can contain private data.

use std::net::Ipv6Addr;
use std::sync::Arc;

use crate::error::EdgeError;
use crate::http::{HeaderName, Uri};

/// Maximum complete target metadata, measured in UTF-8 bytes.
pub const MAX_TARGET_BYTES: usize = 16 * 1024;

/// A complete bounded target or an explicit acquisition failure.
#[derive(Clone)]
pub enum CapturedTarget {
    /// A complete value created only by the bounded capture constructor.
    Complete(CompleteTarget),
    /// No complete target is available.
    Unavailable(TargetUnavailable),
}

impl CapturedTarget {
    /// Capture a whole runtime target without retaining an oversized prefix.
    #[inline]
    #[must_use]
    pub fn capture(value: &str, source: TargetSource, fidelity: Preservation) -> Self {
        if value.len() > MAX_TARGET_BYTES {
            return Self::Unavailable(TargetUnavailable::TooLarge);
        }
        Self::Complete(CompleteTarget {
            fidelity,
            source,
            value: Arc::from(value),
        })
    }
}

/// A complete bounded target with explicit provenance.
///
/// Fields remain private so callers cannot bypass the capture bound.
///
/// ```compile_fail
/// use edgezero_core::request::{CompleteTarget, Preservation, TargetSource};
/// let target = CompleteTarget {
///     value: std::sync::Arc::from("http://example.com/"),
///     source: TargetSource::RuntimeUrl,
///     fidelity: Preservation::Unknown,
/// };
/// ```
#[derive(Clone)]
pub struct CompleteTarget {
    fidelity: Preservation,
    source: TargetSource,
    value: Arc<str>,
}

impl CompleteTarget {
    /// Return the established target preservation guarantee.
    #[inline]
    #[must_use]
    pub const fn fidelity(&self) -> Preservation {
        self.fidelity
    }

    /// Return the runtime acquisition source.
    #[inline]
    #[must_use]
    pub const fn source(&self) -> TargetSource {
        self.source
    }

    /// Borrow the complete captured target.
    #[inline]
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Independent original-field preservation facts.
///
/// These axes compare original HTTP field values with core headers; successful
/// runtime-to-core copying alone does not establish original-wire preservation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeaderFidelity {
    field_multiplicity: Preservation,
    global_field_order: Preservation,
    octets: Preservation,
    same_name_order: Preservation,
}

impl HeaderFidelity {
    /// Return original field-count preservation for this header.
    #[inline]
    #[must_use]
    pub const fn field_multiplicity(&self) -> Preservation {
        self.field_multiplicity
    }

    /// Return original ordering across different field names.
    #[inline]
    #[must_use]
    pub const fn global_field_order(&self) -> Preservation {
        self.global_field_order
    }

    /// Construct four independent fidelity guarantees.
    #[inline]
    #[must_use]
    pub const fn new(
        octets: Preservation,
        field_multiplicity: Preservation,
        same_name_order: Preservation,
        global_field_order: Preservation,
    ) -> Self {
        Self {
            field_multiplicity,
            global_field_order,
            octets,
            same_name_order,
        }
    }

    /// Return original field-value byte preservation.
    #[inline]
    #[must_use]
    pub const fn octets(&self) -> Preservation {
        self.octets
    }

    /// Return original value ordering within a single field name.
    #[inline]
    #[must_use]
    pub const fn same_name_order(&self) -> Preservation {
        self.same_name_order
    }
}

/// Bounded validated origin facts supplied by a trusted adapter source.
///
/// Syntax validation does not make a forwarded header trustworthy. Adapters
/// must establish provenance before calling the constructor.
#[derive(Clone)]
pub struct InboundOrigin {
    authority: Arc<str>,
    scheme: &'static str,
    source: OriginSource,
}

impl InboundOrigin {
    /// Borrow the validated authority, including any explicit port.
    #[inline]
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Validate a bounded HTTP(S) scheme and authority.
    ///
    /// # Errors
    ///
    /// Returns a bounded validation error for invalid or oversized input.
    #[inline]
    pub fn parse(scheme: &str, authority: &str, source: OriginSource) -> Result<Self, EdgeError> {
        let normalized_scheme = if scheme.eq_ignore_ascii_case("http") {
            "http"
        } else if scheme.eq_ignore_ascii_case("https") {
            "https"
        } else {
            return Err(EdgeError::bad_request("invalid ingress origin"));
        };
        if authority.is_empty()
            || authority.len() > 1024
            || authority
                .chars()
                .any(|character| matches!(character, '@' | '/' | '?' | '#' | '\\'))
        {
            return Err(EdgeError::bad_request("invalid ingress origin"));
        }
        let uri = Uri::builder()
            .scheme(normalized_scheme)
            .authority(authority)
            .path_and_query("/")
            .build()
            .map_err(|_error| EdgeError::bad_request("invalid ingress origin"))?;
        let parsed = uri
            .authority()
            .ok_or_else(|| EdgeError::bad_request("invalid ingress origin"))?;
        let host = parsed.host();
        if host.chars().any(|character| matches!(character, '[' | ']'))
            && host
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .is_none_or(|inner| inner.parse::<Ipv6Addr>().is_err())
        {
            return Err(EdgeError::bad_request("invalid ingress origin"));
        }
        let suffix = authority
            .strip_prefix(parsed.host())
            .ok_or_else(|| EdgeError::bad_request("invalid ingress origin"))?;
        if parsed.host().is_empty()
            || (!suffix.is_empty()
                && suffix.strip_prefix(':').is_none_or(|port| {
                    port.is_empty()
                        || !port.bytes().all(|byte| byte.is_ascii_digit())
                        || port.parse::<u16>().is_err()
                }))
        {
            return Err(EdgeError::bad_request("invalid ingress origin"));
        }
        Ok(Self {
            authority: Arc::from(authority),
            scheme: normalized_scheme,
            source,
        })
    }

    /// Return the normalized HTTP(S) scheme.
    #[inline]
    #[must_use]
    pub const fn scheme(&self) -> &str {
        self.scheme
    }

    /// Return the adapter-owned origin acquisition source.
    #[inline]
    #[must_use]
    pub const fn source(&self) -> OriginSource {
        self.source
    }
}

/// Provenance of a validated inbound origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginSource {
    /// Scheme and authority supplied by the runtime request URI.
    RuntimeUri,
    /// A trusted transport scheme combined with one validated inbound request authority.
    ///
    /// The authority identifies the requested origin; it does not establish
    /// that an operator approves or owns that host.
    TransportBinding,
}

/// Degree of preservation established for a particular fact.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Preservation {
    /// Pinned source and transport evidence establish equality.
    Preserved,
    /// A known transformation changed the original information.
    Transformed,
    /// The runtime does not expose the required information.
    Unavailable,
    /// Preservation has not been established.
    #[default]
    Unknown,
}

/// Confidential adapter-owned metadata stored in request extensions.
///
/// Absence means unknown capability, not complete preservation. Existing
/// headers remain in the request `HeaderMap` and are never duplicated here.
#[derive(Clone)]
pub struct RequestIngress {
    common_header_fidelity: HeaderFidelity,
    origin: Option<InboundOrigin>,
    overrides: Vec<(HeaderName, HeaderFidelity)>,
    target: CapturedTarget,
}

impl RequestIngress {
    /// Return a header-specific guarantee or the common conservative default.
    #[inline]
    #[must_use]
    pub fn header_fidelity(&self, name: &HeaderName) -> HeaderFidelity {
        self.overrides
            .iter()
            .find_map(|(candidate, fidelity)| (candidate == name).then_some(*fidelity))
            .unwrap_or(self.common_header_fidelity)
    }

    /// Construct metadata with unambiguous per-header fidelity overrides.
    ///
    /// Sources and preservation guarantees are caller assertions. Validation
    /// checks override names, not the provenance or fidelity of captured facts.
    ///
    /// # Errors
    ///
    /// Returns a bounded validation error for duplicate override names.
    #[inline]
    pub fn new(
        target: CapturedTarget,
        origin: Option<InboundOrigin>,
        common_header_fidelity: HeaderFidelity,
        mut overrides: Vec<(HeaderName, HeaderFidelity)>,
    ) -> Result<Self, EdgeError> {
        overrides.sort_unstable_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        if overrides.windows(2).any(|pair| {
            let [left, right] = pair else { return false };
            left.0 == right.0
        }) {
            return Err(EdgeError::bad_request("duplicate ingress header override"));
        }
        Ok(Self {
            common_header_fidelity,
            origin,
            overrides,
            target,
        })
    }

    /// Borrow the validated origin, when its trusted source is available.
    #[inline]
    #[must_use]
    pub const fn origin(&self) -> Option<&InboundOrigin> {
        self.origin.as_ref()
    }

    /// Borrow complete target metadata or its explicit unavailability reason.
    #[inline]
    #[must_use]
    pub const fn target(&self) -> &CapturedTarget {
        &self.target
    }
}

/// Source of captured target information, independent of fidelity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetSource {
    /// A runtime path/query before additional `EdgeZero` parsing.
    RuntimePathAndQuery,
    /// A runtime-exposed URL; earlier normalization may have occurred.
    RuntimeUrl,
    /// A transport-exposed request target with evidence of its provenance.
    TransportRequestTarget,
}

/// Why no complete target was retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetUnavailable {
    /// The runtime offers no safe acquisition API.
    NotExposed,
    /// Acquisition failed without retaining error text.
    ReadFailed,
    /// The target exceeded the metadata bound.
    TooLarge,
}

#[cfg(test)]
mod tests {
    use crate::http::header::{CONTENT_LENGTH, COOKIE};
    use crate::request::{
        CapturedTarget, HeaderFidelity, InboundOrigin, OriginSource, Preservation, RequestIngress,
        TargetSource, TargetUnavailable,
    };

    #[test]
    fn ingress_fidelity_defaults_are_unknown() {
        let fidelity = HeaderFidelity::default();
        assert_eq!(fidelity.octets(), Preservation::Unknown);
        assert_eq!(fidelity.field_multiplicity(), Preservation::Unknown);
        assert_eq!(fidelity.same_name_order(), Preservation::Unknown);
        assert_eq!(fidelity.global_field_order(), Preservation::Unknown);
    }

    #[test]
    fn ingress_header_override_is_field_specific() {
        let common = HeaderFidelity::default();
        let folded = HeaderFidelity::new(
            Preservation::Unknown,
            Preservation::Transformed,
            Preservation::Unknown,
            Preservation::Unavailable,
        );
        let metadata = RequestIngress::new(
            CapturedTarget::Unavailable(TargetUnavailable::NotExposed),
            None,
            common,
            vec![(CONTENT_LENGTH, folded)],
        )
        .expect("should accept distinct header overrides");
        assert_eq!(metadata.header_fidelity(&CONTENT_LENGTH), folded);
        assert_eq!(metadata.header_fidelity(&COOKIE), common);
        assert!(metadata.origin().is_none());
    }

    #[test]
    fn ingress_origin_accepts_bounded_http_authorities() {
        for (scheme, authority) in [
            ("HTTP", "example.com"),
            ("https", "example.com:443"),
            ("http", "[::1]:8787"),
        ] {
            let origin = InboundOrigin::parse(scheme, authority, OriginSource::TransportBinding)
                .expect("should accept a validated HTTP origin");
            assert_eq!(origin.scheme(), scheme.to_ascii_lowercase());
            assert_eq!(origin.authority(), authority);
            assert_eq!(origin.source(), OriginSource::TransportBinding);
        }
    }

    #[test]
    fn ingress_origin_rejects_untrusted_or_invalid_input() {
        for (scheme, authority) in [
            ("ftp", "example.com"),
            ("https", "user@example.com"),
            ("https", "example.com/path"),
            ("https", "example.com?secret=1"),
            ("https", "example.com#fragment"),
            ("https", "example.com:invalid"),
            ("https", "example.com:65536"),
            ("https", "example.com:+443"),
            ("https", "[::1]:+443"),
            ("https", "[]"),
            ("https", "[not-an-ip]"),
            ("https", "example[bad]"),
            ("https", ""),
            ("https", "example.com\\evil"),
        ] {
            assert!(
                InboundOrigin::parse(scheme, authority, OriginSource::RuntimeUri)
                    .err()
                    .is_some(),
                "should reject malformed authority without retaining it",
            );
        }
        assert!(
            InboundOrigin::parse("https", &"a".repeat(1025), OriginSource::RuntimeUri)
                .err()
                .is_some(),
            "should bound origin metadata",
        );
    }

    #[test]
    fn ingress_rejects_duplicate_overrides() {
        let result = RequestIngress::new(
            CapturedTarget::Unavailable(TargetUnavailable::ReadFailed),
            None,
            HeaderFidelity::default(),
            vec![
                (COOKIE, HeaderFidelity::default()),
                (COOKIE, HeaderFidelity::default()),
            ],
        );
        assert!(
            result.err().is_some(),
            "should reject ambiguous override selection"
        );
    }

    #[test]
    fn ingress_target_bound_is_atomic() {
        let mut value = "a".repeat(16 * 1024);
        let target =
            CapturedTarget::capture(&value, TargetSource::RuntimeUrl, Preservation::Transformed);
        assert!(
            matches!(&target, CapturedTarget::Complete(complete) if complete.value() == value),
            "should preserve the entire value at the bound",
        );
        value.push('b');
        assert!(
            matches!(
                CapturedTarget::capture(&value, TargetSource::RuntimeUrl, Preservation::Unknown,),
                CapturedTarget::Unavailable(TargetUnavailable::TooLarge),
            ),
            "should retain no truncated prefix",
        );
    }

    #[test]
    fn ingress_target_counts_utf8_bytes() {
        let value = "\u{e9}".repeat(8193);
        assert!(
            matches!(
                CapturedTarget::capture(&value, TargetSource::RuntimeUrl, Preservation::Unknown,),
                CapturedTarget::Unavailable(TargetUnavailable::TooLarge),
            ),
            "should bound bytes rather than Unicode characters",
        );
    }
}
