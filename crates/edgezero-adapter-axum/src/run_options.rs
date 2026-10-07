use std::env::vars_os;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use edgezero_core::addr;
use edgezero_core::app::StoresMetadata;
use edgezero_core::env_config::EnvConfig;
use log::LevelFilter;

use crate::diagnostics::NativeDiagnosticsHandle;
use crate::proxy::TrustedProxyPolicy;

/// Validated native JSON buffering ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JsonBodyLimit(NonZeroUsize);

impl JsonBodyLimit {
    pub(crate) const DEFAULT: Self =
        Self(NonZeroUsize::new(0x0020_0000).expect("positive default"));

    pub(crate) fn from_config(env: &EnvConfig) -> anyhow::Result<Self> {
        let Some(raw) = env.get(&["adapter", "json_body_limit_bytes"]) else {
            return Ok(Self::DEFAULT);
        };
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Self::invalid());
        }
        Self::new(raw.parse::<usize>().map_err(|_error| Self::invalid())?)
    }

    pub(crate) fn get(self) -> NonZeroUsize {
        self.0
    }

    fn invalid() -> anyhow::Error {
        failure(
            "settings",
            "JSON_BODY_LIMIT_BYTES",
            "invalid positive representable integer bytes",
        )
    }
    pub(crate) fn new(limit: usize) -> anyhow::Result<Self> {
        NonZeroUsize::new(limit)
            .filter(|value| value.get() <= isize::MAX.unsigned_abs())
            .map(Self)
            .ok_or_else(Self::invalid)
    }
}

/// Validated explicit production hosting options. Construction never selects a
/// policy from build profile, cwd or container detection. Explicit constructors
/// do not merge environment values; only `from_env` reads hosting environment.
#[derive(Clone)]
pub struct AxumRunOptions {
    addr: SocketAddr,
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    diagnostics: NativeDiagnosticsHandle,
    env: EnvConfig,
    grace: Duration,
    json_body_limit: JsonBodyLimit,
    level: LevelFilter,
    non_unicode_overrides: Vec<Vec<String>>,
    request_records: bool,
    trusted_proxy_policy: TrustedProxyPolicy,
}

impl AxumRunOptions {
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn config_dir(&self) -> Option<&Path> {
        self.config_dir.as_deref()
    }

    pub(crate) fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    pub(crate) fn diagnostics(&self) -> NativeDiagnosticsHandle {
        self.diagnostics.clone()
    }

    pub(crate) fn env_config(&self) -> &EnvConfig {
        &self.env
    }

    /// Captures hosting settings and store overrides once. Secret values still
    /// use the existing environment-backed secret backend.
    ///
    /// # Errors
    /// Returns safe setting categories for supplied invalid hosting settings.
    #[inline]
    pub fn from_env() -> anyhow::Result<Self> {
        let mut pairs = Vec::new();
        let mut non_unicode = Vec::new();
        for (os_key, os_value) in vars_os() {
            let Some(key) = os_key.to_str() else { continue };
            let Some(rest) = key.strip_prefix("EDGEZERO__") else {
                continue;
            };
            let path: Vec<String> = rest.split("__").map(str::to_ascii_lowercase).collect();
            if let Some(value) = os_value.to_str() {
                pairs.push((key.to_owned(), value.to_owned()));
            } else if let Some(setting) = fixed_setting(&path) {
                return Err(failure("settings", setting, "non-Unicode value"));
            } else {
                let segments: Vec<&str> = path.iter().map(String::as_str).collect();
                if matches!(
                    segments.as_slice(),
                    ["stores", "config" | "kv" | "secrets", _, "name" | "key"]
                ) {
                    non_unicode.push(path);
                }
            }
        }
        let mut options = Self::from_vars(pairs)?;
        options.non_unicode_overrides = non_unicode;
        Ok(options)
    }

    /// Constructs options using only supplied string pairs and omission defaults.
    /// Declared-store overrides are validated against baked metadata at startup.
    ///
    /// # Errors
    /// Returns safe setting categories for invalid bind, logging, roots or grace.
    #[inline]
    pub fn from_vars<I, K, V>(vars: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let env = EnvConfig::from_vars(vars);
        let host = env.adapter_host().map_or(Ok(addr::DEFAULT_HOST), |raw| {
            raw.parse()
                .map_err(|_error| failure("settings", "HOST", "invalid IP literal"))
        })?;
        let port = env.adapter_port().map_or(Ok(addr::DEFAULT_PORT), |raw| {
            raw.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| failure("settings", "PORT", "invalid nonzero port"))
        })?;
        let mut options = Self::new(SocketAddr::new(host, port))?;
        if let Some(level) = env.logging_level() {
            options.level = LevelFilter::from_str(level)
                .map_err(|_error| failure("settings", "LOGGING__LEVEL", "invalid level"))?;
        }
        options.grace = shutdown_grace(&env)?;
        options.json_body_limit = JsonBodyLimit::from_config(&env)?;
        (options.trusted_proxy_policy, options.request_records) = native_settings(&env)?;
        if let Some(root) = env.get(&["adapter", "config_dir"]) {
            options = options.with_config_dir(root)?;
        }
        if let Some(root) = env.get(&["adapter", "data_dir"]) {
            options = options.with_data_dir(root)?;
        }
        options.env = env;
        Ok(options)
    }

    pub(crate) fn json_body_limit(&self) -> JsonBodyLimit {
        self.json_body_limit
    }

    pub(crate) fn logging_level(&self) -> LevelFilter {
        self.level
    }

    /// Explicit bind with default grace/logging, no roots, and no store overrides.
    ///
    /// # Errors
    /// Rejects port zero. Production omission defaults apply only to omitted values.
    #[inline]
    pub fn new(addr: SocketAddr) -> anyhow::Result<Self> {
        if addr.port() == 0 {
            return Err(failure("settings", "PORT", "invalid nonzero port"));
        }
        Ok(Self {
            addr,
            config_dir: None,
            data_dir: None,
            diagnostics: NativeDiagnosticsHandle::new(),
            env: EnvConfig::default(),
            grace: Duration::from_secs(10),
            json_body_limit: JsonBodyLimit::DEFAULT,
            level: LevelFilter::Info,
            non_unicode_overrides: Vec::new(),
            request_records: true,
            trusted_proxy_policy: TrustedProxyPolicy::no_trust(),
        })
    }

    pub(crate) fn request_records(&self) -> bool {
        self.request_records
    }

    pub(crate) fn shutdown_grace(&self) -> Duration {
        self.grace
    }

    pub(crate) fn trusted_proxy_policy(&self) -> TrustedProxyPolicy {
        self.trusted_proxy_policy.clone()
    }

    pub(crate) fn validate_stores(&self, metadata: StoresMetadata) -> anyhow::Result<()> {
        for (kind, declaration) in [
            ("config", metadata.config),
            ("kv", metadata.kv),
            ("secrets", metadata.secrets),
        ] {
            if let Some(meta) = declaration {
                for id in meta.ids {
                    for field in ["name", "key"] {
                        if self
                            .non_unicode_overrides
                            .iter()
                            .any(|path| match path.as_slice() {
                                [_, selected_kind, selected_id, selected_field] => {
                                    selected_kind == kind
                                        && selected_id.eq_ignore_ascii_case(id)
                                        && selected_field == field
                                }
                                _ => false,
                            })
                        {
                            return Err(failure("settings", id, "non-Unicode store override"));
                        }
                        if self.env.store_setting(kind, id, field).is_some_and(|raw| {
                            raw.is_empty()
                                || raw.chars().all(char::is_whitespace)
                                || raw.chars().any(char::is_control)
                        }) {
                            return Err(failure("settings", id, "invalid store NAME/KEY override"));
                        }
                    }
                }
            }
        }
        for (meta, root, label) in [
            (metadata.config, self.config_dir.as_deref(), "CONFIG_DIR"),
            (metadata.kv, self.data_dir.as_deref(), "DATA_DIR"),
        ] {
            if meta.is_some() && root.is_none() {
                return Err(failure("settings", label, "required for declared stores"));
            }
            if let Some(directory) = root {
                validate_root(directory, label)?;
            }
        }
        Ok(())
    }

    /// Selects an existing absolute config root; never creates it.
    ///
    /// # Errors
    /// Rejects relative, missing or non-directory roots.
    #[inline]
    pub fn with_config_dir<Root: Into<PathBuf>>(mut self, root: Root) -> anyhow::Result<Self> {
        let directory = root.into();
        validate_root(&directory, "CONFIG_DIR")?;
        self.config_dir = Some(directory);
        Ok(self)
    }

    /// Selects an existing absolute data root. Startup also checks actual write access.
    ///
    /// # Errors
    /// Rejects relative, missing or non-directory roots.
    #[inline]
    pub fn with_data_dir<Root: Into<PathBuf>>(mut self, root: Root) -> anyhow::Result<Self> {
        let directory = root.into();
        validate_root(&directory, "DATA_DIR")?;
        self.data_dir = Some(directory);
        Ok(self)
    }

    /// Shares native counters and the optional observer with this runner.
    #[inline]
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: NativeDiagnosticsHandle) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Selects the ceiling for framework-managed JSON buffering, in bytes.
    ///
    /// # Errors
    /// Rejects zero and values above the collector's representable capacity.
    #[inline]
    pub fn with_json_body_limit_bytes(mut self, limit: usize) -> anyhow::Result<Self> {
        self.json_body_limit = JsonBodyLimit::new(limit)?;
        Ok(self)
    }

    #[inline]
    #[must_use]
    pub fn with_logging_level(mut self, level: LevelFilter) -> Self {
        self.level = level;
        self
    }

    /// Enables only default native request logs; observers and accounting remain active.
    #[inline]
    #[must_use]
    pub fn with_request_records(mut self, enabled: bool) -> Self {
        self.request_records = enabled;
        self
    }

    /// Selects a positive, representable integer-second drain budget.
    ///
    /// # Errors
    /// Rejects zero, fractional seconds and unusable native timer values.
    #[inline]
    pub fn with_shutdown_grace(mut self, grace: Duration) -> anyhow::Result<Self> {
        validate_grace(grace)?;
        self.grace = grace;
        Ok(self)
    }

    /// Installs an explicit immutable incoming-proxy policy without reading the environment.
    #[inline]
    #[must_use]
    pub fn with_trusted_proxy_policy(mut self, policy: TrustedProxyPolicy) -> Self {
        self.trusted_proxy_policy = policy;
        self
    }
}

pub(crate) fn native_settings(env: &EnvConfig) -> anyhow::Result<(TrustedProxyPolicy, bool)> {
    let policy = TrustedProxyPolicy::from_values(
        env.get(&["adapter", "trusted_proxy_cidrs"]),
        env.get(&["adapter", "forwarding_header_family"]),
    )
    .map_err(|_error| {
        failure(
            "settings",
            "TRUSTED_PROXY_POLICY",
            "invalid literal networks or forwarding family",
        )
    })?;
    let records = match env.get(&["logging", "request_records"]) {
        None | Some("true") => true,
        Some("false") => false,
        Some(_) => {
            return Err(failure(
                "settings",
                "LOGGING__REQUEST_RECORDS",
                "invalid boolean",
            ));
        }
    };
    Ok((policy, records))
}

pub(crate) fn failure(stage: &str, subject: &str, category: &str) -> anyhow::Error {
    // No source chain survives this boundary. Subject is a fixed setting name or
    // baked logical id, never settings/config/secret contents or a provider string.
    anyhow::anyhow!("startup {stage}: {subject}: {category}")
}

fn fixed_setting(path: &[String]) -> Option<&'static str> {
    let segments: Vec<&str> = path.iter().map(String::as_str).collect();
    match segments.as_slice() {
        ["adapter", "host"] => Some("HOST"),
        ["adapter", "port"] => Some("PORT"),
        ["adapter", "config_dir"] => Some("CONFIG_DIR"),
        ["adapter", "data_dir"] => Some("DATA_DIR"),
        ["adapter", "shutdown_grace_seconds"] => Some("SHUTDOWN_GRACE_SECONDS"),
        ["adapter", "trusted_proxy_cidrs"] => Some("TRUSTED_PROXY_CIDRS"),
        ["adapter", "forwarding_header_family"] => Some("FORWARDING_HEADER_FAMILY"),
        ["logging", "request_records"] => Some("LOGGING__REQUEST_RECORDS"),
        ["adapter", "json_body_limit_bytes"] => Some("JSON_BODY_LIMIT_BYTES"),
        ["logging", "level"] => Some("LOGGING__LEVEL"),
        _ => None,
    }
}

pub(crate) fn shutdown_grace(env: &EnvConfig) -> anyhow::Result<Duration> {
    let Some(raw) = env.get(&["adapter", "shutdown_grace_seconds"]) else {
        return Ok(Duration::from_secs(10));
    };
    let seconds = raw
        .parse::<u64>()
        .ok()
        .filter(|_seconds| raw.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| {
            failure(
                "settings",
                "SHUTDOWN_GRACE_SECONDS",
                "invalid positive integer seconds",
            )
        })?;
    let grace = Duration::from_secs(seconds);
    validate_grace(grace)?;
    Ok(grace)
}

fn validate_grace(grace: Duration) -> anyhow::Result<()> {
    if grace.is_zero() || grace.subsec_nanos() != 0 || Instant::now().checked_add(grace).is_none() {
        return Err(failure(
            "settings",
            "SHUTDOWN_GRACE_SECONDS",
            "invalid or unusable positive integer seconds",
        ));
    }
    Ok(())
}

fn validate_root(root: &Path, setting: &str) -> anyhow::Result<()> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(failure(
            "settings",
            setting,
            "requires an absolute existing directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::StoreMetadata;

    #[test]
    fn native_settings_defaults_explicit_builders_and_strict_values() {
        let defaults = AxumRunOptions::from_vars(Vec::<(String, String)>::new()).expect("defaults");
        assert!(defaults.request_records());
        assert_eq!(
            defaults.trusted_proxy_policy(),
            TrustedProxyPolicy::no_trust()
        );
        let configured = AxumRunOptions::from_vars([
            ("EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS", "127.0.0.2/32"),
            ("EDGEZERO__ADAPTER__FORWARDING_HEADER_FAMILY", "forwarded"),
            ("EDGEZERO__LOGGING__REQUEST_RECORDS", "false"),
        ])
        .expect("configured");
        assert!(!configured.request_records());
        assert_ne!(
            configured.trusted_proxy_policy(),
            TrustedProxyPolicy::no_trust()
        );
        let explicit = AxumRunOptions::new(defaults.addr())
            .expect("explicit")
            .with_request_records(false)
            .with_trusted_proxy_policy(configured.trusted_proxy_policy());
        assert!(!explicit.request_records());
        assert_eq!(
            explicit.trusted_proxy_policy(),
            configured.trusted_proxy_policy()
        );
        assert!(
            explicit
                .env_config()
                .get(&["adapter", "trusted_proxy_cidrs"])
                .is_none()
        );
        for (key, value) in [
            ("EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS", "127.0.0.2"),
            ("EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS", "sentinel-policy"),
            (
                "EDGEZERO__ADAPTER__FORWARDING_HEADER_FAMILY",
                "sentinel-family",
            ),
            ("EDGEZERO__LOGGING__REQUEST_RECORDS", "TRUE"),
            ("EDGEZERO__LOGGING__REQUEST_RECORDS", "false "),
            ("EDGEZERO__LOGGING__REQUEST_RECORDS", ""),
        ] {
            let error = AxumRunOptions::from_vars([(key, value)])
                .err()
                .expect("invalid supplied setting");
            assert_eq!(error.chain().count(), 1);
            assert!(!error.to_string().contains("sentinel"));
        }
    }

    #[test]
    fn json_body_limit_is_positive_decimal_and_representable() {
        let key = "EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES";
        let defaults = AxumRunOptions::from_vars(Vec::<(String, String)>::new()).expect("defaults");
        assert_eq!(defaults.json_body_limit.get().get(), 0x0020_0000);
        for bytes in [
            1,
            64,
            9 * 1024 * 1024,
            usize::try_from(isize::MAX).expect("maximum"),
        ] {
            let options = AxumRunOptions::from_vars([(key, bytes.to_string())]).expect("valid");
            assert_eq!(options.json_body_limit.get().get(), bytes);
            assert_eq!(
                defaults
                    .clone()
                    .with_json_body_limit_bytes(bytes)
                    .expect("builder")
                    .json_body_limit
                    .get()
                    .get(),
                bytes
            );
        }
        for raw in [
            "",
            "0",
            "-1",
            "+1",
            " 64",
            "64 ",
            "1.5",
            "2MiB",
            "sentinel-limit",
            "9999999999999999999999999999999999",
        ] {
            let error = AxumRunOptions::from_vars([(key, raw)])
                .err()
                .expect("invalid");
            assert!(error.to_string().contains("JSON_BODY_LIMIT_BYTES"));
            assert!(!format!("{error:?}").contains("sentinel"));
            assert_eq!(error.chain().count(), 1);
        }
        assert!(defaults.clone().with_json_body_limit_bytes(0).is_err());
        assert!(defaults.with_json_body_limit_bytes(usize::MAX).is_err());
    }

    #[test]
    fn defaults_and_invalid_supplied_scalars_are_distinct() {
        let options = AxumRunOptions::from_vars(Vec::<(String, String)>::new()).expect("defaults");
        assert_eq!(
            options.addr,
            SocketAddr::from((addr::DEFAULT_HOST, addr::DEFAULT_PORT))
        );
        assert_eq!(options.grace, Duration::from_secs(10));
        for (key, values) in [
            (
                "EDGEZERO__ADAPTER__HOST",
                vec!["sentinel-host", "", "localhost"],
            ),
            (
                "EDGEZERO__ADAPTER__PORT",
                vec!["0", "65536", "sentinel-port", ""],
            ),
            ("EDGEZERO__LOGGING__LEVEL", vec!["sentinel-level", ""]),
            (
                "EDGEZERO__ADAPTER__SHUTDOWN_GRACE_SECONDS",
                vec![
                    "0",
                    "",
                    "-1",
                    "+1",
                    "sentinel-duration",
                    "18446744073709551615",
                ],
            ),
        ] {
            for value in values {
                let error = AxumRunOptions::from_vars([(key, value)])
                    .err()
                    .expect("invalid");
                assert!(error.chain().count() == 1);
                assert!(!format!("{error:?}").contains("sentinel"));
            }
        }
    }

    #[test]
    fn selected_override_validation_precedes_normalization() {
        let metadata = StoresMetadata {
            config: Some(StoreMetadata {
                ids: &["settings"],
                default: "settings",
            }),
            ..StoresMetadata::default()
        };
        let root = tempfile::tempdir().expect("root");
        for value in ["", "   ", "sentinel\n"] {
            let options = AxumRunOptions::from_vars([
                (
                    "EDGEZERO__ADAPTER__CONFIG_DIR",
                    root.path().to_str().expect("path"),
                ),
                ("EDGEZERO__STORES__CONFIG__SETTINGS__KEY", value),
                ("EDGEZERO__STORES__KV__UNDECLARED__NAME", "\n"),
            ])
            .expect("native settings");
            let error = options
                .validate_stores(metadata)
                .expect_err("declared invalid override");
            assert!(!format!("{error:?}").contains("sentinel"));
        }
        let options =
            AxumRunOptions::new(SocketAddr::from(([127, 0, 0, 1], 1234))).expect("explicit");
        assert!(options.env.adapter_host().is_none());
        assert_eq!(options.env.store_key("config", "settings"), "settings");
        assert!(options.validate_stores(metadata).is_err());
    }
}
