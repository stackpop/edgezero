//! `EDGEZERO__*` environment-config layer.
//!
//! Adapter-specific runtime config — platform store names, per-store tuning,
//! bind host/port, and logging level — is supplied at runtime through
//! `EDGEZERO__`-prefixed environment variables. `__` (double underscore)
//! separates key-path segments, so `EDGEZERO__STORES__KV__SESSIONS__NAME`
//! parses to the segment path `["stores", "kv", "sessions", "name"]`.
//!
//! Every segment is lower-cased on parse, and lookup arguments are lower-cased
//! before matching — callers pass lower-case logical ids and get a
//! case-insensitive match against the upper-case env-var convention.

use std::collections::BTreeMap;
use std::env;

/// The prefix every recognised variable must start with.
const PREFIX: &str = "EDGEZERO__";
/// The key-path segment separator.
const SEPARATOR: &str = "__";

/// Adapter runtime config resolved from `EDGEZERO__*` environment variables.
///
/// Keys are lower-cased segment paths; values are the raw environment-variable
/// strings. Build one with [`EnvConfig::from_env`] (native targets) or
/// [`EnvConfig::from_vars`] (e.g. Cloudflare Workers, which have no
/// `std::env`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvConfig {
    entries: BTreeMap<Vec<String>, String>,
}

impl EnvConfig {
    /// `EDGEZERO__ADAPTER__HOST`.
    #[must_use]
    #[inline]
    pub fn adapter_host(&self) -> Option<&str> {
        self.get(&["adapter", "host"])
    }

    /// `EDGEZERO__ADAPTER__PORT` (raw string — callers parse it).
    #[must_use]
    #[inline]
    pub fn adapter_port(&self) -> Option<&str> {
        self.get(&["adapter", "port"])
    }

    /// Read all `EDGEZERO__`-prefixed variables from the process environment
    /// (`std::env::vars()`). On targets without a process environment (e.g.
    /// `wasm32-unknown-unknown`) this yields an empty config.
    #[must_use]
    #[inline]
    pub fn from_env() -> Self {
        Self::from_vars(env::vars())
    }

    /// Build from an explicit `(key, value)` iterator. Cloudflare Workers have
    /// no `std::env`; that adapter enumerates its `Env` binding object and
    /// calls this instead of [`EnvConfig::from_env`].
    #[must_use]
    #[inline]
    pub fn from_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut entries = BTreeMap::new();
        for (key, value) in vars {
            let Some(rest) = key.as_ref().strip_prefix(PREFIX) else {
                continue;
            };
            let segments: Vec<String> =
                rest.split(SEPARATOR).map(str::to_ascii_lowercase).collect();
            if segments.is_empty() || segments.iter().any(String::is_empty) {
                continue;
            }
            entries.insert(segments, value.into());
        }
        Self { entries }
    }

    /// Generic lookup by segment path. Segments are matched case-insensitively
    /// — they are lower-cased before comparison, matching the lower-cased
    /// parsed keys.
    #[must_use]
    #[inline]
    pub fn get(&self, segments: &[&str]) -> Option<&str> {
        let path: Vec<String> = segments
            .iter()
            .map(|seg| seg.to_ascii_lowercase())
            .collect();
        self.entries.get(&path).map(String::as_str)
    }

    /// `EDGEZERO__LOGGING__LEVEL`.
    #[must_use]
    #[inline]
    pub fn logging_level(&self) -> Option<&str> {
        self.get(&["logging", "level"])
    }

    /// Platform name for a logical store — `EDGEZERO__STORES__<KIND>__<ID>__NAME`
    /// — falling back to `id` itself when the variable is unset. `kind` is
    /// `"kv"` / `"config"` / `"secrets"`.
    #[must_use]
    #[inline]
    pub fn store_name(&self, kind: &str, id: &str) -> String {
        self.get(&["stores", kind, id, "name"])
            .map_or_else(|| id.to_owned(), str::to_owned)
    }

    /// Free-form per-store tuning — `EDGEZERO__STORES__<KIND>__<ID>__<KEY>`.
    #[must_use]
    #[inline]
    pub fn store_setting(&self, kind: &str, id: &str, key: &str) -> Option<&str> {
        self.get(&["stores", kind, id, key])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EnvConfig {
        EnvConfig::from_vars([
            ("EDGEZERO__STORES__KV__SESSIONS__NAME", "prod-sessions"),
            ("EDGEZERO__STORES__KV__SESSIONS__MAX_LIST_KEYS", "500"),
            ("EDGEZERO__ADAPTER__HOST", "0.0.0.0"),
            ("EDGEZERO__ADAPTER__PORT", "9000"),
            ("EDGEZERO__LOGGING__LEVEL", "debug"),
            ("PATH", "/usr/bin"),
        ])
    }

    #[test]
    fn parses_and_lower_cases_segments() {
        let cfg = sample();
        assert_eq!(
            cfg.get(&["stores", "kv", "sessions", "name"]),
            Some("prod-sessions")
        );
    }

    #[test]
    fn get_is_case_insensitive() {
        let cfg = sample();
        assert_eq!(
            cfg.get(&["STORES", "KV", "Sessions", "NAME"]),
            Some("prod-sessions")
        );
    }

    #[test]
    fn store_name_hit() {
        let cfg = sample();
        assert_eq!(cfg.store_name("kv", "sessions"), "prod-sessions");
    }

    #[test]
    fn store_name_falls_back_to_id() {
        let cfg = sample();
        assert_eq!(cfg.store_name("kv", "cache"), "cache");
    }

    #[test]
    fn store_setting_lookup() {
        let cfg = sample();
        assert_eq!(
            cfg.store_setting("kv", "sessions", "max_list_keys"),
            Some("500")
        );
        assert_eq!(cfg.store_setting("kv", "sessions", "ttl"), None);
    }

    #[test]
    fn adapter_and_logging_accessors() {
        let cfg = sample();
        assert_eq!(cfg.adapter_host(), Some("0.0.0.0"));
        assert_eq!(cfg.adapter_port(), Some("9000"));
        assert_eq!(cfg.logging_level(), Some("debug"));
    }

    #[test]
    fn empty_config_returns_none_and_fallbacks() {
        let empty: [(&str, &str); 0] = [];
        let cfg = EnvConfig::from_vars(empty);
        assert_eq!(cfg.adapter_host(), None);
        assert_eq!(cfg.adapter_port(), None);
        assert_eq!(cfg.logging_level(), None);
        assert_eq!(cfg.store_setting("kv", "sessions", "name"), None);
        assert_eq!(cfg.get(&["stores", "kv", "sessions", "name"]), None);
        assert_eq!(cfg.store_name("kv", "sessions"), "sessions");
    }

    #[test]
    fn non_prefixed_variable_is_ignored() {
        let cfg = EnvConfig::from_vars([
            ("PATH", "/usr/bin"),
            ("EDGEZERO_HOST", "ignored-no-double-underscore"),
            ("EDGEZERO__ADAPTER__HOST", "kept"),
        ]);
        assert_eq!(cfg.adapter_host(), Some("kept"));
        assert_eq!(cfg.get(&["host"]), None);
    }

    #[test]
    fn malformed_variables_are_skipped() {
        // `EDGEZERO__` alone, a trailing `__`, and an interior empty segment
        // must all be skipped without panicking.
        let cfg = EnvConfig::from_vars([
            ("EDGEZERO__", "empty"),
            ("EDGEZERO__ADAPTER__", "trailing"),
            ("EDGEZERO__ADAPTER____PORT", "interior-empty"),
            ("EDGEZERO__ADAPTER__HOST", "good"),
        ]);
        assert_eq!(cfg.adapter_host(), Some("good"));
        assert_eq!(cfg.adapter_port(), None);
        assert_eq!(cfg.get(&["adapter"]), None);
    }
}
