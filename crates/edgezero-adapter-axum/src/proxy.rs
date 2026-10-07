//! Checked incoming-proxy assertions for the managed native socket boundary.

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::from_utf8;

use edgezero_core::http::{Authority, HeaderMap, HeaderName, RequestParts};
use edgezero_core::ingress::{CheckedEffectiveHost, CheckedHostSource};
use ipnet::{IpNet, Ipv4Net};

const MAX_BYTES: usize = 0x4000;
const MAX_CHAIN_SLOTS: usize = 64;
const MAX_NETWORKS: usize = 256;
const MAX_PARAMETER_SLOTS: usize = 16;
const X_HEADERS: [&str; 3] = ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto"];

/// The sole forwarding assertion family selected by an operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardingHeaderFamily {
    Forwarded,
    XForwarded,
}

/// Invalid trust configuration. Formatting never includes supplied values.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ProxyTrustConfigError;

impl fmt::Display for ProxyTrustConfigError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid_trusted_proxy_configuration")
    }
}

impl fmt::Debug for ProxyTrustConfigError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[expect(
    clippy::missing_trait_methods,
    reason = "the source-free safe category uses Error defaults; remaining methods are deprecated or unstable"
)]
impl Error for ProxyTrustConfigError {}

/// Immutable, bounded literal-network policy. Empty policies never enable trust.
#[derive(Clone, Eq, PartialEq)]
pub struct TrustedProxyPolicy {
    family: Option<ForwardingHeaderFamily>,
    networks: Vec<IpNet>,
}

impl fmt::Debug for TrustedProxyPolicy {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedProxyPolicy")
            .field("family", &self.family)
            .field("network_count", &self.networks.len())
            .finish()
    }
}

impl Default for TrustedProxyPolicy {
    #[inline]
    fn default() -> Self {
        Self::no_trust()
    }
}

impl TrustedProxyPolicy {
    /// Parses captured standalone settings. Unknown supplied families always fail.
    ///
    /// # Errors
    /// Rejects invalid settings and nonempty networks without a selected family.
    #[inline]
    pub fn from_values(
        cidrs: Option<&str>,
        family: Option<&str>,
    ) -> Result<Self, ProxyTrustConfigError> {
        let selected_family = family
            .map(|value| match value.trim_matches([' ', '\t']) {
                "forwarded" => Ok(ForwardingHeaderFamily::Forwarded),
                "x-forwarded" => Ok(ForwardingHeaderFamily::XForwarded),
                _ => Err(ProxyTrustConfigError),
            })
            .transpose()?;
        let raw = cidrs.unwrap_or("");
        if raw.len() > MAX_BYTES
            || raw
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(ProxyTrustConfigError);
        }
        if raw.trim_matches([' ', '\t']).is_empty() {
            return Ok(Self {
                family: selected_family,
                networks: Vec::new(),
            });
        }
        Self::new(
            selected_family.ok_or(ProxyTrustConfigError)?,
            raw.split(','),
        )
    }

    /// Validates literal IPs/CIDRs without exposing the network-library type.
    ///
    /// # Errors
    /// Rejects invalid literals, parser-budget excesses and whole-family trust.
    #[inline]
    pub fn new<I, S>(
        family: ForwardingHeaderFamily,
        networks: I,
    ) -> Result<Self, ProxyTrustConfigError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized = Vec::new();
        let mut bytes = 0_usize;
        for (index, entry) in networks.into_iter().enumerate() {
            if index >= MAX_NETWORKS {
                return Err(ProxyTrustConfigError);
            }
            let raw = entry.as_ref();
            bytes = bytes
                .checked_add(raw.len())
                .and_then(|size| size.checked_add(usize::from(index != 0)))
                .filter(|size| *size <= MAX_BYTES)
                .ok_or(ProxyTrustConfigError)?;
            if raw
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
            {
                return Err(ProxyTrustConfigError);
            }
            let literal = raw.trim_matches([' ', '\t']);
            let parsed = literal
                .parse::<IpNet>()
                .or_else(|_error| literal.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_error| ProxyTrustConfigError)?;
            let network = normalize_network(parsed)?;
            if !normalized.contains(&network) {
                normalized.push(network);
            }
        }
        if covers_family(&normalized, false) || covers_family(&normalized, true) {
            return Err(ProxyTrustConfigError);
        }
        Ok(Self {
            family: Some(family),
            networks: normalized,
        })
    }

    #[must_use]
    #[inline]
    pub fn no_trust() -> Self {
        Self {
            family: None,
            networks: Vec::new(),
        }
    }

    fn trusts(&self, address: IpAddr) -> bool {
        let canonical = canonical_ip(address);
        self.networks
            .iter()
            .any(|network| network.contains(&canonical))
    }
}

/// A bounded assertion outcome, never a raw header or source error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardingResult {
    Absent,
    Accepted,
    Malformed,
    Partial,
    UnresolvedChain,
    UntrustedPeer,
}

impl ForwardingResult {
    #[must_use]
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Accepted => "accepted",
            Self::Partial => "partial",
            Self::UntrustedPeer => "untrusted_peer",
            Self::Malformed => "malformed",
            Self::UnresolvedChain => "unresolved_chain",
        }
    }
}

/// The checked public scheme, independent of the client-address chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    #[must_use]
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// Checked ingress facts, not host authorization or authenticated visitor identity.
#[derive(Clone)]
pub struct CheckedProxyMetadata {
    client_source: CheckedHostSource,
    direct_peer: Option<SocketAddr>,
    effective_client: Option<IpAddr>,
    effective_host: CheckedEffectiveHost,
    effective_scheme: Scheme,
    forwarding_result: ForwardingResult,
    scheme_source: CheckedHostSource,
}

impl fmt::Debug for CheckedProxyMetadata {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedProxyMetadata")
            .field("forwarding_result", &self.forwarding_result)
            .finish_non_exhaustive()
    }
}

impl CheckedProxyMetadata {
    #[must_use]
    #[inline]
    pub fn client_source(&self) -> CheckedHostSource {
        self.client_source
    }

    #[must_use]
    #[inline]
    pub fn direct_peer(&self) -> Option<SocketAddr> {
        self.direct_peer
    }

    #[must_use]
    #[inline]
    pub fn effective_client(&self) -> Option<IpAddr> {
        self.effective_client
    }

    #[must_use]
    #[inline]
    pub fn effective_host(&self) -> &CheckedEffectiveHost {
        &self.effective_host
    }

    #[must_use]
    #[inline]
    pub fn effective_scheme(&self) -> Scheme {
        self.effective_scheme
    }

    #[must_use]
    #[inline]
    pub fn forwarding_result(&self) -> ForwardingResult {
        self.forwarding_result
    }

    #[must_use]
    #[inline]
    pub fn scheme_source(&self) -> CheckedHostSource {
        self.scheme_source
    }
}

#[derive(Default)]
struct ParsedAssertion {
    host: Option<Authority>,
    nodes: Vec<Option<IpAddr>>,
    scheme: Option<Scheme>,
}

fn canonical_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

fn normalize_network(network: IpNet) -> Result<IpNet, ProxyTrustConfigError> {
    let truncated = network.trunc();
    if truncated.prefix_len() == 0 {
        return Err(ProxyTrustConfigError);
    }
    if let IpNet::V6(v6) = truncated {
        let mapped_start = Ipv6Addr::from(0xffff_u128 << 32_u32);
        if v6.prefix_len() < 96 && v6.contains(&mapped_start) {
            // A shorter overlapping prefix contains every mapped IPv4 address.
            return Err(ProxyTrustConfigError);
        }
        if let Some(v4) = v6.network().to_ipv4_mapped() {
            let prefix = v6
                .prefix_len()
                .checked_sub(96)
                .ok_or(ProxyTrustConfigError)?;
            if prefix == 0 {
                return Err(ProxyTrustConfigError);
            }
            return Ipv4Net::new(v4, prefix)
                .map(IpNet::V4)
                .map_err(|_error| ProxyTrustConfigError);
        }
    }
    Ok(truncated)
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "the maximum endpoint returns before end + 1; IPv4 endpoints fit u128 and the IPv6 maximum is handled explicitly"
)]
fn covers_family(networks: &[IpNet], ipv6: bool) -> bool {
    let mut intervals: Vec<(u128, u128)> = networks
        .iter()
        .filter_map(|network| match network {
            IpNet::V4(v4) if !ipv6 => Some((
                u128::from(u32::from(v4.network())),
                u128::from(u32::from(v4.broadcast())),
            )),
            IpNet::V6(v6) if ipv6 => Some((u128::from(v6.network()), u128::from(v6.broadcast()))),
            IpNet::V4(_) | IpNet::V6(_) => None,
        })
        .collect();
    intervals.sort_unstable();
    let maximum = if ipv6 {
        u128::MAX
    } else {
        u128::from(u32::MAX)
    };
    let mut next = 0_u128;
    for (start, end) in intervals {
        if start > next {
            return false;
        }
        if end == maximum {
            return true;
        }
        next = next.max(end + 1);
    }
    false
}

pub(crate) fn normalize(
    policy: &TrustedProxyPolicy,
    peer: Option<SocketAddr>,
    parts: &RequestParts,
) -> CheckedProxyMetadata {
    let direct_authority = match parts.uri.authority() {
        Some(authority) => parse_authority(authority.as_str().as_bytes()).ok(),
        None => parts
            .headers
            .get("host")
            .and_then(|value| parse_authority(value.as_bytes()).ok()),
    };
    let host_source = if direct_authority.is_some() {
        CheckedHostSource::Direct
    } else {
        CheckedHostSource::Unavailable
    };
    let mut metadata = CheckedProxyMetadata {
        direct_peer: peer,
        effective_client: peer.map(|socket| canonical_ip(socket.ip())),
        effective_host: CheckedEffectiveHost::from_ingress(direct_authority, host_source),
        effective_scheme: Scheme::Http,
        client_source: if peer.is_some() {
            CheckedHostSource::Direct
        } else {
            CheckedHostSource::Unavailable
        },
        scheme_source: CheckedHostSource::Direct,
        forwarding_result: ForwardingResult::Absent,
    };
    let present = match policy.family {
        Some(ForwardingHeaderFamily::Forwarded) => parts.headers.contains_key("forwarded"),
        Some(ForwardingHeaderFamily::XForwarded) => X_HEADERS
            .iter()
            .any(|name| parts.headers.contains_key(*name)),
        None => parts.headers.keys().any(is_forwarding_header),
    };
    if !present {
        return metadata;
    }
    if !peer.is_some_and(|socket| policy.trusts(socket.ip())) {
        metadata.forwarding_result = ForwardingResult::UntrustedPeer;
        return metadata;
    }
    let (parse_result, source) = match policy.family {
        Some(ForwardingHeaderFamily::Forwarded) => (
            parse_forwarded(&parts.headers),
            CheckedHostSource::TrustedForwarded,
        ),
        Some(ForwardingHeaderFamily::XForwarded) => (
            parse_x_forwarded(&parts.headers),
            CheckedHostSource::TrustedXForwarded,
        ),
        None => return metadata,
    };
    let Ok(parsed) = parse_result else {
        metadata.forwarding_result = ForwardingResult::Malformed;
        return metadata;
    };
    let mut accepted_client = false;
    let mut unresolved = false;
    if !parsed.nodes.is_empty() {
        unresolved = true;
        for predecessor in parsed.nodes.iter().rev() {
            let Some(address) = predecessor else {
                break;
            };
            if !policy.trusts(*address) {
                metadata.effective_client = Some(*address);
                metadata.client_source = source;
                accepted_client = true;
                unresolved = false;
                break;
            }
        }
    }
    let accepted_host = parsed.host.is_some();
    let accepted_scheme = parsed.scheme.is_some();
    if let Some(host) = parsed.host {
        metadata.effective_host = CheckedEffectiveHost::from_ingress(Some(host), source);
    }
    if let Some(scheme) = parsed.scheme {
        metadata.effective_scheme = scheme;
        metadata.scheme_source = source;
    }
    metadata.forwarding_result = if unresolved {
        ForwardingResult::UnresolvedChain
    } else if accepted_client && accepted_host && accepted_scheme {
        ForwardingResult::Accepted
    } else if accepted_client || accepted_host || accepted_scheme {
        ForwardingResult::Partial
    } else {
        ForwardingResult::Absent
    };
    metadata
}

fn is_forwarding_header(name: &HeaderName) -> bool {
    name.as_str() == "forwarded" || name.as_str().starts_with("x-forwarded-")
}

/// Removes all raw forwarding assertions, including unselected extensions.
pub(crate) fn strip_forwarding_headers(headers: &mut HeaderMap) {
    while let Some(name) = headers
        .keys()
        .find(|name| is_forwarding_header(name))
        .cloned()
    {
        headers.remove(name);
    }
}

// Input is preflighted before any parsing scratch or retained nodes are allocated.
fn preflight(headers: &HeaderMap, names: &[&str]) -> Result<(), ()> {
    let mut bytes = 0_usize;
    for name in names {
        for (index, value) in headers.get_all(*name).iter().enumerate() {
            bytes = bytes
                .checked_add(value.as_bytes().len())
                .and_then(|size| size.checked_add(usize::from(index != 0)))
                .filter(|size| *size <= MAX_BYTES)
                .ok_or(())?;
        }
    }
    Ok(())
}

#[expect(
    clippy::indexing_slicing,
    reason = "both endpoints are positions found in this slice, and first non-OWS cannot follow last non-OWS"
)]
fn trim_ows(value: &[u8]) -> &[u8] {
    let Some(start) = value.iter().position(|byte| !matches!(byte, b' ' | b'\t')) else {
        return &[];
    };
    let Some(end) = value.iter().rposition(|byte| !matches!(byte, b' ' | b'\t')) else {
        return &[];
    };
    &value[start..=end]
}

// Slots include empty members; repeated-line boundaries join rather than add slots.
#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "private callers cap slots at 64 or 16; delimiter indices come from the same preflighted slice and start advances at most to its length"
)]
fn split_slots<'input>(
    value: &'input [u8],
    delimiter: u8,
    slots: &mut usize,
    limit: usize,
    quoting: bool,
) -> Result<Vec<&'input [u8]>, ()> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    *slots += 1;
    if *slots > limit {
        return Err(());
    }
    for (index, byte) in value.iter().copied().enumerate() {
        if quoting && escaped {
            escaped = false;
        } else if quoting && quoted && byte == b'\\' {
            escaped = true;
        } else if quoting && byte == b'"' {
            quoted = !quoted;
        } else if !quoted && byte == delimiter {
            *slots += 1;
            if *slots > limit {
                return Err(());
            }
            result.push(&value[start..index]);
            start = index + 1;
        } else {
            // An ordinary byte does not change delimiter/quoting state.
        }
    }
    if quoted || escaped {
        return Err(());
    }
    result.push(&value[start..]);
    Ok(result)
}

fn token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

// Unknown extensions are validated without allocating or retaining their payload.
#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "the quoted branch checks length >= 2 before excluding the two quote bytes"
)]
fn validate_value(value: &[u8]) -> Result<bool, ()> {
    if value.first() != Some(&b'"') {
        return if !value.is_empty() && value.iter().copied().all(token_byte) {
            Ok(false)
        } else {
            Err(())
        };
    }
    if value.len() < 2 || value.last() != Some(&b'"') {
        return Err(());
    }
    let mut escaped = false;
    for byte in value[1..value.len() - 1].iter().copied() {
        if escaped {
            if !(byte == b'\t' || byte == b' ' || (0x21..=0x7e).contains(&byte) || byte >= 0x80) {
                return Err(());
            }
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if !(byte == b'\t'
            || byte == b' '
            || byte == 0x21
            || (0x23..=0x5b).contains(&byte)
            || (0x5d..=0x7e).contains(&byte)
            || byte >= 0x80)
        {
            return Err(());
        } else {
            // RFC qdtext, including obs-text, requires no state change.
        }
    }
    if escaped { Err(()) } else { Ok(true) }
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "only called after validate_value; quoted input has two validated delimiters and fits the assertion byte budget"
)]
fn decode_value(value: &[u8], quoted: bool) -> Vec<u8> {
    if !quoted {
        return value.to_vec();
    }
    let mut decoded = Vec::with_capacity(value.len() - 2);
    let mut escaped = false;
    for byte in value[1..value.len() - 1].iter().copied() {
        if !escaped && byte == b'\\' {
            escaped = true;
        } else {
            decoded.push(byte);
            escaped = false;
        }
    }
    decoded
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "equals is a position found within the same nonempty parameter slice, so both name/value ranges and equals + 1 are bounded"
)]
fn parse_forwarded(headers: &HeaderMap) -> Result<ParsedAssertion, ()> {
    preflight(headers, &["forwarded"])?;
    let mut parsed = ParsedAssertion::default();
    let mut slots = 0;
    for field in headers.get_all("forwarded") {
        for raw_element in split_slots(field.as_bytes(), b',', &mut slots, MAX_CHAIN_SLOTS, true)? {
            let element = trim_ows(raw_element);
            if element.is_empty() {
                continue;
            }
            let mut parameter_slots = 0;
            let mut names: Vec<&[u8]> = Vec::new();
            let mut node = None;
            let mut host = None;
            let mut scheme = None;
            for raw_parameter in split_slots(
                element,
                b';',
                &mut parameter_slots,
                MAX_PARAMETER_SLOTS,
                true,
            )? {
                let parameter = trim_ows(raw_parameter);
                if parameter.is_empty() {
                    continue;
                }
                let equals = parameter.iter().position(|byte| *byte == b'=').ok_or(())?;
                let name = &parameter[..equals];
                if name.is_empty()
                    || !name.iter().copied().all(token_byte)
                    || names
                        .iter()
                        .any(|previous| previous.eq_ignore_ascii_case(name))
                {
                    return Err(());
                }
                names.push(name);
                let value = &parameter[equals + 1..];
                let quoted = validate_value(value)?;
                if name.eq_ignore_ascii_case(b"for") || name.eq_ignore_ascii_case(b"by") {
                    let decoded = decode_value(value, quoted);
                    let address = parse_node(&decoded, false)?;
                    if name.eq_ignore_ascii_case(b"for") {
                        node = address;
                    }
                } else if name.eq_ignore_ascii_case(b"host") {
                    host = Some(parse_authority(&decode_value(value, quoted))?);
                } else if name.eq_ignore_ascii_case(b"proto") {
                    scheme = Some(parse_scheme(&decode_value(value, quoted))?);
                } else {
                    // Unknown payloads are syntactically checked, never retained.
                }
            }
            if names.is_empty() {
                continue;
            }
            parsed.nodes.push(node);
            // A new substantive element replaces, rather than borrows, final-hop facts.
            parsed.host = host;
            parsed.scheme = scheme;
        }
    }
    Ok(parsed)
}

fn singleton<'headers>(
    headers: &'headers HeaderMap,
    name: &str,
) -> Result<Option<&'headers [u8]>, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().map(|field| trim_ows(field.as_bytes()));
    if values.next().is_some() || value.is_some_and(|bytes| bytes.contains(&b',')) {
        return Err(());
    }
    Ok(value)
}

fn parse_x_forwarded(headers: &HeaderMap) -> Result<ParsedAssertion, ()> {
    preflight(headers, &X_HEADERS)?;
    let mut parsed = ParsedAssertion::default();
    let mut slots = 0;
    for value in headers.get_all("x-forwarded-for") {
        for node in split_slots(value.as_bytes(), b',', &mut slots, MAX_CHAIN_SLOTS, false)? {
            // XFF has no RFC empty-list tolerance: never skip a predecessor slot.
            parsed.nodes.push(parse_node(trim_ows(node), true)?);
        }
    }
    parsed.host = singleton(headers, "x-forwarded-host")?
        .map(parse_authority)
        .transpose()?;
    parsed.scheme = singleton(headers, "x-forwarded-proto")?
        .map(parse_scheme)
        .transpose()?;
    Ok(parsed)
}

fn parse_scheme(value: &[u8]) -> Result<Scheme, ()> {
    if value.eq_ignore_ascii_case(b"http") {
        Ok(Scheme::Http)
    } else if value.eq_ignore_ascii_case(b"https") {
        Ok(Scheme::Https)
    } else {
        Err(())
    }
}

fn valid_obfuscated(value: &str) -> bool {
    value.starts_with('_')
        && value.len() > 1
        && value
            .as_bytes()
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
}

fn validate_node_port(port: &str) -> Result<(), ()> {
    if valid_obfuscated(port)
        || (!port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok())
    {
        Ok(())
    } else {
        Err(())
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::string_slice,
    reason = "input is checked ASCII before slicing; closing is the bracket position found in that same string and its successor is at most the string length"
)]
fn parse_node(bytes: &[u8], allow_bare_ipv6: bool) -> Result<Option<IpAddr>, ()> {
    let value = from_utf8(bytes).map_err(|_error| ())?;
    if value.is_empty() || !value.is_ascii() {
        return Err(());
    }
    if let Some(bracketed) = value.strip_prefix('[') {
        let closing = bracketed.find(']').ok_or(())?;
        let address = bracketed[..closing]
            .parse::<Ipv6Addr>()
            .map_err(|_error| ())?;
        let remainder = &bracketed[closing + 1..];
        if !remainder.is_empty() {
            validate_node_port(remainder.strip_prefix(':').ok_or(())?)?;
        }
        return Ok(Some(canonical_ip(IpAddr::V6(address))));
    }
    if allow_bare_ipv6 && let Ok(address) = value.parse::<Ipv6Addr>() {
        return Ok(Some(canonical_ip(IpAddr::V6(address))));
    }
    let (node, optional_port) = value
        .split_once(':')
        .map_or((value, None), |(name, number)| (name, Some(number)));
    if let Some(port) = optional_port {
        validate_node_port(port)?;
    }
    if node.eq_ignore_ascii_case("unknown") || valid_obfuscated(node) {
        return Ok(None);
    }
    node.parse::<Ipv4Addr>()
        .map(|address| Some(IpAddr::V4(address)))
        .map_err(|_error| ())
}

#[expect(
    clippy::arithmetic_side_effects,
    clippy::string_slice,
    reason = "input is checked ASCII before slicing; closing is found in the same bracketed authority and closing + 1 cannot exceed its length"
)]
fn parse_authority(bytes: &[u8]) -> Result<Authority, ()> {
    let value = from_utf8(bytes).map_err(|_error| ())?;
    if value.is_empty()
        || !value.is_ascii()
        || value.bytes().any(|byte| {
            byte.is_ascii_whitespace() || byte.is_ascii_control() || b"@/?#\\,".contains(&byte)
        })
    {
        return Err(());
    }
    let authority = value.parse::<Authority>().map_err(|_error| ())?;
    if authority.host().is_empty() {
        return Err(());
    }
    let optional_port = if let Some(bracketed) = value.strip_prefix('[') {
        let closing = bracketed.find(']').ok_or(())?;
        bracketed[..closing]
            .parse::<Ipv6Addr>()
            .map_err(|_error| ())?;
        let suffix = &bracketed[closing + 1..];
        if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or(())?)
        }
    } else {
        let mut components = value.split(':');
        let host = components.next().ok_or(())?;
        if host.is_empty() || host.contains(['[', ']']) {
            return Err(());
        }
        let port = components.next();
        if components.next().is_some() {
            return Err(());
        }
        port
    };
    if let Some(port) = optional_port
        && (port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().is_err())
    {
        return Err(());
    }
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::iter::{empty, repeat, repeat_n};

    use super::*;
    use edgezero_core::body::Body;
    use edgezero_core::http::{HeaderName, HeaderValue, request_builder};

    const PEER: &str = "10.0.0.2:1234";
    const CLIENT: &str = "198.51.100.7";

    fn policy(family: ForwardingHeaderFamily) -> TrustedProxyPolicy {
        TrustedProxyPolicy::new(family, ["10.0.0.0/24"]).unwrap()
    }

    fn parts(headers: &[(&str, &str)]) -> RequestParts {
        let mut request = request_builder().uri("/").body(Body::empty()).unwrap();
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("direct.example:8080"));
        for (name, value) in headers {
            request.headers_mut().append(
                name.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        request.into_parts().0
    }

    fn checked(family: ForwardingHeaderFamily, headers: &[(&str, &str)]) -> CheckedProxyMetadata {
        normalize(
            &policy(family),
            Some(PEER.parse().unwrap()),
            &parts(headers),
        )
    }

    fn direct_fallback(metadata: &CheckedProxyMetadata, result: ForwardingResult) {
        assert_eq!(metadata.forwarding_result(), result);
        assert_eq!(metadata.direct_peer(), Some(PEER.parse().unwrap()));
        assert_eq!(
            metadata.effective_client(),
            Some("10.0.0.2".parse().unwrap())
        );
        assert_eq!(metadata.client_source(), CheckedHostSource::Direct);
        assert_eq!(
            metadata.effective_host().authority(),
            Some("direct.example:8080")
        );
        assert_eq!(
            metadata.effective_host().source(),
            CheckedHostSource::Direct
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Http);
        assert_eq!(metadata.scheme_source(), CheckedHostSource::Direct);
    }

    fn assert_client(metadata: &CheckedProxyMetadata, client: &str) {
        assert_eq!(metadata.effective_client(), Some(client.parse().unwrap()));
    }

    #[test]
    fn defaults_and_strict_configuration() {
        let default_policy = TrustedProxyPolicy::default();
        assert_eq!(default_policy, TrustedProxyPolicy::no_trust());
        assert!(!default_policy.trusts("127.0.0.1".parse().unwrap()));
        assert_eq!(
            TrustedProxyPolicy::from_values(None, None).unwrap(),
            default_policy
        );
        for raw in ["", " ", "\t "] {
            assert!(
                TrustedProxyPolicy::from_values(Some(raw), None)
                    .unwrap()
                    .networks
                    .is_empty()
            );
        }
        for family in ["forwarded", "x-forwarded"] {
            assert!(
                TrustedProxyPolicy::from_values(None, Some(family))
                    .unwrap()
                    .networks
                    .is_empty()
            );
        }
        assert!(
            TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, empty::<&str>())
                .unwrap()
                .networks
                .is_empty()
        );
        TrustedProxyPolicy::from_values(Some("10.0.0.1"), None).unwrap_err();
        for family in ["", "auto", "Forwarded", "forwarded\n", "sentinel-secret"] {
            TrustedProxyPolicy::from_values(None, Some(family)).unwrap_err();
        }
        for cidrs in [
            "all",
            "private",
            "localhost",
            "example.com",
            "10.0.0.1,",
            ",10.0.0.1",
            "10.0.0.1,,10.0.0.2",
            "10.0.0.1/33",
            "\n",
            "10.0.0.1\r",
            "10.0.0.1\0",
        ] {
            TrustedProxyPolicy::from_values(Some(cidrs), Some("forwarded")).expect_err(cidrs);
        }
        TrustedProxyPolicy::from_values(Some(" \t10.0.0.1 , 10.0.0.2/32\t"), Some("forwarded"))
            .unwrap();
        TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, [""]).unwrap_err();
    }

    #[test]
    fn configuration_errors_are_safe() {
        let error = TrustedProxyPolicy::from_values(Some("sentinel-secret"), Some("forwarded"))
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid_trusted_proxy_configuration");
        assert_eq!(format!("{error:?}"), error.to_string());
        let policy = policy(ForwardingHeaderFamily::Forwarded);
        assert!(!format!("{policy:?}").contains("10.0.0"));
    }

    #[test]
    fn configuration_entry_and_byte_bounds_precede_deduplication() {
        let family = ForwardingHeaderFamily::Forwarded;
        assert_eq!(
            TrustedProxyPolicy::new(family, repeat_n("10.0.0.1", MAX_NETWORKS))
                .unwrap()
                .networks
                .len(),
            1
        );
        TrustedProxyPolicy::new(family, repeat("10.0.0.1")).unwrap_err();
        let exact = format!("{}127.0.0.1", " ".repeat(MAX_BYTES - "127.0.0.1".len()));
        TrustedProxyPolicy::new(family, [&exact]).unwrap();
        TrustedProxyPolicy::from_values(Some(&exact), Some("forwarded")).unwrap();
        let over = format!(" {exact}");
        TrustedProxyPolicy::new(family, [&over]).unwrap_err();
        TrustedProxyPolicy::from_values(Some(&over), Some("forwarded")).unwrap_err();
        let second = "10.0.0.2";
        let first = format!(
            "{}10.0.0.1",
            " ".repeat(MAX_BYTES - 1 - second.len() - "10.0.0.1".len())
        );
        TrustedProxyPolicy::new(family, [first.as_str(), second]).unwrap();
        TrustedProxyPolicy::new(family, [format!(" {first}"), second.to_owned()]).unwrap_err();
    }

    #[test]
    fn cidrs_truncate_deduplicate_and_project_mapped_ipv6() {
        let policy = TrustedProxyPolicy::new(
            ForwardingHeaderFamily::Forwarded,
            [
                "192.0.2.19/24",
                "192.0.2.0/24",
                "::ffff:192.0.2.99/120",
                "2001:db8::9/32",
            ],
        )
        .unwrap();
        assert_eq!(policy.networks.len(), 2);
        for address in [
            "192.0.2.0",
            "192.0.2.255",
            "::ffff:192.0.2.7",
            "2001:db8::ffff",
        ] {
            assert!(policy.trusts(address.parse().unwrap()), "{address}");
        }
        for address in [
            "192.0.3.0",
            "::ffff:192.0.3.1",
            "2001:db9::",
            "127.0.0.1",
            "10.0.0.1",
        ] {
            assert!(!policy.trusts(address.parse().unwrap()), "{address}");
        }
        let mapped =
            TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, ["::ffff:192.0.2.7"])
                .unwrap();
        assert!(mapped.trusts("192.0.2.7".parse().unwrap()));
        assert!(!mapped.trusts("192.0.2.8".parse().unwrap()));
    }

    #[test]
    fn universal_and_mapped_crossing_policies_are_rejected() {
        for entries in [
            vec!["0.0.0.0/0"],
            vec!["::/0"],
            vec!["0.0.0.0/1", "128.0.0.0/1"],
            vec!["0.0.0.0/2", "64.0.0.0/2", "128.0.0.0/1"],
            vec!["::/1", "8000::/1"],
            vec!["::ffff:0.0.0.0/96"],
            vec!["::fffe:0:0/95"],
            vec!["::/80"],
            vec!["0.0.0.0/1", "::ffff:128.0.0.0/97"],
        ] {
            TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, entries).unwrap_err();
        }
        // Exercise interval merging's maximum endpoint without enumerating addresses.
        let v6 = ["::/1".parse().unwrap(), "8000::/1".parse().unwrap()];
        assert!(covers_family(&v6, true));
        assert!(!covers_family(&["ffff::/16".parse().unwrap()], true));
        assert!(!covers_family(
            &["0.0.0.0/1".parse().unwrap(), "192.0.0.0/2".parse().unwrap()],
            false
        ));
        TrustedProxyPolicy::new(
            ForwardingHeaderFamily::Forwarded,
            ["::fffd:0:0/96", "ffff::/16"],
        )
        .unwrap();
    }

    #[test]
    #[expect(
        clippy::shadow_unrelated,
        reason = "the same checked variable describes separate missing-peer and nonmatching-peer fixture observations"
    )]
    fn absent_disabled_untrusted_and_missing_peer_never_parse_assertions() {
        let assertion = parts(&[
            ("forwarded", "invalid sentinel-secret"),
            ("x-forwarded-host", "evil.example"),
        ]);
        for policy in [
            TrustedProxyPolicy::no_trust(),
            policy(ForwardingHeaderFamily::Forwarded),
        ] {
            let peer: SocketAddr = "127.0.0.1:10".parse().unwrap();
            let checked = normalize(&policy, Some(peer), &assertion);
            assert_eq!(checked.forwarding_result(), ForwardingResult::UntrustedPeer);
            assert_eq!(checked.effective_client(), Some(peer.ip()));
            assert_eq!(
                checked.effective_host().authority(),
                Some("direct.example:8080")
            );
            let checked = normalize(&policy, None, &assertion);
            assert_eq!(checked.effective_client(), None);
            assert_eq!(checked.client_source(), CheckedHostSource::Unavailable);
            assert_eq!(checked.forwarding_result(), ForwardingResult::UntrustedPeer);
        }
        direct_fallback(
            &checked(ForwardingHeaderFamily::Forwarded, &[]),
            ForwardingResult::Absent,
        );
        let empty =
            TrustedProxyPolicy::new(ForwardingHeaderFamily::Forwarded, empty::<&str>()).unwrap();
        assert_eq!(
            normalize(&empty, Some(PEER.parse().unwrap()), &assertion).forwarding_result(),
            ForwardingResult::UntrustedPeer
        );
    }

    #[test]
    #[expect(
        clippy::shadow_unrelated,
        reason = "paired metadata observations compare Forwarded and X-Forwarded family selection with the same assertions"
    )]
    fn family_selection_never_borrows_or_validates_other_family() {
        let metadata = checked(
            ForwardingHeaderFamily::Forwarded,
            &[
                ("forwarded", "for=198.51.100.7"),
                ("x-forwarded-host", "evil.example"),
                ("x-forwarded-proto", "garbage"),
            ],
        );
        assert_client(&metadata, CLIENT);
        assert_eq!(
            metadata.effective_host().authority(),
            Some("direct.example:8080")
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Http);
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Partial);
        let metadata = checked(
            ForwardingHeaderFamily::XForwarded,
            &[
                ("forwarded", "malformed"),
                ("x-forwarded-host", "public.example"),
                ("x-forwarded-proto", "https"),
            ],
        );
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Partial);
        assert_client(&metadata, "10.0.0.2");
        assert_eq!(
            metadata.effective_host().authority(),
            Some("public.example")
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Https);
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("x-forwarded-for", CLIENT)],
            ),
            ForwardingResult::Absent,
        );
    }

    #[test]
    #[expect(
        clippy::shadow_unrelated,
        reason = "paired chain fixtures compare a final asserted host/scheme against final-hop omission without changing assertion names"
    )]
    fn repeated_forwarded_order_and_final_proxy_host_scheme() {
        let metadata = checked(
            ForwardingHeaderFamily::Forwarded,
            &[
                (
                    "forwarded",
                    "for=203.0.113.99;host=attacker.example;proto=http, for=198.51.100.7",
                ),
                ("forwarded", "for=10.0.0.1;HOST=public.example;PROTO=HTTPS"),
            ],
        );
        assert_client(&metadata, CLIENT);
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Accepted);
        assert_eq!(
            metadata.effective_host().authority(),
            Some("public.example")
        );
        assert_eq!(
            metadata.effective_host().source(),
            CheckedHostSource::TrustedForwarded
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Https);
        assert_eq!(
            metadata.scheme_source(),
            CheckedHostSource::TrustedForwarded
        );
        assert_eq!(
            metadata.client_source(),
            CheckedHostSource::TrustedForwarded
        );
        let metadata = checked(
            ForwardingHeaderFamily::Forwarded,
            &[(
                "forwarded",
                "for=198.51.100.7;host=earlier.example;proto=https, for=10.0.0.1",
            )],
        );
        assert_eq!(
            metadata.effective_host().authority(),
            Some("direct.example:8080")
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Http);
    }

    #[test]
    fn forwarded_rfc_empty_slots_and_unknown_extensions() {
        for value in [",,", "", " ; ; ", " , ; ; , "] {
            direct_fallback(
                &checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", value)]),
                ForwardingResult::Absent,
            );
        }
        let metadata = checked(
            ForwardingHeaderFamily::Forwarded,
            &[(
                "forwarded",
                " , ;for=198.51.100.7;;host=public.example;proto=https;ext=\"a,b;c\\\"d\\\\e\"; , ",
            )],
        );
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Accepted);
        assert_client(&metadata, CLIENT);
        let mut parts = parts(&[]);
        parts.headers.append(
            "forwarded",
            HeaderValue::from_bytes(
                b"for=198.51.100.7;host=public.example;proto=https;ext=\"\x80\xff\\\xfe\"",
            )
            .unwrap(),
        );
        assert_eq!(
            normalize(
                &policy(ForwardingHeaderFamily::Forwarded),
                Some(PEER.parse().unwrap()),
                &parts
            )
            .forwarding_result(),
            ForwardingResult::Accepted
        );
    }

    #[test]
    fn quoted_values_unescape_before_recognized_validation() {
        let metadata = checked(
            ForwardingHeaderFamily::Forwarded,
            &[(
                "forwarded",
                "for=\"198.51.100.7\";host=\"public\\.example:443\";proto=\"h\\ttps\"",
            )],
        );
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Accepted);
        assert_eq!(
            metadata.effective_host().authority(),
            Some("public.example:443")
        );
        for value in [
            "for=\"198.51.100.7",
            "for=198.51.100.7;ext=\"a\\\"",
            "for=198.51.100.7;ext=\"a\"x",
            "for=198.51.100.7;ext=a b",
            "for =198.51.100.7",
            "for= 198.51.100.7",
            "for=198.51.100.7;ext=",
        ] {
            direct_fallback(
                &checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", value)]),
                ForwardingResult::Malformed,
            );
        }
    }

    #[test]
    #[expect(
        clippy::shadow_unrelated,
        reason = "each node table row runs identical metadata assertions against both family-specific results"
    )]
    fn recognized_nodes_and_ports_follow_family_grammar() {
        for (value, expected) in [
            ("198.51.100.7", Some(CLIENT)),
            ("198.51.100.7:65535", Some(CLIENT)),
            ("198.51.100.7:0", Some(CLIENT)),
            ("198.51.100.7:_masked", Some(CLIENT)),
            ("[2001:db8::7]", Some("2001:db8::7")),
            ("[2001:db8::7]:443", Some("2001:db8::7")),
            ("[2001:db8::7]:_masked", Some("2001:db8::7")),
            ("[::ffff:198.51.100.7]:80", Some(CLIENT)),
            ("unknown", None),
            ("UNKNOWN:80", None),
            ("_hidden:_port", None),
        ] {
            let metadata = checked(
                ForwardingHeaderFamily::Forwarded,
                &[(
                    "forwarded",
                    &format!("for=\"{value}\";host=public.example;proto=https"),
                )],
            );
            let metadata_x = checked(
                ForwardingHeaderFamily::XForwarded,
                &[
                    ("x-forwarded-for", value),
                    ("x-forwarded-host", "public.example"),
                    ("x-forwarded-proto", "https"),
                ],
            );
            for metadata in [&metadata, &metadata_x] {
                if let Some(address) = expected {
                    assert_client(metadata, address);
                    assert_eq!(
                        metadata.forwarding_result(),
                        ForwardingResult::Accepted,
                        "{value}"
                    );
                } else {
                    assert_client(metadata, "10.0.0.2");
                    assert_eq!(
                        metadata.forwarding_result(),
                        ForwardingResult::UnresolvedChain,
                        "{value}"
                    );
                    assert_eq!(metadata.effective_scheme(), Scheme::Https);
                }
            }
        }
        let bare_v6 = checked(
            ForwardingHeaderFamily::XForwarded,
            &[("x-forwarded-for", "2001:db8::7")],
        );
        assert_client(&bare_v6, "2001:db8::7");
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", "for=\"2001:db8::7\";proto=https")],
            ),
            ForwardingResult::Malformed,
        );
        for value in [
            "",
            "example.com",
            "_",
            "_bad!",
            "unknown:",
            "198.51.100.7:65536",
            "198.51.100.7:-1",
            "198.51.100.7:+1",
            "198.51.100.7:_",
            "[2001:db8::7]:65536",
            "[fe80::1%eth0]",
            "[2001:db8::7]extra",
            "[2001:db8::7",
            "[198.51.100.7]",
            "010.0.0.1",
            "[::1]::80",
        ] {
            parse_node(value.as_bytes(), true).expect_err(value);
            parse_node(value.as_bytes(), false).expect_err(value);
        }
    }

    #[test]
    fn whole_family_validation_includes_unselected_prefix_and_by_nodes() {
        for value in [
            "for=garbage,for=198.51.100.7;host=public.example;proto=https",
            "for=198.51.100.7;by=garbage;host=public.example;proto=https",
            "for=198.51.100.7;FOR=198.51.100.8;host=public.example;proto=https",
            "for=198.51.100.7;ext=a;EXT=b;host=public.example;proto=https",
            "for=203.0.113.9;proto=ftp,for=198.51.100.7;host=public.example;proto=https",
            "for=203.0.113.9;host=\"bad.example:65536\",for=198.51.100.7;host=public.example;proto=https",
            "for=\"198.51.100.7:65536\";host=public.example;proto=https",
        ] {
            direct_fallback(
                &checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", value)]),
                ForwardingResult::Malformed,
            );
        }
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::XForwarded,
                &[
                    ("x-forwarded-for", "garbage,198.51.100.7"),
                    ("x-forwarded-host", "public.example"),
                    ("x-forwarded-proto", "https"),
                ],
            ),
            ForwardingResult::Malformed,
        );
        let valid = checked(
            ForwardingHeaderFamily::Forwarded,
            &[("forwarded", "for=198.51.100.7;by=\"[2001:db8::1]:_port\"")],
        );
        assert_client(&valid, CLIENT);
    }

    #[test]
    fn xff_empty_members_invalidate_even_good_host_and_proto() {
        for value in [
            "",
            " ",
            ",",
            ",198.51.100.7",
            "198.51.100.7,",
            "198.51.100.7,,10.0.0.1",
            "198.51.100.7, \t,10.0.0.1",
        ] {
            direct_fallback(
                &checked(
                    ForwardingHeaderFamily::XForwarded,
                    &[
                        ("x-forwarded-for", value),
                        ("x-forwarded-host", "public.example"),
                        ("x-forwarded-proto", "https"),
                    ],
                ),
                ForwardingResult::Malformed,
            );
        }
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::XForwarded,
                &[
                    ("x-forwarded-for", CLIENT),
                    ("x-forwarded-for", ""),
                    ("x-forwarded-host", "public.example"),
                    ("x-forwarded-proto", "https"),
                ],
            ),
            ForwardingResult::Malformed,
        );
        let no_xff = checked(
            ForwardingHeaderFamily::XForwarded,
            &[
                ("x-forwarded-host", "public.example"),
                ("x-forwarded-proto", "https"),
            ],
        );
        assert_eq!(no_xff.forwarding_result(), ForwardingResult::Partial);
        assert_eq!(no_xff.effective_host().authority(), Some("public.example"));
        assert_eq!(no_xff.effective_scheme(), Scheme::Https);
        assert_client(&no_xff, "10.0.0.2");
    }

    #[test]
    fn x_forwarded_host_and_proto_are_strict_singletons() {
        for headers in [
            vec![
                ("x-forwarded-for", CLIENT),
                ("x-forwarded-host", "public.example"),
                ("x-forwarded-host", "public.example"),
            ],
            vec![
                ("x-forwarded-for", CLIENT),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-proto", "https"),
            ],
            vec![
                ("x-forwarded-for", CLIENT),
                ("x-forwarded-host", "public.example,evil.example"),
            ],
            vec![
                ("x-forwarded-for", CLIENT),
                ("x-forwarded-proto", "https,http"),
            ],
            vec![("x-forwarded-for", CLIENT), ("x-forwarded-host", "")],
            vec![("x-forwarded-for", CLIENT), ("x-forwarded-proto", "")],
        ] {
            direct_fallback(
                &checked(ForwardingHeaderFamily::XForwarded, &headers),
                ForwardingResult::Malformed,
            );
        }
        let accepted = checked(
            ForwardingHeaderFamily::XForwarded,
            &[
                ("x-forwarded-for", "198.51.100.7, 10.0.0.1"),
                ("x-forwarded-host", " \tpublic.example:443 "),
                ("x-forwarded-proto", " HTTPS "),
            ],
        );
        assert_eq!(accepted.forwarding_result(), ForwardingResult::Accepted);
        assert_eq!(
            accepted.effective_host().source(),
            CheckedHostSource::TrustedXForwarded
        );
        assert_eq!(
            accepted.client_source(),
            CheckedHostSource::TrustedXForwarded
        );
        assert_eq!(
            accepted.scheme_source(),
            CheckedHostSource::TrustedXForwarded
        );
    }

    #[test]
    fn authorities_and_schemes_are_checked_before_acceptance() {
        for host in [
            "public.example",
            "public.example:0",
            "public.example:65535",
            "[2001:db8::1]",
            "[::1]:443",
            "192.0.2.1:80",
        ] {
            parse_authority(host.as_bytes()).expect(host);
        }
        for host in [
            "",
            "user@public.example",
            "public.example/path",
            "public.example?q=secret",
            "public.example#secret",
            "public.example\\path",
            " public.example",
            "public.example ",
            "public.example:",
            "public.example:65536",
            "public.example:-1",
            "public.example:_port",
            "public.example:443:80",
            "[fe80::1%eth0]",
            "[::1]:",
            "[::1]extra",
            ":443",
            "a,b",
        ] {
            parse_authority(host.as_bytes()).expect_err(host);
            let encoded_host = host.replace('\\', "\\\\").replace('"', "\\\"");
            direct_fallback(
                &checked(
                    ForwardingHeaderFamily::Forwarded,
                    &[(
                        "forwarded",
                        &format!("for=198.51.100.7;host=\"{encoded_host}\";proto=https"),
                    )],
                ),
                ForwardingResult::Malformed,
            );
        }
        for scheme in ["ftp", "https:", "wss", "", "https http"] {
            direct_fallback(
                &checked(
                    ForwardingHeaderFamily::Forwarded,
                    &[(
                        "forwarded",
                        &format!("for=198.51.100.7;host=public.example;proto=\"{scheme}\""),
                    )],
                ),
                ForwardingResult::Malformed,
            );
        }
    }

    #[test]
    fn walk_stops_at_first_untrusted_boundary_and_never_skips_gaps() {
        for (chain, expected, result) in [
            (
                "203.0.113.99,198.51.100.7,10.0.0.1",
                CLIENT,
                ForwardingResult::Partial,
            ),
            (
                "203.0.113.99,198.51.100.7",
                CLIENT,
                ForwardingResult::Partial,
            ),
            (
                "203.0.113.99,10.1.0.1",
                "10.1.0.1",
                ForwardingResult::Partial,
            ),
            (
                "198.51.100.7,unknown,10.0.0.1",
                "10.0.0.2",
                ForwardingResult::UnresolvedChain,
            ),
            (
                "198.51.100.7,_gap,10.0.0.1",
                "10.0.0.2",
                ForwardingResult::UnresolvedChain,
            ),
            ("10.0.0.1", "10.0.0.2", ForwardingResult::UnresolvedChain),
            (
                "10.0.0.1,10.0.0.3",
                "10.0.0.2",
                ForwardingResult::UnresolvedChain,
            ),
            ("unknown,198.51.100.7", CLIENT, ForwardingResult::Partial),
        ] {
            let metadata = checked(
                ForwardingHeaderFamily::XForwarded,
                &[("x-forwarded-for", chain)],
            );
            assert_client(&metadata, expected);
            assert_eq!(metadata.forwarding_result(), result, "{chain}");
        }
        let gap = checked(
            ForwardingHeaderFamily::Forwarded,
            &[(
                "forwarded",
                "for=198.51.100.7,host=public.example;proto=https",
            )],
        );
        assert_eq!(gap.forwarding_result(), ForwardingResult::UnresolvedChain);
        assert_client(&gap, "10.0.0.2");
        assert_eq!(gap.effective_host().authority(), Some("public.example"));
        assert_eq!(gap.effective_scheme(), Scheme::Https);
    }

    #[test]
    fn mapped_peer_and_asserted_client_match_equivalent_ipv4_trust() {
        let peer: SocketAddr = "[::ffff:10.0.0.2]:1234".parse().unwrap();
        let parts = parts(&[(
            "forwarded",
            "for=\"[::ffff:198.51.100.7]:_p\",for=\"[::ffff:10.0.0.1]\";host=public.example;proto=https",
        )]);
        let metadata = normalize(
            &policy(ForwardingHeaderFamily::Forwarded),
            Some(peer),
            &parts,
        );
        assert_eq!(metadata.direct_peer(), Some(peer));
        assert_client(&metadata, CLIENT);
        assert_eq!(metadata.forwarding_result(), ForwardingResult::Accepted);
    }

    #[test]
    fn forwarded_list_slot_bounds_are_exact_across_repeated_fields_and_empty_members() {
        let for_node = "for=198.51.100.7";
        let exact = repeat_n(for_node, MAX_CHAIN_SLOTS)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", &exact)])
                .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &format!("{exact},{for_node}"))],
            ),
            ForwardingResult::Malformed,
        );
        let two_nodes = format!("{for_node},{for_node}");
        let mut repeated = parts(&[]);
        for _ in 0_usize..32 {
            repeated
                .headers
                .append("forwarded", HeaderValue::from_str(&two_nodes).unwrap());
        }
        assert_eq!(
            normalize(
                &policy(ForwardingHeaderFamily::Forwarded),
                Some(PEER.parse().unwrap()),
                &repeated
            )
            .forwarding_result(),
            ForwardingResult::Partial
        );
        repeated
            .headers
            .append("forwarded", HeaderValue::from_static(""));
        direct_fallback(
            &normalize(
                &policy(ForwardingHeaderFamily::Forwarded),
                Some(PEER.parse().unwrap()),
                &repeated,
            ),
            ForwardingResult::Malformed,
        );
        let exact_empty_slots = format!("{}{for_node}", ",".repeat(63));
        assert_eq!(
            checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &exact_empty_slots)]
            )
            .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &format!(",{exact_empty_slots}"))],
            ),
            ForwardingResult::Malformed,
        );
    }

    #[test]
    fn xff_list_slot_bounds_are_exact_across_repeated_fields() {
        let exact = repeat_n(CLIENT, MAX_CHAIN_SLOTS)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            checked(
                ForwardingHeaderFamily::XForwarded,
                &[("x-forwarded-for", &exact)]
            )
            .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::XForwarded,
                &[("x-forwarded-for", &format!("{exact},{CLIENT}"))],
            ),
            ForwardingResult::Malformed,
        );
        let mut repeated = parts(&[]);
        for _ in 0_usize..32 {
            repeated.headers.append(
                "x-forwarded-for",
                HeaderValue::from_str(&format!("{CLIENT},{CLIENT}")).unwrap(),
            );
        }
        assert_eq!(
            normalize(
                &policy(ForwardingHeaderFamily::XForwarded),
                Some(PEER.parse().unwrap()),
                &repeated
            )
            .forwarding_result(),
            ForwardingResult::Partial
        );
        repeated
            .headers
            .append("x-forwarded-for", HeaderValue::from_str(CLIENT).unwrap());
        direct_fallback(
            &normalize(
                &policy(ForwardingHeaderFamily::XForwarded),
                Some(PEER.parse().unwrap()),
                &repeated,
            ),
            ForwardingResult::Malformed,
        );
    }

    #[test]
    fn parameter_slot_bounds_include_empty_parameters() {
        let exact = format!("for=198.51.100.7{}", ";".repeat(15));
        assert_eq!(
            checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", &exact)])
                .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &format!("{exact};"))],
            ),
            ForwardingResult::Malformed,
        );
        let mut named = String::from("for=198.51.100.7");
        for index in 0_usize..15 {
            write!(named, ";e{index}=value").unwrap();
        }
        assert_eq!(
            checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", &named)])
                .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &format!("{named};e15=value"))],
            ),
            ForwardingResult::Malformed,
        );
    }

    #[test]
    fn assertion_byte_budget_is_exact_and_counts_repeated_delimiters() {
        let prefix = "for=198.51.100.7;host=public.example;proto=https;ext=\"";
        let exact = format!("{prefix}{}\"", "a".repeat(MAX_BYTES - prefix.len() - 1));
        assert_eq!(
            checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", &exact)])
                .forwarding_result(),
            ForwardingResult::Accepted
        );
        let over = exact.replacen("ext=\"", "ext=\"a", 1);
        direct_fallback(
            &checked(ForwardingHeaderFamily::Forwarded, &[("forwarded", &over)]),
            ForwardingResult::Malformed,
        );
        let second = "for=198.51.100.7;host=public.example;proto=https";
        let first = format!("ext=\"{}\"", "a".repeat(MAX_BYTES - second.len() - 1 - 6));
        assert_eq!(first.len() + second.len() + 1, MAX_BYTES);
        assert_eq!(
            checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &first), ("forwarded", second)]
            )
            .forwarding_result(),
            ForwardingResult::Accepted
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::Forwarded,
                &[("forwarded", &format!(" {first}")), ("forwarded", second)],
            ),
            ForwardingResult::Malformed,
        );
        let padding = " ".repeat(MAX_BYTES - CLIENT.len());
        assert_eq!(
            checked(
                ForwardingHeaderFamily::XForwarded,
                &[("x-forwarded-for", &format!("{padding}{CLIENT}"))]
            )
            .forwarding_result(),
            ForwardingResult::Partial
        );
        direct_fallback(
            &checked(
                ForwardingHeaderFamily::XForwarded,
                &[
                    ("x-forwarded-for", &format!(" {padding}{CLIENT}")),
                    ("x-forwarded-host", "public.example"),
                ],
            ),
            ForwardingResult::Malformed,
        );
    }

    #[test]
    fn stripping_removes_every_forwarding_extension_without_changing_direct_head() {
        let mut parts = parts(&[
            ("Forwarded", "for=198.51.100.7"),
            ("X-Forwarded-For", CLIENT),
            ("x-forwarded-host", "public.example"),
            ("x-forwarded-proto", "https"),
            ("X-Forwarded-Secret", "sentinel-secret"),
            ("x-request-id", "visitor-id"),
        ]);
        let original_uri = parts.uri.clone();
        strip_forwarding_headers(&mut parts.headers);
        assert!(!parts.headers.keys().any(is_forwarding_header));
        assert_eq!(parts.headers["host"], "direct.example:8080");
        assert_eq!(parts.headers["x-request-id"], "visitor-id");
        assert_eq!(parts.uri, original_uri);
    }

    #[test]
    #[expect(
        clippy::shadow_unrelated,
        reason = "sequential direct-head fixtures assert metadata with authority present and then unavailable"
    )]
    fn direct_authority_uses_http_authority_without_inferring_transport_scheme() {
        let mut parts = parts(&[]);
        parts.uri = "https://uri.example:443/path?secret=value".parse().unwrap();
        let metadata = normalize(&TrustedProxyPolicy::no_trust(), None, &parts);
        assert_eq!(
            metadata.effective_host().authority(),
            Some("uri.example:443")
        );
        assert_eq!(metadata.effective_scheme(), Scheme::Http);
        assert_eq!(metadata.scheme_source(), CheckedHostSource::Direct);
        parts.uri = "/".parse().unwrap();
        parts.headers.remove("host");
        let metadata = normalize(&TrustedProxyPolicy::no_trust(), None, &parts);
        assert_eq!(metadata.effective_host().authority(), None);
        assert_eq!(
            metadata.effective_host().source(),
            CheckedHostSource::Unavailable
        );
        parts
            .headers
            .insert("host", HeaderValue::from_static("bad.example:65536"));
        assert_eq!(
            normalize(&TrustedProxyPolicy::no_trust(), None, &parts)
                .effective_host()
                .authority(),
            None
        );
    }

    #[test]
    fn debug_metadata_does_not_dump_private_ingress_values() {
        let metadata = checked(
            ForwardingHeaderFamily::XForwarded,
            &[
                ("x-forwarded-for", CLIENT),
                ("x-forwarded-host", "sentinel-secret.example"),
                ("x-forwarded-proto", "https"),
            ],
        );
        let debug = format!("{metadata:?}");
        for private in [CLIENT, "sentinel-secret", "10.0.0.2"] {
            assert!(!debug.contains(private));
        }
        for result in [
            ForwardingResult::Absent,
            ForwardingResult::Accepted,
            ForwardingResult::Partial,
            ForwardingResult::UntrustedPeer,
            ForwardingResult::Malformed,
            ForwardingResult::UnresolvedChain,
        ] {
            assert!(
                result
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            );
        }
        assert_eq!(Scheme::Http.as_str(), "http");
        assert_eq!(Scheme::Https.as_str(), "https");
    }
}
