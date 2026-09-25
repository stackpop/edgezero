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

    /// `EDGEZERO__LOGGING__ENDPOINT`. Adapters that wire a platform-specific
    /// logger (e.g. Fastly's named log endpoints) read this to know which
    /// endpoint to attach to; a `None` value means "don't init a platform
    /// logger" — useful under local emulators (Viceroy) that reject reserved
    /// names like `stdout`.
    #[must_use]
    #[inline]
    pub fn logging_endpoint(&self) -> Option<&str> {
        self.get(&["logging", "endpoint"])
    }

    /// `EDGEZERO__LOGGING__LEVEL`.
    #[must_use]
    #[inline]
    pub fn logging_level(&self) -> Option<&str> {
        self.get(&["logging", "level"])
    }

    /// Key for a logical store — `EDGEZERO__STORES__<KIND>__<ID>__KEY` —
    /// falling back to `id` itself when unset, blank, whitespace-only, or
    /// containing control characters.
    ///
    /// This fallback form is intended for runtime registry construction. Code
    /// that may mutate provider state must use [`Self::store_key_checked`] so a
    /// present invalid selector fails before mutation.
    #[must_use]
    #[inline]
    pub fn store_key(&self, kind: &str, id: &str) -> String {
        self.get(&["stores", kind, id, "key"])
            .filter(|value| !is_blank_or_control(value))
            .map_or_else(|| id.to_owned(), str::to_owned)
    }

    /// Checked key for a logical store.
    ///
    /// An absent selector uses the logical ID. A present blank value or a
    /// value containing control characters is rejected so callers cannot
    /// mutate the fallback key and later fail stricter deployment validation.
    /// The error names the canonical variable without including its value.
    ///
    /// # Errors
    /// Returns an error when the canonical `__KEY` selector is present but
    /// invalid.
    #[inline]
    pub fn store_key_checked(&self, kind: &str, id: &str) -> Result<String, String> {
        self.store_selector_checked(kind, id, "key")?
            .map_or_else(|| Ok(id.to_owned()), |value| Ok(value.to_owned()))
    }

    /// Platform name for a logical store — `EDGEZERO__STORES__<KIND>__<ID>__NAME`
    /// — falling back to `id` itself when the variable is unset OR when
    /// the value is empty / whitespace-only. `kind` is `"kv"` /
    /// `"config"` / `"secrets"`.
    ///
    /// The empty/whitespace skip is deliberate: an env var like
    /// `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=` (set but blank)
    /// would otherwise flow into `wrangler kv namespace create ""`
    /// or `fastly config-store create --name=` or be written as
    /// the binding name in wrangler.toml -- all of which fail at
    /// the platform with confusing errors rather than the clear
    /// "did you forget to set the env var" message you'd expect.
    /// Falling back to the logical id is consistent with the
    /// "unset" path and gives the operator a working default.
    ///
    /// Control characters are similarly rejected because no
    /// platform (cloudflare bindings, fastly store names, spin
    /// labels) accepts them as resource identifiers.
    ///
    /// This fallback form is intended for runtime registry construction. Code
    /// that may mutate provider state must use [`Self::store_name_checked`] so
    /// a present invalid selector fails before mutation.
    #[must_use]
    #[inline]
    pub fn store_name(&self, kind: &str, id: &str) -> String {
        self.get(&["stores", kind, id, "name"])
            .filter(|value| !is_blank_or_control(value))
            .map_or_else(|| id.to_owned(), str::to_owned)
    }

    /// Checked platform name for a logical store.
    ///
    /// An absent selector defaults to `id`. A present blank value or a value
    /// containing control characters is rejected. The error names the
    /// canonical variable without including its value.
    ///
    /// # Errors
    /// Returns an error when the canonical `__NAME` selector is present but
    /// invalid.
    #[inline]
    pub fn store_name_checked(&self, kind: &str, id: &str) -> Result<String, String> {
        self.store_selector_checked(kind, id, "name")?
            .map_or_else(|| Ok(id.to_owned()), |value| Ok(value.to_owned()))
    }

    fn store_selector_checked<'value>(
        &'value self,
        kind: &str,
        id: &str,
        setting: &str,
    ) -> Result<Option<&'value str>, String> {
        let value = self.get(&["stores", kind, id, setting]);
        if value.is_some_and(is_blank_or_control) {
            return Err(format!(
                "EDGEZERO__STORES__{}__{}__{} is present but must be non-blank and contain no control characters (value redacted)",
                kind.to_ascii_uppercase(),
                id.to_ascii_uppercase(),
                setting.to_ascii_uppercase()
            ));
        }
        Ok(value)
    }

    /// Free-form per-store tuning — `EDGEZERO__STORES__<KIND>__<ID>__<KEY>`.
    #[must_use]
    #[inline]
    pub fn store_setting(&self, kind: &str, id: &str, key: &str) -> Option<&str> {
        self.get(&["stores", kind, id, key])
    }
}

/// Merge manifest environment-variable defaults with parent-process values.
///
/// Entries from `parent` are applied last and therefore override defaults.
/// Recognised `EDGEZERO__` keys are normalised before the layers are merged so
/// segment casing cannot let a manifest default override the parent value that
/// [`EnvConfig::from_vars`] would resolve to the same path. Other environment
/// variable names retain their ordinary case-sensitive semantics. The returned
/// map is provider-neutral; callers may validate it or pass it to
/// [`EnvConfig::from_vars`].
#[must_use]
#[inline]
pub fn merge_env_defaults<DI, DK, DV, PI, PK, PV>(
    defaults: DI,
    parent: PI,
) -> BTreeMap<String, String>
where
    DI: IntoIterator<Item = (DK, DV)>,
    DK: AsRef<str>,
    DV: AsRef<str>,
    PI: IntoIterator<Item = (PK, PV)>,
    PK: AsRef<str>,
    PV: AsRef<str>,
{
    let mut merged = defaults
        .into_iter()
        .map(|(key, value)| {
            (
                normalized_merge_key(key.as_ref()),
                value.as_ref().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    merged.extend(parent.into_iter().map(|(key, value)| {
        (
            normalized_merge_key(key.as_ref()),
            value.as_ref().to_owned(),
        )
    }));
    merged
}

fn normalized_merge_key(key: &str) -> String {
    if key.starts_with(PREFIX) {
        key.to_ascii_uppercase()
    } else {
        key.to_owned()
    }
}

/// `true` if `value` is empty, made entirely of whitespace, or
/// contains any ASCII / Unicode control character. Used to reject
/// platform-name overrides that would otherwise flow as empty
/// strings (or control chars) into platform-side resource names.
fn is_blank_or_control(value: &str) -> bool {
    value.is_empty()
        || value.chars().all(char::is_whitespace)
        || value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_env_defaults_applies_parent_values_last() {
        let merged = merge_env_defaults(
            [
                (
                    "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME",
                    "manifest-name",
                ),
                ("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "manifest-key"),
            ],
            [
                ("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "parent-name"),
                ("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "parent-key"),
            ],
        );

        assert_eq!(
            merged.get("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME"),
            Some(&"parent-name".to_owned())
        );
        assert_eq!(
            merged.get("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"),
            Some(&"parent-key".to_owned())
        );
    }

    #[test]
    fn merge_env_defaults_applies_parent_precedence_after_selector_normalization() {
        let merged = merge_env_defaults(
            [(
                "EDGEZERO__stores__config__app_config__name",
                "manifest-name",
            )],
            [("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "parent-name")],
        );
        let config = EnvConfig::from_vars(merged);

        assert_eq!(
            config.store_name_checked("config", "app_config"),
            Ok("parent-name".to_owned())
        );
    }

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
    fn store_name_falls_back_to_id_when_env_value_is_empty() {
        // An exported but empty `EDGEZERO__STORES__<KIND>__<ID>__NAME=`
        // would otherwise flow into a platform `create` call with
        // an empty name and a binding written as `binding = ""` in
        // wrangler.toml. Treat it the same as unset.
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", "")]);
        assert_eq!(cfg.store_name("kv", "sessions"), "sessions");
    }

    #[test]
    fn store_name_falls_back_to_id_when_env_value_is_whitespace_only() {
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", "   \t  ")]);
        assert_eq!(cfg.store_name("kv", "sessions"), "sessions");
    }

    #[test]
    fn store_name_falls_back_to_id_when_env_value_has_control_chars() {
        // A literal newline or NUL embedded in the override would
        // be passed through to `wrangler kv namespace create
        // <name>` and similar. Reject and fall back to the id.
        let with_newline =
            EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", "prod\nname")]);
        assert_eq!(with_newline.store_name("kv", "sessions"), "sessions");
        let with_nul =
            EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", "prod\x00name")]);
        assert_eq!(with_nul.store_name("kv", "sessions"), "sessions");
    }

    #[test]
    fn checked_store_name_rejects_present_invalid_values_without_disclosing_them() {
        for invalid in ["", "   \t  ", "sensitive\nname", "sensitive\0name"] {
            let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", invalid)]);
            let error = cfg
                .store_name_checked("kv", "sessions")
                .expect_err("a present invalid selector must not fall back");
            assert!(
                error.contains("EDGEZERO__STORES__KV__SESSIONS__NAME"),
                "the diagnostic must identify the canonical variable: {error}"
            );
            assert!(
                invalid.is_empty() || !error.contains(invalid),
                "the diagnostic must redact the selector value: {error}"
            );
        }
    }

    #[test]
    fn checked_store_name_defaults_only_when_selector_is_absent() {
        let cfg = EnvConfig::default();
        assert_eq!(
            cfg.store_name_checked("config", "app_config"),
            Ok("app_config".to_owned())
        );
    }

    #[test]
    fn store_name_accepts_real_world_punctuation() {
        // Underscores, dashes, and dots are valid in every platform
        // store-name we target. Don't false-reject them.
        let cfg = EnvConfig::from_vars([(
            "EDGEZERO__STORES__KV__SESSIONS__NAME",
            "prod-app_v2.sessions",
        )]);
        assert_eq!(cfg.store_name("kv", "sessions"), "prod-app_v2.sessions");
    }

    #[test]
    fn store_key_returns_env_var_when_set() {
        let cfg = EnvConfig::from_vars([(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
            "app_config_staging",
        )]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config_staging");
    }

    #[test]
    fn store_key_falls_back_to_id_when_unset() {
        let empty: [(&str, &str); 0] = [];
        let cfg = EnvConfig::from_vars(empty);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }

    #[test]
    fn store_key_falls_back_on_blank_value() {
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "   ")]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }

    #[test]
    fn store_key_falls_back_on_control_chars() {
        let cfg =
            EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "bad\x01key")]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }

    #[test]
    fn checked_store_key_rejects_present_invalid_values_without_disclosing_them() {
        for invalid in ["", "   \t  ", "sensitive\nkey", "sensitive\0key"] {
            let cfg =
                EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", invalid)]);
            let error = cfg
                .store_key_checked("config", "app_config")
                .expect_err("a present invalid selector must not fall back");
            assert!(
                error.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"),
                "the diagnostic must identify the canonical variable: {error}"
            );
            assert!(
                invalid.is_empty() || !error.contains(invalid),
                "the diagnostic must redact the selector value: {error}"
            );
        }
    }

    #[test]
    fn checked_store_key_defaults_to_logical_id_only_when_selector_is_absent() {
        let cfg = EnvConfig::default();
        assert_eq!(
            cfg.store_key_checked("config", "app_config"),
            Ok("app_config".to_owned())
        );
    }

    #[test]
    fn checked_store_key_uses_canonical_environment_value() {
        let cfg = EnvConfig::from_vars([(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
            "publisher-selected",
        )]);
        assert_eq!(
            cfg.store_key_checked("config", "app_config"),
            Ok("publisher-selected".to_owned())
        );
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
