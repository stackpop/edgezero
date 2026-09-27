//! Target platform properties that affect application resource policy.

/// What shares a target's published memory ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryCeilingScope {
    /// Every request execution receives an independent budget.
    PerExecution,
    /// Concurrent requests served by one runtime instance share one budget.
    PerInstance,
}

/// Where a memory ceiling value comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryCeilingSource {
    /// A default quota published by a named hosted runtime profile.
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

/// A target's primary memory ceiling and its accounting semantics.
///
/// `total_bytes` is the primary memory allowance. When `stack_bytes` is
/// present, the platform publishes that stack allowance separately and it is
/// not included in `total_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MemoryCeiling {
    scope: MemoryCeilingScope,
    source: MemoryCeilingSource,
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
        source: MemoryCeilingSource,
    ) -> Self {
        Self {
            scope,
            source,
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

    /// Where this ceiling value comes from.
    #[must_use]
    #[inline]
    pub const fn source(self) -> MemoryCeilingSource {
        self.source
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

/// Immutable target properties supplied to application construction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct PlatformMetadata {
    memory_ceiling: Option<MemoryCeiling>,
}

impl PlatformMetadata {
    /// The target's known memory ceiling, or `None` when deployment policy owns it.
    #[must_use]
    #[inline]
    pub const fn memory_ceiling(self) -> Option<MemoryCeiling> {
        self.memory_ceiling
    }

    /// Construct target metadata with an optional known memory ceiling.
    #[must_use]
    #[inline]
    pub const fn new(memory_ceiling: Option<MemoryCeiling>) -> Self {
        Self { memory_ceiling }
    }
}
