//! Validated limits for Axum's adapter-owned HTTP/1 listener and parser.

use std::time::Duration;

use edgezero_core::env_config::EnvConfig;
use edgezero_core::http::{HeaderName, HeaderValue};

/// Effective HTTP/1 ingress limits, frozen before the listener starts.
///
/// The raw-head budget includes the request line, field syntax, CRLFs and final
/// blank line. It bounds both the request line and headers jointly, not with
/// independent byte allowances. The head timeout also bounds keep-alive idle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxumIngressConfig {
    head_read_timeout: Duration,
    max_connections: usize,
    max_header_count: usize,
    max_raw_head_bytes: usize,
}

/// A rejected setting. Diagnostics never include the supplied value.
#[derive(Debug, thiserror::Error)]
#[error("invalid Axum ingress setting `{field}`")]
pub struct IngressConfigError {
    field: &'static str,
}

impl Default for AxumIngressConfig {
    #[inline]
    fn default() -> Self {
        Self {
            head_read_timeout: Duration::from_secs(10),
            max_connections: 256,
            max_header_count: 100,
            max_raw_head_bytes: 0x0001_0000,
        }
    }
}

impl AxumIngressConfig {
    /// Resolve `EDGEZERO__ADAPTER__INGRESS__*`; only missing settings use defaults.
    ///
    /// # Errors
    /// Rejects malformed, zero, overflowing or out-of-range explicit settings.
    #[inline]
    pub fn from_env(env: &EnvConfig) -> Result<Self, IngressConfigError> {
        let defaults = Self::default();
        let milliseconds = setting(env, "head_read_timeout_ms", 10_000)?;
        let timeout_millis =
            u64::try_from(milliseconds).map_err(|_overflow| IngressConfigError {
                field: "head_read_timeout_ms",
            })?;
        Self::new(
            setting(env, "max_connections", defaults.max_connections)?,
            setting(env, "max_raw_head_bytes", defaults.max_raw_head_bytes)?,
            setting(env, "max_header_count", defaults.max_header_count)?,
            Duration::from_millis(timeout_millis),
        )
    }

    /// Absolute idle-plus-header budget for each request head.
    #[must_use]
    #[inline]
    pub const fn head_read_timeout(self) -> Duration {
        self.head_read_timeout
    }

    /// Maximum owned live HTTP/1 connections, including keep-alive.
    #[must_use]
    #[inline]
    pub const fn max_connections(self) -> usize {
        self.max_connections
    }

    /// Maximum raw fields in a request head, counting duplicates separately.
    #[must_use]
    #[inline]
    pub const fn max_header_count(self) -> usize {
        self.max_header_count
    }

    /// Combined raw request-head byte limit, inclusive for complete heads.
    #[must_use]
    #[inline]
    pub const fn max_raw_head_bytes(self) -> usize {
        self.max_raw_head_bytes
    }

    /// Maximum application response field bytes before Hyper encodes/caches them.
    #[must_use]
    #[inline]
    pub const fn max_response_header_bytes(self) -> usize {
        self.max_raw_head_bytes
    }

    /// Response field cap bounds the header map Hyper reuses for request parsing.
    #[must_use]
    #[inline]
    pub fn max_response_header_count(self) -> usize {
        self.max_header_count.max(100)
    }

    /// Construct validated settings.
    ///
    /// Ranges: 1..=4096 connections, 8192..=1048576 raw-head bytes,
    /// 1..=1024 fields, and 1 ms..=120 s idle-plus-header timeout.
    ///
    /// # Errors
    /// Rejects values outside the supported finite ranges.
    #[inline]
    pub fn new(
        max_connections: usize,
        max_raw_head_bytes: usize,
        max_header_count: usize,
        head_read_timeout: Duration,
    ) -> Result<Self, IngressConfigError> {
        for (valid, field) in [
            ((1..=4096).contains(&max_connections), "max_connections"),
            (
                (8192..=0x0010_0000).contains(&max_raw_head_bytes),
                "max_raw_head_bytes",
            ),
            ((1..=1024).contains(&max_header_count), "max_header_count"),
            (
                (Duration::from_millis(1)..=Duration::from_mins(2)).contains(&head_read_timeout),
                "head_read_timeout_ms",
            ),
        ] {
            if !valid {
                return Err(IngressConfigError { field });
            }
        }
        Ok(Self {
            head_read_timeout,
            max_connections,
            max_header_count,
            max_raw_head_bytes,
        })
    }

    /// Hyper's logical buffer threshold; allocation overhead is accounted separately.
    #[must_use]
    #[inline]
    pub const fn parser_buffer_bytes(self) -> usize {
        self.max_raw_head_bytes
    }

    /// Conservative requested metadata capacity per parser/cache header map.
    /// Excludes name/value bytes, the fixed map object, allocator overhead and RSS.
    /// Audited against http 1.4.2 and Rust 1.95.0, including collision-driven growth.
    #[must_use]
    #[inline]
    pub fn parser_header_map_allocation_bytes(self) -> usize {
        let fields = self.max_response_header_count().saturating_add(1);
        let raw_slots = 1_usize << fields.saturating_mul(10).ilog2();
        let buckets = raw_slots.saturating_mul(3) >> 2_usize;
        let extras = fields.saturating_sub(1).max(4).next_power_of_two();
        let word = size_of::<usize>();
        let bucket_bytes = size_of::<HeaderName>()
            .saturating_add(size_of::<HeaderValue>())
            .saturating_add(word.saturating_mul(8));
        let extra_bytes = size_of::<HeaderValue>().saturating_add(word.saturating_mul(6));
        raw_slots
            .saturating_mul(4)
            .saturating_add(buckets.saturating_mul(bucket_bytes))
            .saturating_add(extras.saturating_mul(extra_bytes))
    }

    /// Conservative capacity bound per receive backing allocation, not RSS.
    /// Audited against Hyper 1.12.0, bytes 1.12.1 and Rust 1.95.0.
    #[must_use]
    #[inline]
    pub const fn parser_read_allocation_bytes(self) -> usize {
        self.max_raw_head_bytes.saturating_mul(4)
    }

    /// Conservative capacity bound for the separate raw chunked-trailer buffer.
    #[must_use]
    #[inline]
    pub const fn parser_trailer_allocation_bytes(self) -> usize {
        self.max_raw_head_bytes.next_power_of_two()
    }
}

fn setting(
    env: &EnvConfig,
    field: &'static str,
    default: usize,
) -> Result<usize, IngressConfigError> {
    let Some(value) = env.get(&["adapter", "ingress", field]) else {
        return Ok(default);
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(IngressConfigError { field });
    }
    value
        .parse()
        .map_err(|_invalid_integer| IngressConfigError { field })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use edgezero_core::env_config::EnvConfig;
    use edgezero_core::http::HeaderValue;

    use super::AxumIngressConfig;

    #[test]
    fn defaults_are_finite_and_effective() {
        let config = AxumIngressConfig::default();
        assert_eq!(config.max_connections(), 256);
        assert_eq!(config.max_raw_head_bytes(), 0x0001_0000);
        assert_eq!(config.max_header_count(), 100);
        assert_eq!(config.parser_buffer_bytes(), 0x0001_0000);
        assert_eq!(config.parser_read_allocation_bytes(), 0x0004_0000);
        assert_eq!(config.parser_trailer_allocation_bytes(), 0x0001_0000);
        assert_eq!(config.head_read_timeout(), Duration::from_secs(10));
        assert!(config.parser_header_map_allocation_bytes() > 100 * size_of::<HeaderValue>());
        assert_eq!(
            AxumIngressConfig::from_env(&EnvConfig::default()).unwrap(),
            config
        );
    }

    #[test]
    fn explicit_environment_values_are_validated() {
        let env = EnvConfig::from_vars([
            ("EDGEZERO__ADAPTER__INGRESS__MAX_CONNECTIONS", "2"),
            ("EDGEZERO__ADAPTER__INGRESS__MAX_RAW_HEAD_BYTES", "8192"),
            ("EDGEZERO__ADAPTER__INGRESS__MAX_HEADER_COUNT", "4"),
            ("EDGEZERO__ADAPTER__INGRESS__HEAD_READ_TIMEOUT_MS", "25"),
        ]);
        let config = AxumIngressConfig::from_env(&env).unwrap();
        assert_eq!(config.max_connections(), 2);
        assert_eq!(config.max_raw_head_bytes(), 8192);
        assert_eq!(config.max_header_count(), 4);
        assert_eq!(config.head_read_timeout(), Duration::from_millis(25));
        let non_power_of_two = AxumIngressConfig::new(2, 8193, 4, Duration::from_secs(1)).unwrap();
        assert_eq!(non_power_of_two.parser_buffer_bytes(), 8193);
        assert_eq!(non_power_of_two.parser_read_allocation_bytes(), 0x8004);
        assert_eq!(non_power_of_two.parser_trailer_allocation_bytes(), 0x4000);
    }

    #[test]
    fn invalid_explicit_values_never_fall_back() {
        for field in [
            "MAX_CONNECTIONS",
            "MAX_RAW_HEAD_BYTES",
            "MAX_HEADER_COUNT",
            "HEAD_READ_TIMEOUT_MS",
        ] {
            for value in ["", " ", "0", "-1", "+1", "abc", "184467440737095516160"] {
                let env =
                    EnvConfig::from_vars([(format!("EDGEZERO__ADAPTER__INGRESS__{field}"), value)]);
                let error = AxumIngressConfig::from_env(&env).unwrap_err();
                assert!(!error.to_string().contains(value) || value.is_empty() || value == " ");
            }
        }
    }

    #[test]
    fn programmatic_limits_accept_boundaries_and_reject_overflow() {
        AxumIngressConfig::new(1, 8192, 1, Duration::from_millis(1)).unwrap();
        AxumIngressConfig::new(4096, 0x0010_0000, 1024, Duration::from_mins(2)).unwrap();
        for (connections, bytes, count, timeout) in [
            (0, 8192, 1, Duration::from_secs(1)),
            (4097, 8192, 1, Duration::from_secs(1)),
            (1, 8191, 1, Duration::from_secs(1)),
            (1, 0x0010_0001, 1, Duration::from_secs(1)),
            (1, 8192, 0, Duration::from_secs(1)),
            (1, 8192, 1025, Duration::from_secs(1)),
            (1, 8192, 1, Duration::ZERO),
            (1, 8192, 1, Duration::from_secs(121)),
        ] {
            AxumIngressConfig::new(connections, bytes, count, timeout).unwrap_err();
        }
    }
}
