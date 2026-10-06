//! Shared logging policy for adapter and application-owned backends.

use std::str::FromStr as _;

use log::LevelFilter;

use crate::env_config::EnvConfig;

/// Default boot filter for managed backends, independent of ordinary logging.
pub const BOOT_LOG_LEVEL: LevelFilter = LevelFilter::Warn;
/// Logical target for startup diagnostics; this is not a platform log endpoint.
pub const BOOT_LOG_TARGET: &str = "edgezero::boot";

/// Resolve `EDGEZERO__LOGGING__LEVEL` without installing or reconfiguring a logger.
///
/// Valid values are case-insensitive `off`, `error`, `warn`, `info`, `debug`, and
/// `trace`. Missing or invalid values fall back to `Info`. The caller supplies
/// platform-resolved environment configuration; this function does not read the
/// process environment or implicitly apply manifest logging settings.
///
/// Application-owned backends must apply this level to ordinary records, apply
/// their chosen boot filter to [`BOOT_LOG_TARGET`], and set the facade maximum
/// high enough for both. Raising the facade maximum does not override a backend
/// filter.
#[must_use]
#[inline]
pub fn resolve_logging_level(env: &EnvConfig) -> LevelFilter {
    env.logging_level()
        .and_then(|raw| LevelFilter::from_str(raw).ok())
        .unwrap_or(LevelFilter::Info)
}

#[cfg(test)]
mod tests {
    use log::LevelFilter;

    use crate::env_config::EnvConfig;

    #[test]
    fn boot_target_preserves_warnings_without_runtime_verbosity() {
        assert_eq!(super::BOOT_LOG_TARGET, "edgezero::boot");
        assert_eq!(super::BOOT_LOG_LEVEL, LevelFilter::Warn);
    }

    #[test]
    fn resolver_defaults_absent_and_invalid_levels_to_info() {
        for raw in [None, Some(""), Some("invalid"), Some(" debug ")] {
            let env = EnvConfig::from_vars(raw.map(|level| ("EDGEZERO__LOGGING__LEVEL", level)));
            assert_eq!(super::resolve_logging_level(&env), LevelFilter::Info);
        }
    }

    #[test]
    fn resolver_preserves_all_levels_and_case_insensitivity() {
        for (raw, expected) in [
            ("off", LevelFilter::Off),
            ("error", LevelFilter::Error),
            ("warn", LevelFilter::Warn),
            ("info", LevelFilter::Info),
            ("debug", LevelFilter::Debug),
            ("trace", LevelFilter::Trace),
            ("DeBuG", LevelFilter::Debug),
        ] {
            let env = EnvConfig::from_vars([("EDGEZERO__LOGGING__LEVEL", raw)]);
            assert_eq!(super::resolve_logging_level(&env), expected);
        }
    }

    #[test]
    fn resolver_does_not_change_global_logging_state() {
        let before = log::max_level();
        let env = EnvConfig::from_vars([("EDGEZERO__LOGGING__LEVEL", "trace")]);
        assert_eq!(super::resolve_logging_level(&env), LevelFilter::Trace);
        assert_eq!(log::max_level(), before);
    }
}
