//! Target platform properties that affect application resource policy.

use std::num::NonZeroU32;

/// Where a platform resource fact comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PlatformResourceSource {
    /// A default published by a named hosted runtime profile.
    HostedDefault {
        /// Hosted runtime profile that publishes the default.
        provider: &'static str,
    },
    /// A limit published for the named platform.
    PlatformLimit {
        /// Platform that publishes the limit.
        provider: &'static str,
    },
    /// A value supplied by a configurable runtime deployment.
    RuntimeConfiguration {
        /// Runtime whose deployment configuration supplied the value.
        runtime: &'static str,
    },
}

/// Why a platform resource fact is not known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PlatformUnknownReason {
    /// The operator must supply the value.
    OperatorConfigured,
    /// The provider does not publish the value.
    ProviderUnpublished,
    /// The runtime deployment determines the value.
    RuntimeConfigured,
    /// No more specific reason was supplied.
    Unspecified,
}

/// A platform resource fact that preserves both provenance and unknown state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PlatformFact<Value> {
    /// A known value and its source.
    Known {
        /// Published or configured value.
        value: Value,
        /// Where the value comes from.
        source: PlatformResourceSource,
    },
    /// A value that is not known.
    Unknown {
        /// Why the value is not known.
        reason: PlatformUnknownReason,
    },
}

impl<Value> PlatformFact<Value> {
    /// Construct a known platform fact.
    #[must_use]
    #[inline]
    pub const fn known(value: Value, source: PlatformResourceSource) -> Self {
        Self::Known { value, source }
    }

    /// Construct an unknown platform fact.
    #[must_use]
    #[inline]
    pub const fn unknown(reason: PlatformUnknownReason) -> Self {
        Self::Unknown { reason }
    }
}

impl<Value> PlatformFact<Value>
where
    Value: Copy,
{
    /// Return the source of a known fact.
    #[must_use]
    #[inline]
    pub const fn source(self) -> Option<PlatformResourceSource> {
        match self {
            Self::Known { source, .. } => Some(source),
            Self::Unknown { .. } => None,
        }
    }

    /// Return the reason for an unknown fact.
    #[must_use]
    #[inline]
    pub const fn unknown_reason(self) -> Option<PlatformUnknownReason> {
        match self {
            Self::Known { .. } => None,
            Self::Unknown { reason } => Some(reason),
        }
    }

    /// Return the known value, if present.
    #[must_use]
    #[inline]
    pub const fn value(self) -> Option<Value> {
        match self {
            Self::Known { value, .. } => Some(value),
            Self::Unknown { .. } => None,
        }
    }
}

/// What shares a target's published memory ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryCeilingScope {
    /// Every request execution receives an independent budget.
    PerExecution,
    /// Concurrent requests served by one runtime instance share one budget.
    PerInstance,
}

/// A target's primary memory ceiling and its accounting semantics.
///
/// `total_bytes` is the primary memory allowance. When `stack_bytes` is present,
/// the platform publishes that stack allowance separately and it is not
/// included in `total_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MemoryCeiling {
    scope: MemoryCeilingScope,
    stack_bytes: Option<u64>,
    total_bytes: u64,
}

impl MemoryCeiling {
    /// Construct a memory ceiling from exact byte counts.
    #[must_use]
    #[inline]
    pub const fn new(
        total_bytes: u64,
        scope: MemoryCeilingScope,
        stack_bytes: Option<u64>,
    ) -> Self {
        Self {
            scope,
            stack_bytes,
            total_bytes,
        }
    }

    /// What shares this memory budget.
    #[must_use]
    #[inline]
    pub const fn scope(self) -> MemoryCeilingScope {
        self.scope
    }

    /// Separately published stack allowance, in bytes.
    #[must_use]
    #[inline]
    pub const fn stack_bytes(self) -> Option<u64> {
        self.stack_bytes
    }

    /// Primary memory allowance, in bytes, excluding a separately reported stack.
    #[must_use]
    #[inline]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }
}

/// Maximum live inbound request executions charged to one memory ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct InboundRequestPopulationBound {
    max_live_requests: NonZeroU32,
}

impl InboundRequestPopulationBound {
    /// Maximum live inbound requests charged to one memory ceiling.
    #[must_use]
    #[inline]
    pub const fn max_live_requests(self) -> NonZeroU32 {
        self.max_live_requests
    }

    /// Construct an inbound request population bound.
    #[must_use]
    #[inline]
    pub const fn new(max_live_requests: NonZeroU32) -> Self {
        Self { max_live_requests }
    }
}

/// Whether host-owned ingress memory is charged to the published ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HostIngressMemoryAccounting {
    /// Host-owned ingress memory is charged to the published ceiling.
    CountsTowardCeiling,
    /// Host-owned ingress memory is outside the published ceiling.
    OutsideCeiling,
}

/// Application memory requirements declared for one platform memory domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MemoryEnvelope {
    fixed_bytes: u64,
    per_live_request_bytes: u64,
    scope: MemoryCeilingScope,
    separate_stack_bytes: Option<u64>,
}

impl MemoryEnvelope {
    /// Bytes charged once to the memory domain.
    #[must_use]
    #[inline]
    pub const fn fixed_bytes(self) -> u64 {
        self.fixed_bytes
    }

    /// Construct a memory envelope from exact byte counts.
    #[must_use]
    #[inline]
    pub const fn new(
        scope: MemoryCeilingScope,
        fixed_bytes: u64,
        per_live_request_bytes: u64,
        separate_stack_bytes: Option<u64>,
    ) -> Self {
        Self {
            fixed_bytes,
            per_live_request_bytes,
            scope,
            separate_stack_bytes,
        }
    }

    /// Bytes charged for each live inbound request.
    #[must_use]
    #[inline]
    pub const fn per_live_request_bytes(self) -> u64 {
        self.per_live_request_bytes
    }

    /// Scope in which this envelope's accounting applies.
    #[must_use]
    #[inline]
    pub const fn scope(self) -> MemoryCeilingScope {
        self.scope
    }

    /// Required bytes from a separately published stack allowance.
    #[must_use]
    #[inline]
    pub const fn separate_stack_bytes(self) -> Option<u64> {
        self.separate_stack_bytes
    }
}

/// Immutable target properties supplied to application construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PlatformMetadata {
    host_ingress_memory_accounting: PlatformFact<HostIngressMemoryAccounting>,
    inbound_request_population_bound: PlatformFact<InboundRequestPopulationBound>,
    memory_ceiling: PlatformFact<MemoryCeiling>,
}

impl PlatformMetadata {
    /// The target's host ingress memory accounting fact.
    #[must_use]
    #[inline]
    pub const fn host_ingress_memory_accounting(self) -> PlatformFact<HostIngressMemoryAccounting> {
        self.host_ingress_memory_accounting
    }

    /// The target's live inbound request population bound fact.
    #[must_use]
    #[inline]
    pub const fn inbound_request_population_bound(
        self,
    ) -> PlatformFact<InboundRequestPopulationBound> {
        self.inbound_request_population_bound
    }

    /// The target's published memory ceiling fact.
    #[must_use]
    #[inline]
    pub const fn memory_ceiling(self) -> PlatformFact<MemoryCeiling> {
        self.memory_ceiling
    }

    /// Construct target metadata from explicit platform resource facts.
    #[must_use]
    #[inline]
    pub const fn new(
        memory_ceiling: PlatformFact<MemoryCeiling>,
        inbound_request_population_bound: PlatformFact<InboundRequestPopulationBound>,
        host_ingress_memory_accounting: PlatformFact<HostIngressMemoryAccounting>,
    ) -> Self {
        Self {
            host_ingress_memory_accounting,
            inbound_request_population_bound,
            memory_ceiling,
        }
    }
}

impl Default for PlatformMetadata {
    #[inline]
    fn default() -> Self {
        Self::new(
            PlatformFact::unknown(PlatformUnknownReason::Unspecified),
            PlatformFact::unknown(PlatformUnknownReason::Unspecified),
            PlatformFact::unknown(PlatformUnknownReason::Unspecified),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_fact_distinguishes_known_and_unknown_metadata() {
        const CEILING: MemoryCeiling =
            MemoryCeiling::new(128_000_000, MemoryCeilingScope::PerInstance, None);
        const SOURCE: PlatformResourceSource = PlatformResourceSource::PlatformLimit {
            provider: "test-platform",
        };
        const KNOWN: PlatformFact<MemoryCeiling> = PlatformFact::known(CEILING, SOURCE);
        const UNKNOWN: PlatformFact<MemoryCeiling> =
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished);

        assert_eq!(KNOWN.value(), Some(CEILING));
        assert_eq!(KNOWN.source(), Some(SOURCE));
        assert_eq!(KNOWN.unknown_reason(), None);
        assert_eq!(UNKNOWN.value(), None);
        assert_eq!(UNKNOWN.source(), None);
        assert_eq!(
            UNKNOWN.unknown_reason(),
            Some(PlatformUnknownReason::ProviderUnpublished)
        );
    }

    #[test]
    fn memory_envelope_accessors_preserve_declared_shape() {
        const ENVELOPE: MemoryEnvelope =
            MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 8_192, 4_096, Some(1_024));

        assert_eq!(ENVELOPE.scope(), MemoryCeilingScope::PerInstance);
        assert_eq!(ENVELOPE.fixed_bytes(), 8_192);
        assert_eq!(ENVELOPE.per_live_request_bytes(), 4_096);
        assert_eq!(ENVELOPE.separate_stack_bytes(), Some(1_024));
    }
}
