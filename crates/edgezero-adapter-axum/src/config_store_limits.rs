//! Validated allocation budgets for immutable local configuration snapshots.

use edgezero_core::env_config::EnvConfig;

/// Limits shared by every local config store loaded during one server startup.
///
/// Resident and startup allowances are requested allocation charges, not RSS.
/// Startup reserves five file budgets for input, decoder growth and temporary strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigStoreLimits {
    entries: usize,
    file_bytes: usize,
    key_bytes: usize,
    resident_bytes: usize,
    startup_bytes: usize,
    value_bytes: usize,
}

/// A rejected setting. The supplied value is never included in diagnostics.
#[derive(Debug, thiserror::Error)]
#[error("invalid Axum config-store setting `{field}`")]
pub struct ConfigStoreLimitsError {
    field: &'static str,
}

impl Default for ConfigStoreLimits {
    #[inline]
    fn default() -> Self {
        Self {
            entries: 1024,
            file_bytes: 0x0100_0000,
            key_bytes: 1024,
            resident_bytes: 0x0200_0000,
            startup_bytes: 0x0700_0000,
            value_bytes: 0x0080_0000,
        }
    }
}

impl ConfigStoreLimits {
    /// Resolve `EDGEZERO__ADAPTER__CONFIG_STORE__*`. Missing settings alone use defaults.
    ///
    /// # Errors
    /// Rejects malformed, zero, out-of-range or inconsistent explicit settings.
    #[inline]
    pub fn from_env(env: &EnvConfig) -> Result<Self, ConfigStoreLimitsError> {
        let defaults = Self::default();
        Self::new(
            setting(env, "max_file_bytes", defaults.file_bytes)?,
            setting(env, "max_entries", defaults.entries)?,
            setting(env, "max_key_bytes", defaults.key_bytes)?,
            setting(env, "max_value_bytes", defaults.value_bytes)?,
            setting(env, "max_resident_bytes", defaults.resident_bytes)?,
            setting(env, "max_startup_bytes", defaults.startup_bytes)?,
        )
    }

    /// Maximum entries in any one snapshot, including duplicate-key attempts.
    #[inline]
    #[must_use]
    pub const fn max_entries(self) -> usize {
        self.entries
    }

    /// Maximum wire bytes read from a single opened local file.
    #[inline]
    #[must_use]
    pub const fn max_file_bytes(self) -> usize {
        self.file_bytes
    }

    /// Maximum decoded UTF-8 bytes per key.
    #[inline]
    #[must_use]
    pub const fn max_key_bytes(self) -> usize {
        self.key_bytes
    }

    /// Maximum charged residency across the entire startup registry.
    #[inline]
    #[must_use]
    pub const fn max_resident_bytes(self) -> usize {
        self.resident_bytes
    }

    /// Maximum charged residency plus staging during sequential startup loading.
    #[inline]
    #[must_use]
    pub const fn max_startup_bytes(self) -> usize {
        self.startup_bytes
    }

    /// Maximum decoded UTF-8 bytes per value, before creating its shared allocation.
    #[inline]
    #[must_use]
    pub const fn max_value_bytes(self) -> usize {
        self.value_bytes
    }

    /// Construct finite limits. File/value/resident limits support up to 256 MiB;
    /// startup up to 1 GiB, keys up to 64 KiB and entries up to 65536.
    ///
    /// # Errors
    /// Rejects invalid ranges, keys/values larger than the wire budget, or a startup
    /// budget smaller than the worst-case input/decoder staging reservation.
    #[inline]
    pub fn new(
        max_file_bytes: usize,
        max_entries: usize,
        max_key_bytes: usize,
        max_value_bytes: usize,
        max_resident_bytes: usize,
        max_startup_bytes: usize,
    ) -> Result<Self, ConfigStoreLimitsError> {
        for (valid, field) in [
            (
                (1..=0x1000_0000).contains(&max_file_bytes),
                "max_file_bytes",
            ),
            ((1..=0x0001_0000).contains(&max_entries), "max_entries"),
            (
                (1..=0x0001_0000).contains(&max_key_bytes) && max_key_bytes <= max_file_bytes,
                "max_key_bytes",
            ),
            (
                (1..=0x1000_0000).contains(&max_value_bytes) && max_value_bytes <= max_file_bytes,
                "max_value_bytes",
            ),
            (
                (1..=0x1000_0000).contains(&max_resident_bytes),
                "max_resident_bytes",
            ),
            (
                (1..=0x4000_0000).contains(&max_startup_bytes)
                    && max_startup_bytes >= max_file_bytes.max(32).saturating_mul(5),
                "max_startup_bytes",
            ),
        ] {
            if !valid {
                return Err(ConfigStoreLimitsError { field });
            }
        }
        Ok(Self {
            entries: max_entries,
            file_bytes: max_file_bytes,
            key_bytes: max_key_bytes,
            resident_bytes: max_resident_bytes,
            startup_bytes: max_startup_bytes,
            value_bytes: max_value_bytes,
        })
    }

    /// Reserved requested input/decoder capacity while one file is loaded.
    #[inline]
    #[must_use]
    pub const fn staging_bytes(self) -> usize {
        let base = if self.file_bytes < 32 {
            32
        } else {
            self.file_bytes
        };
        base.saturating_mul(5)
    }
}

fn setting(
    env: &EnvConfig,
    field: &'static str,
    default: usize,
) -> Result<usize, ConfigStoreLimitsError> {
    let Some(value) = env.get(&["adapter", "config_store", field]) else {
        return Ok(default);
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ConfigStoreLimitsError { field });
    }
    value
        .parse()
        .map_err(|_invalid_integer| ConfigStoreLimitsError { field })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_explicit_limits_are_validated() {
        let defaults = ConfigStoreLimits::from_env(&EnvConfig::default()).expect("defaults");
        assert_eq!(defaults, ConfigStoreLimits::default());
        assert_eq!(defaults.staging_bytes(), 5 * defaults.max_file_bytes());
        let env = EnvConfig::from_vars([("EDGEZERO__ADAPTER__CONFIG_STORE__MAX_ENTRIES", "2")]);
        assert_eq!(
            ConfigStoreLimits::from_env(&env)
                .expect("explicit")
                .max_entries(),
            2
        );
    }

    #[test]
    fn invalid_explicit_limits_never_fall_back_or_echo_values() {
        for field in [
            "MAX_FILE_BYTES",
            "MAX_ENTRIES",
            "MAX_KEY_BYTES",
            "MAX_VALUE_BYTES",
            "MAX_RESIDENT_BYTES",
            "MAX_STARTUP_BYTES",
        ] {
            for value in [
                "",
                "0",
                "-1",
                "+2",
                " 2",
                "credential=private",
                "18446744073709551616",
            ] {
                let key = format!("EDGEZERO__ADAPTER__CONFIG_STORE__{field}");
                let env = EnvConfig::from_vars([(key.as_str(), value)]);
                let error =
                    ConfigStoreLimits::from_env(&env).expect_err("invalid explicit setting");
                assert!(!error.to_string().contains("credential=private"));
            }
        }
        ConfigStoreLimits::new(10, 1, 1, 1, 10, 29).expect_err("invalid policy");
        ConfigStoreLimits::new(10, 1, 11, 1, 10, 30).expect_err("invalid policy");
        ConfigStoreLimits::new(10, 1, 1, 11, 10, 30).expect_err("invalid policy");
    }
}
