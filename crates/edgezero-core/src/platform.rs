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

/// Result of validating an application memory envelope against platform facts.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryEnvelopeValidation {
    /// A known memory resource cannot satisfy its requirement.
    Exceeds {
        /// Published bytes available for the resource.
        available_bytes: u64,
        /// Bytes required from the resource.
        required_bytes: u64,
        /// Resource whose requirement exceeds its allowance.
        ///
        /// Primary memory excess is reported before separate-stack excess when both apply.
        resource: MemoryResource,
    },
    /// All required facts are known and every requirement fits.
    Fits {
        /// Published primary memory allowance.
        available_primary_bytes: u64,
        /// Separately published stack allowance, when present.
        available_separate_stack_bytes: Option<u64>,
        /// Required primary memory.
        required_primary_bytes: u64,
        /// Required separately published stack memory, when present.
        required_separate_stack_bytes: Option<u64>,
    },
    /// No known requirement exceeds its allowance, but facts needed for a fit are missing.
    Indeterminate {
        /// Known primary allowance, when published.
        available_primary_bytes: Option<u64>,
        /// Checked minimum primary memory requirement.
        minimum_required_primary_bytes: u64,
        /// Missing facts in deterministic validation order: unknown memory ceiling,
        /// unknown inbound request population, unknown host ingress memory accounting,
        /// then missing separate-stack requirement.
        reasons: Vec<MemoryValidationIndeterminateReason>,
    },
}

/// Invalid memory-envelope or platform-fact combinations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryEnvelopeValidationError {
    /// Checked primary requirement arithmetic overflowed.
    #[error("memory envelope primary requirement arithmetic overflow")]
    ArithmeticOverflow {
        /// Bytes charged once to the memory domain.
        fixed_bytes: u64,
        /// Known request population used in the calculation, if any.
        max_live_requests: Option<NonZeroU32>,
        /// Bytes charged for each live inbound request.
        per_live_request_bytes: u64,
    },
    /// A per-execution memory domain published a population other than one.
    #[error("invalid per-execution request population")]
    NonUnitPerExecutionPopulation {
        /// Published maximum live request population.
        max_live_requests: NonZeroU32,
    },
    /// The envelope and published ceiling use different accounting scopes.
    #[error("memory envelope scope mismatch")]
    ScopeMismatch {
        /// Scope of the published ceiling.
        ceiling_scope: MemoryCeilingScope,
        /// Scope declared by the envelope.
        envelope_scope: MemoryCeilingScope,
    },
    /// The envelope requires a separate stack but the known ceiling does not publish one.
    #[error("unexpected separate stack requirement")]
    UnexpectedSeparateStackRequirement {
        /// Required separate stack bytes.
        required_bytes: u64,
    },
}

/// Memory resource compared by envelope validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryResource {
    /// Primary memory allowance.
    Primary,
    /// Separately published stack allowance.
    SeparateStack,
}

/// Missing fact that prevents a complete memory-envelope validation result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryValidationIndeterminateReason {
    /// The platform publishes a separate stack allowance but no requirement was supplied.
    MissingSeparateStackRequirement,
    /// Host ingress memory accounting is unknown.
    UnknownHostIngressMemoryAccounting {
        /// Why the accounting fact is unknown.
        reason: PlatformUnknownReason,
    },
    /// The maximum live inbound request population is unknown.
    UnknownInboundRequestPopulation {
        /// Why the population fact is unknown.
        reason: PlatformUnknownReason,
    },
    /// The platform memory ceiling is unknown.
    UnknownMemoryCeiling {
        /// Why the ceiling fact is unknown.
        reason: PlatformUnknownReason,
    },
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

    /// Validate an application memory envelope against the target's published facts.
    ///
    /// # Errors
    ///
    /// Returns an error for contradictory scopes or stack shapes, a non-unit
    /// per-execution population, or checked primary requirement overflow.
    #[inline]
    pub fn validate_memory_envelope(
        self,
        envelope: MemoryEnvelope,
    ) -> Result<MemoryEnvelopeValidation, MemoryEnvelopeValidationError> {
        let known_ceiling = self.memory_ceiling.value();
        let known_population = self
            .inbound_request_population_bound
            .value()
            .map(InboundRequestPopulationBound::max_live_requests);

        if let Some(ceiling) = known_ceiling {
            if envelope.scope() != ceiling.scope() {
                return Err(MemoryEnvelopeValidationError::ScopeMismatch {
                    ceiling_scope: ceiling.scope(),
                    envelope_scope: envelope.scope(),
                });
            }
            if let (Some(required_bytes), None) =
                (envelope.separate_stack_bytes(), ceiling.stack_bytes())
            {
                return Err(
                    MemoryEnvelopeValidationError::UnexpectedSeparateStackRequirement {
                        required_bytes,
                    },
                );
            }
        }

        if envelope.scope() == MemoryCeilingScope::PerExecution
            && let Some(max_live_requests) = known_population
            && max_live_requests != NonZeroU32::MIN
        {
            return Err(
                MemoryEnvelopeValidationError::NonUnitPerExecutionPopulation { max_live_requests },
            );
        }

        let population_multiplier = match envelope.scope() {
            MemoryCeilingScope::PerExecution => 1,
            MemoryCeilingScope::PerInstance => {
                known_population.map_or(1, |value| value.get().into())
            }
        };
        let minimum_required_primary_bytes = envelope
            .per_live_request_bytes()
            .checked_mul(population_multiplier)
            .and_then(|request_bytes| envelope.fixed_bytes().checked_add(request_bytes))
            .ok_or(MemoryEnvelopeValidationError::ArithmeticOverflow {
                fixed_bytes: envelope.fixed_bytes(),
                max_live_requests: known_population,
                per_live_request_bytes: envelope.per_live_request_bytes(),
            })?;

        if let Some(ceiling) = known_ceiling {
            if minimum_required_primary_bytes > ceiling.total_bytes() {
                return Ok(MemoryEnvelopeValidation::Exceeds {
                    available_bytes: ceiling.total_bytes(),
                    required_bytes: minimum_required_primary_bytes,
                    resource: MemoryResource::Primary,
                });
            }
            if let (Some(required_bytes), Some(available_bytes)) =
                (envelope.separate_stack_bytes(), ceiling.stack_bytes())
                && required_bytes > available_bytes
            {
                return Ok(MemoryEnvelopeValidation::Exceeds {
                    available_bytes,
                    required_bytes,
                    resource: MemoryResource::SeparateStack,
                });
            }
        }

        let mut reasons = Vec::new();
        if let PlatformFact::Unknown { reason } = self.memory_ceiling {
            reasons.push(MemoryValidationIndeterminateReason::UnknownMemoryCeiling { reason });
        }
        if let PlatformFact::Unknown { reason } = self.inbound_request_population_bound {
            reasons.push(
                MemoryValidationIndeterminateReason::UnknownInboundRequestPopulation { reason },
            );
        }
        if let PlatformFact::Unknown { reason } = self.host_ingress_memory_accounting {
            reasons.push(
                MemoryValidationIndeterminateReason::UnknownHostIngressMemoryAccounting { reason },
            );
        }
        if known_ceiling.is_some_and(|ceiling| ceiling.stack_bytes().is_some())
            && envelope.separate_stack_bytes().is_none()
        {
            reasons.push(MemoryValidationIndeterminateReason::MissingSeparateStackRequirement);
        }

        if let Some(ceiling) = known_ceiling
            && reasons.is_empty()
        {
            return Ok(MemoryEnvelopeValidation::Fits {
                available_primary_bytes: ceiling.total_bytes(),
                available_separate_stack_bytes: ceiling.stack_bytes(),
                required_primary_bytes: minimum_required_primary_bytes,
                required_separate_stack_bytes: envelope.separate_stack_bytes(),
            });
        }

        Ok(MemoryEnvelopeValidation::Indeterminate {
            available_primary_bytes: known_ceiling.map(MemoryCeiling::total_bytes),
            minimum_required_primary_bytes,
            reasons,
        })
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
    use std::error::Error;
    use std::num::NonZeroU32;

    use super::*;

    const TEST_SOURCE: PlatformResourceSource = PlatformResourceSource::PlatformLimit {
        provider: "test-platform",
    };

    fn known<Value>(value: Value) -> PlatformFact<Value> {
        PlatformFact::known(value, TEST_SOURCE)
    }

    fn population(max_live_requests: u32) -> InboundRequestPopulationBound {
        InboundRequestPopulationBound::new(
            NonZeroU32::new(max_live_requests).expect("test population must be non-zero"),
        )
    }

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

    #[test]
    fn validation_error_messages_are_stable_and_category_only() {
        let errors = [
            MemoryEnvelopeValidationError::ArithmeticOverflow {
                fixed_bytes: 123,
                max_live_requests: Some(population(2).max_live_requests()),
                per_live_request_bytes: 456,
            },
            MemoryEnvelopeValidationError::NonUnitPerExecutionPopulation {
                max_live_requests: population(2).max_live_requests(),
            },
            MemoryEnvelopeValidationError::ScopeMismatch {
                ceiling_scope: MemoryCeilingScope::PerExecution,
                envelope_scope: MemoryCeilingScope::PerInstance,
            },
            MemoryEnvelopeValidationError::UnexpectedSeparateStackRequirement {
                required_bytes: 789,
            },
        ];

        for error in &errors {
            let _: &dyn Error = error;
        }
        assert_eq!(
            errors[0].to_string(),
            "memory envelope primary requirement arithmetic overflow"
        );
        assert_eq!(
            errors[1].to_string(),
            "invalid per-execution request population"
        );
        assert_eq!(errors[2].to_string(), "memory envelope scope mismatch");
        assert_eq!(
            errors[3].to_string(),
            "unexpected separate stack requirement"
        );
    }

    #[test]
    fn validation_fits_per_execution_with_counted_host_memory() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                Some(20),
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::CountsTowardCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerExecution, 40, 30, Some(15));

        assert_eq!(population(1).max_live_requests(), NonZeroU32::MIN);
        let validation: crate::MemoryEnvelopeValidation = metadata
            .validate_memory_envelope(envelope)
            .expect("valid envelope should produce a validation result");
        assert_eq!(
            validation,
            crate::MemoryEnvelopeValidation::Fits {
                available_primary_bytes: 100,
                available_separate_stack_bytes: Some(20),
                required_primary_bytes: 70,
                required_separate_stack_bytes: Some(15),
            }
        );
    }

    #[test]
    fn validation_fits_per_instance_with_host_memory_outside_ceiling() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                1_000,
                MemoryCeilingScope::PerInstance,
                None,
            )),
            known(population(3)),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 100, 200, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Ok(crate::MemoryEnvelopeValidation::Fits {
                available_primary_bytes: 1_000,
                available_separate_stack_bytes: None,
                required_primary_bytes: 700,
                required_separate_stack_bytes: None,
            })
        );
    }

    #[test]
    fn validation_known_minimum_excess_precedes_unknown_facts() {
        let primary_metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                25,
                MemoryCeilingScope::PerInstance,
                Some(5),
            )),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
        );
        let primary_envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 20, 10, None);

        assert_eq!(
            primary_metadata.validate_memory_envelope(primary_envelope),
            Ok(crate::MemoryEnvelopeValidation::Exceeds {
                available_bytes: 25,
                required_bytes: 30,
                resource: crate::MemoryResource::Primary,
            })
        );

        let stack_metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerInstance,
                Some(5),
            )),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
        );
        let stack_envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 20, 10, Some(6));

        assert_eq!(
            stack_metadata.validate_memory_envelope(stack_envelope),
            Ok(crate::MemoryEnvelopeValidation::Exceeds {
                available_bytes: 5,
                required_bytes: 6,
                resource: crate::MemoryResource::SeparateStack,
            })
        );
    }

    #[test]
    fn validation_missing_published_stack_requirement_is_indeterminate() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                Some(20),
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerExecution, 20, 10, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Ok(crate::MemoryEnvelopeValidation::Indeterminate {
                available_primary_bytes: Some(100),
                minimum_required_primary_bytes: 30,
                reasons: vec![
                    crate::MemoryValidationIndeterminateReason::MissingSeparateStackRequirement,
                ],
            })
        );
    }

    #[test]
    fn validation_rejects_non_unit_per_execution_population() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                None,
            )),
            known(population(2)),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerExecution, 20, 10, None);
        let validation: Result<
            crate::MemoryEnvelopeValidation,
            crate::MemoryEnvelopeValidationError,
        > = metadata.validate_memory_envelope(envelope);

        assert_eq!(
            validation,
            Err(
                crate::MemoryEnvelopeValidationError::NonUnitPerExecutionPopulation {
                    max_live_requests: population(2).max_live_requests(),
                },
            )
        );
    }

    #[test]
    fn validation_rejects_primary_arithmetic_overflow() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                u64::MAX,
                MemoryCeilingScope::PerInstance,
                None,
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::CountsTowardCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, u64::MAX, 1, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Err(crate::MemoryEnvelopeValidationError::ArithmeticOverflow {
                fixed_bytes: u64::MAX,
                max_live_requests: Some(NonZeroU32::MIN),
                per_live_request_bytes: 1,
            })
        );

        let multiplication_metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                u64::MAX,
                MemoryCeilingScope::PerInstance,
                None,
            )),
            known(population(2)),
            known(HostIngressMemoryAccounting::CountsTowardCeiling),
        );
        let multiplication_envelope =
            MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 0, u64::MAX, None);

        assert_eq!(
            multiplication_metadata.validate_memory_envelope(multiplication_envelope),
            Err(crate::MemoryEnvelopeValidationError::ArithmeticOverflow {
                fixed_bytes: 0,
                max_live_requests: Some(population(2).max_live_requests()),
                per_live_request_bytes: u64::MAX,
            })
        );
    }

    #[test]
    fn validation_rejects_scope_mismatch() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                None,
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 20, 10, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Err(crate::MemoryEnvelopeValidationError::ScopeMismatch {
                ceiling_scope: MemoryCeilingScope::PerExecution,
                envelope_scope: MemoryCeilingScope::PerInstance,
            })
        );
    }

    #[test]
    fn validation_rejects_stack_shape_mismatch() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                None,
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerExecution, 20, 10, Some(5));

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Err(
                crate::MemoryEnvelopeValidationError::UnexpectedSeparateStackRequirement {
                    required_bytes: 5,
                },
            )
        );
    }

    #[test]
    fn validation_rejects_unknown_population_lower_bound_overflow() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                u64::MAX,
                MemoryCeilingScope::PerInstance,
                None,
            )),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
            known(HostIngressMemoryAccounting::OutsideCeiling),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, u64::MAX, 1, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Err(crate::MemoryEnvelopeValidationError::ArithmeticOverflow {
                fixed_bytes: u64::MAX,
                max_live_requests: None,
                per_live_request_bytes: 1,
            })
        );
    }

    #[test]
    fn validation_reports_primary_or_stack_excess() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                Some(20),
            )),
            known(population(1)),
            known(HostIngressMemoryAccounting::CountsTowardCeiling),
        );

        assert_eq!(
            metadata.validate_memory_envelope(MemoryEnvelope::new(
                MemoryCeilingScope::PerExecution,
                80,
                30,
                Some(15),
            )),
            Ok(crate::MemoryEnvelopeValidation::Exceeds {
                available_bytes: 100,
                required_bytes: 110,
                resource: crate::MemoryResource::Primary,
            })
        );
        assert_eq!(
            metadata.validate_memory_envelope(MemoryEnvelope::new(
                MemoryCeilingScope::PerExecution,
                40,
                30,
                Some(21),
            )),
            Ok(crate::MemoryEnvelopeValidation::Exceeds {
                available_bytes: 20,
                required_bytes: 21,
                resource: crate::MemoryResource::SeparateStack,
            })
        );
        assert_eq!(
            metadata.validate_memory_envelope(MemoryEnvelope::new(
                MemoryCeilingScope::PerExecution,
                80,
                30,
                Some(21),
            )),
            Ok(crate::MemoryEnvelopeValidation::Exceeds {
                available_bytes: 100,
                required_bytes: 110,
                resource: crate::MemoryResource::Primary,
            })
        );
    }

    #[test]
    fn validation_unknown_ceiling_is_indeterminate() {
        let metadata = PlatformMetadata::new(
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
            PlatformFact::unknown(PlatformUnknownReason::RuntimeConfigured),
            PlatformFact::unknown(PlatformUnknownReason::OperatorConfigured),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 10, 20, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Ok(crate::MemoryEnvelopeValidation::Indeterminate {
                available_primary_bytes: None,
                minimum_required_primary_bytes: 30,
                reasons: vec![
                    crate::MemoryValidationIndeterminateReason::UnknownMemoryCeiling {
                        reason: PlatformUnknownReason::ProviderUnpublished,
                    },
                    crate::MemoryValidationIndeterminateReason::UnknownInboundRequestPopulation {
                        reason: PlatformUnknownReason::RuntimeConfigured,
                    },
                    crate::MemoryValidationIndeterminateReason::UnknownHostIngressMemoryAccounting {
                        reason: PlatformUnknownReason::OperatorConfigured,
                    },
                ],
            })
        );
    }

    #[test]
    fn validation_unknown_host_accounting_is_indeterminate() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerExecution,
                None,
            )),
            known(population(1)),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerExecution, 20, 10, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Ok(crate::MemoryEnvelopeValidation::Indeterminate {
                available_primary_bytes: Some(100),
                minimum_required_primary_bytes: 30,
                reasons: vec![
                    crate::MemoryValidationIndeterminateReason::UnknownHostIngressMemoryAccounting {
                        reason: PlatformUnknownReason::ProviderUnpublished,
                    },
                ],
            })
        );
    }

    #[test]
    fn validation_unknown_population_is_indeterminate() {
        let metadata = PlatformMetadata::new(
            known(MemoryCeiling::new(
                100,
                MemoryCeilingScope::PerInstance,
                Some(20),
            )),
            PlatformFact::unknown(PlatformUnknownReason::ProviderUnpublished),
            PlatformFact::unknown(PlatformUnknownReason::RuntimeConfigured),
        );
        let envelope = MemoryEnvelope::new(MemoryCeilingScope::PerInstance, 10, 20, None);

        assert_eq!(
            metadata.validate_memory_envelope(envelope),
            Ok(crate::MemoryEnvelopeValidation::Indeterminate {
                available_primary_bytes: Some(100),
                minimum_required_primary_bytes: 30,
                reasons: vec![
                    crate::MemoryValidationIndeterminateReason::UnknownInboundRequestPopulation {
                        reason: PlatformUnknownReason::ProviderUnpublished,
                    },
                    crate::MemoryValidationIndeterminateReason::UnknownHostIngressMemoryAccounting {
                        reason: PlatformUnknownReason::RuntimeConfigured,
                    },
                    crate::MemoryValidationIndeterminateReason::MissingSeparateStackRequirement,
                ],
            })
        );
    }
}
