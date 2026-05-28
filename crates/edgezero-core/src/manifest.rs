use log::LevelFilter;
use serde::de::Error as DeError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs, io};
use validator::{Validate, ValidationError};

/// Default config store / binding name used when `[stores.config]` is omitted.
pub const DEFAULT_CONFIG_STORE_NAME: &str = "EDGEZERO_CONFIG";
/// Default KV store / binding name used when `[stores.kv]` is omitted.
pub const DEFAULT_KV_STORE_NAME: &str = "EDGEZERO_KV";
/// Default secret store / binding name used when `[stores.secrets]` is omitted.
pub const DEFAULT_SECRET_STORE_NAME: &str = "EDGEZERO_SECRETS";

pub struct ManifestLoader {
    manifest: Arc<Manifest>,
}

impl ManifestLoader {
    /// # Errors
    /// Returns an [`io::Error`] if `path` cannot be read, or the file content cannot be parsed/validated as an `EdgeZero` manifest.
    #[inline]
    pub fn from_path(path: &Path) -> Result<Self, io::Error> {
        let contents = fs::read_to_string(path)?;
        let mut manifest: Manifest = toml::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let cwd = env::current_dir()?;
        let root_path = resolve_root_path(path, &cwd);
        manifest.root = Some(root_path);
        manifest
            .validate()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        manifest.finalize();
        Ok(Self {
            manifest: Arc::new(manifest),
        })
    }

    /// Loads a manifest from a static, in-process TOML string —
    /// fixture data in tests, build-time compile-checks, and the
    /// `app!` macro's compile-time consumption are the in-tree callers.
    /// The portable store-registry rewrite removed the per-adapter
    /// `run_app(include_str!("edgezero.toml"), …)` shape, so an adapter
    /// binary no longer carries the manifest at runtime; the portable
    /// store registry it would have extracted is baked into
    /// `Hooks::stores()` by the macro instead.
    ///
    /// # Panics
    /// Panics if `contents` is not valid TOML or fails validation.
    /// Because `contents` is statically known to the caller (a
    /// compile-time literal in the macro / tests), a parse failure
    /// indicates corruption that can't be recovered at runtime, and
    /// surfacing it as a clear panic is the right behaviour. Callers
    /// with a fallible input source (file paths, network, user input)
    /// should use [`ManifestLoader::try_load_from_str`] or
    /// [`ManifestLoader::from_path`].
    #[expect(
        clippy::panic,
        reason = "load_from_str only consumes statically-known manifest \
                  literals (macro/tests); a parse error means the caller's \
                  static input is corrupt and cannot recover"
    )]
    #[must_use]
    #[inline]
    pub fn load_from_str(contents: &str) -> Self {
        Self::try_load_from_str(contents).unwrap_or_else(|err| panic!("invalid manifest: {err}"))
    }

    #[must_use]
    #[inline]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// # Errors
    /// Returns an [`io::Error`] if `contents` is not valid TOML or fails manifest validation.
    #[inline]
    pub fn try_load_from_str(contents: &str) -> Result<Self, io::Error> {
        let mut manifest: Manifest = toml::from_str(contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        manifest
            .validate()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        manifest.finalize();
        Ok(Self {
            manifest: Arc::new(manifest),
        })
    }
}

#[derive(Debug, Deserialize, Validate)]
#[expect(
    clippy::partial_pub_fields,
    reason = "deserialized fields are pub for the public API; internal state is private"
)]
pub struct Manifest {
    #[serde(default)]
    #[validate(nested)]
    pub adapters: BTreeMap<String, ManifestAdapter>,
    #[serde(default)]
    #[validate(nested)]
    pub app: ManifestApp,
    #[serde(default)]
    #[validate(nested)]
    pub environment: ManifestEnvironment,
    #[serde(default)]
    #[validate(nested)]
    pub logging: ManifestLogging,
    #[serde(skip)]
    logging_resolved: BTreeMap<String, ResolvedLoggingConfig>,
    #[serde(skip)]
    root: Option<PathBuf>,
    #[serde(default)]
    #[validate(nested)]
    pub stores: ManifestStores,
    #[serde(default)]
    #[validate(nested)]
    pub triggers: ManifestTriggers,
}

impl Manifest {
    #[must_use]
    #[inline]
    pub fn environment(&self) -> &ManifestEnvironment {
        &self.environment
    }

    #[inline]
    pub fn environment_for(&self, adapter: &str) -> ResolvedEnvironment {
        let adapter_lower = adapter.to_ascii_lowercase();

        let variables = self
            .environment
            .variables
            .iter()
            .filter(|binding| binding.applies_to_adapter(&adapter_lower))
            .map(ResolvedEnvironmentBinding::from_manifest)
            .collect();

        let secrets = self
            .environment
            .secrets
            .iter()
            .filter(|binding| binding.applies_to_adapter(&adapter_lower))
            .map(ResolvedEnvironmentBinding::from_manifest)
            .collect();

        ResolvedEnvironment { secrets, variables }
    }

    pub(crate) fn finalize(&mut self) {
        let mut resolved = BTreeMap::new();

        for (adapter, cfg) in &self.adapters {
            if cfg.logging.is_specified() {
                resolved.insert(
                    adapter.clone(),
                    ResolvedLoggingConfig::from_manifest(&cfg.logging),
                );
            }
        }

        for (adapter, cfg) in &self.logging.adapters {
            resolved
                .entry(adapter.clone())
                .or_insert_with(|| ResolvedLoggingConfig::from_manifest(cfg));
        }

        self.logging_resolved = resolved;
    }

    /// Returns the KV store name for a given adapter.
    ///
    /// In the portable model the manifest carries no platform name; the name
    /// resolves to the declared default logical id, or `"EDGEZERO_KV"` when
    /// `[stores.kv]` is omitted.
    #[must_use]
    #[inline]
    pub fn kv_store_name(&self, _adapter: &str) -> &str {
        self.stores
            .kv
            .as_ref()
            .map_or(DEFAULT_KV_STORE_NAME, StoreDeclaration::default_id)
    }

    #[must_use]
    #[inline]
    pub fn logging_for(&self, adapter: &str) -> Option<&ResolvedLoggingConfig> {
        self.logging_resolved.get(adapter)
    }

    #[must_use]
    #[inline]
    pub fn logging_or_default(&self, adapter: &str) -> ResolvedLoggingConfig {
        self.logging_for(adapter).cloned().unwrap_or_default()
    }

    #[must_use]
    #[inline]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Returns the secret store binding identifier for a given adapter.
    ///
    /// In the portable model the manifest carries no platform name; the name
    /// resolves to the declared default logical id, or `"EDGEZERO_SECRETS"`
    /// when `[stores.secrets]` is omitted.
    #[must_use]
    #[inline]
    pub fn secret_store_binding(&self, _adapter: &str) -> &str {
        self.stores
            .secrets
            .as_ref()
            .map_or(DEFAULT_SECRET_STORE_NAME, StoreDeclaration::default_id)
    }

    /// Returns whether the secret store should be attached for a given adapter.
    ///
    /// True whenever a `[stores.secrets]` section is declared.
    #[must_use]
    #[inline]
    pub fn secret_store_enabled(&self, _adapter: &str) -> bool {
        self.stores.secrets.is_some()
    }
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestApp {
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub entry: Option<String>,
    #[serde(default)]
    pub middleware: Vec<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestTriggers {
    #[serde(default)]
    #[validate(nested)]
    pub http: Vec<ManifestHttpTrigger>,
}

#[derive(Clone, Debug, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestHttpTrigger {
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(rename = "body-mode")]
    #[serde(default)]
    pub body_mode: Option<BodyMode>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub description: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub handler: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub id: Option<String>,
    #[serde(default)]
    pub methods: Vec<HttpMethod>,
    #[validate(length(min = 1_u64))]
    pub path: String,
}

impl ManifestHttpTrigger {
    #[inline]
    pub fn methods(&self) -> Vec<&str> {
        if self.methods.is_empty() {
            vec!["GET"]
        } else {
            self.methods
                .iter()
                .copied()
                .map(HttpMethod::as_str)
                .collect()
        }
    }
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestEnvironment {
    #[serde(default)]
    #[validate(nested)]
    pub secrets: Vec<ManifestBinding>,
    #[serde(default)]
    #[validate(nested)]
    pub variables: Vec<ManifestBinding>,
}

#[derive(Debug, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestBinding {
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub description: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub env: Option<String>,
    #[validate(length(min = 1_u64))]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
}

impl ManifestBinding {
    fn applies_to_adapter(&self, adapter: &str) -> bool {
        if self.adapters.is_empty() {
            return true;
        }
        self.adapters
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(adapter))
    }

    fn env_key(&self) -> String {
        self.env.clone().unwrap_or_else(|| self.name.clone())
    }
}

impl ResolvedEnvironmentBinding {
    fn from_manifest(binding: &ManifestBinding) -> Self {
        Self {
            name: binding.name.clone(),
            description: binding.description.clone(),
            env: binding.env_key(),
            value: binding.value.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedEnvironmentBinding {
    pub description: Option<String>,
    pub env: String,
    pub name: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedEnvironment {
    pub secrets: Vec<ResolvedEnvironmentBinding>,
    pub variables: Vec<ResolvedEnvironmentBinding>,
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
#[validate(schema(function = "validate_manifest_adapter"))]
pub struct ManifestAdapter {
    #[serde(default)]
    #[validate(nested)]
    pub adapter: ManifestAdapterDefinition,
    #[serde(default)]
    #[validate(nested)]
    pub build: ManifestAdapterBuild,
    #[serde(default)]
    #[validate(nested)]
    pub commands: ManifestAdapterCommands,
    /// Catch-all for any sub-table other than the four canonical ones
    /// (`adapter`, `build`, `commands`, `logging`). The pre-rewrite
    /// `[adapters.<name>.stores.*]` tables land here and are rejected by
    /// [`validate_manifest_adapter`] with the migration-guide message.
    #[serde(flatten)]
    pub legacy: BTreeMap<String, toml::Value>,
    #[serde(default)]
    #[validate(nested)]
    pub logging: ManifestLoggingConfig,
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
#[validate(schema(function = "validate_manifest_adapter_definition"))]
pub struct ManifestAdapterDefinition {
    /// Spin component id, when the adapter's `manifest` (`spin.toml`) declares
    /// more than one `[component.*]`. Read by `provision` and
    /// `config push`; ignored at runtime. `config validate --strict`
    /// requires it when `spin.toml` declares multiple components.
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub component: Option<String>,
    #[serde(rename = "crate")]
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub crate_path: Option<String>,
    /// Bind address for the adapter server (e.g. `"0.0.0.0"` or `"127.0.0.1"`).
    ///
    /// Stored as a raw string so validation can be deferred until bind-address
    /// resolution, where environment-variable overrides and fallback behavior
    /// are applied consistently (see [`crate::addr::resolve_bind_addr`]).
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub host: Option<String>,
    /// Catch-all for any field other than the declared ones above. The
    /// portable manifest has no per-adapter runtime tuning surface, so an
    /// unknown key under `[adapters.<name>.adapter]` is rejected at load
    /// time rather than silently ignored.
    #[serde(flatten)]
    pub legacy: BTreeMap<String, toml::Value>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub manifest: Option<String>,
    /// Port for the adapter server.
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestAdapterBuild {
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub profile: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub target: Option<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestAdapterCommands {
    /// Per-project override for `edgezero auth login --adapter <name>`.
    /// `None` (the default) means "use the adapter's built-in
    /// command" — `wrangler login`, `fastly profile create`, etc.
    #[serde(default, rename = "auth-login")]
    #[validate(length(min = 1_u64))]
    pub auth_login: Option<String>,
    /// Per-project override for `edgezero auth logout --adapter <name>`.
    #[serde(default, rename = "auth-logout")]
    #[validate(length(min = 1_u64))]
    pub auth_logout: Option<String>,
    /// Per-project override for `edgezero auth status --adapter <name>`.
    #[serde(default, rename = "auth-status")]
    #[validate(length(min = 1_u64))]
    pub auth_status: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub build: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub deploy: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub serve: Option<String>,
}

// ---------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------

/// Top-level `[stores]` section.
#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestStores {
    #[serde(default)]
    #[validate(nested)]
    pub config: Option<StoreDeclaration>,
    #[serde(default)]
    #[validate(nested)]
    pub kv: Option<StoreDeclaration>,
    #[serde(default)]
    #[validate(nested)]
    pub secrets: Option<StoreDeclaration>,
}

/// Portable `[stores.<kind>]` declaration.
///
/// Declares logical store ids only — the portable fact that "this app uses a
/// KV/config/secrets store called `<id>`". No platform names, no per-adapter
/// tuning. Platform-specific runtime config (store names, tuning) is supplied
/// out of band; in this interim model a store's name resolves to its logical
/// [`StoreDeclaration::default_id`].
#[derive(Debug, Deserialize, Validate)]
#[non_exhaustive]
#[validate(schema(function = "validate_store_declaration"))]
pub struct StoreDeclaration {
    /// Logical default store id. Required when `ids.len() > 1`; when there is
    /// exactly one id it resolves to `ids[0]`.
    #[serde(default)]
    pub default: Option<String>,
    /// Logical store ids — non-empty (enforced in validation, not by serde, so
    /// a legacy manifest is rejected with the migration-guide message rather
    /// than a bare "missing field `ids`" parse error).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Any field other than `ids` / `default` — the pre-rewrite store schema
    /// (`name`, `enabled`, `adapters`, `defaults`) lands here and is rejected
    /// with a migration-guide message during validation.
    #[serde(flatten)]
    pub legacy: BTreeMap<String, toml::Value>,
}

impl StoreDeclaration {
    /// Resolve the config store name for a given adapter.
    ///
    /// In the portable model the manifest carries no platform name; the name
    /// resolves to the logical [`StoreDeclaration::default_id`].
    #[must_use]
    #[inline]
    pub fn config_store_name(&self, _adapter: &str) -> &str {
        self.default_id()
    }

    /// Resolve the default logical store id (the explicit `default`, else the
    /// first declared id).
    #[must_use]
    #[inline]
    pub fn default_id(&self) -> &str {
        self.default
            .as_deref()
            .or_else(|| self.ids.first().map(String::as_str))
            .unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Logging (unchanged)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Validate)]
#[non_exhaustive]
pub struct ManifestLogging {
    #[serde(flatten)]
    #[validate(nested)]
    pub adapters: BTreeMap<String, ManifestLoggingConfig>,
}

#[derive(Debug, Default, Deserialize, Clone, Validate)]
#[non_exhaustive]
pub struct ManifestLoggingConfig {
    #[serde(default)]
    pub echo_stdout: Option<bool>,
    #[serde(default)]
    #[validate(length(min = 1_u64))]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub level: Option<LogLevel>,
}

#[derive(Debug, Clone)]
pub struct ResolvedLoggingConfig {
    pub echo_stdout: Option<bool>,
    pub endpoint: Option<String>,
    pub level: LogLevel,
}

impl Default for ResolvedLoggingConfig {
    #[inline]
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            endpoint: None,
            echo_stdout: None,
        }
    }
}

impl ResolvedLoggingConfig {
    fn from_manifest(cfg: &ManifestLoggingConfig) -> Self {
        let mut resolved = Self::default();
        if let Some(level) = cfg.level {
            resolved.level = level;
        }
        if let Some(endpoint) = cfg.endpoint.as_ref() {
            resolved.endpoint = Some(endpoint.clone());
        }
        if let Some(echo_stdout) = cfg.echo_stdout {
            resolved.echo_stdout = Some(echo_stdout);
        }
        resolved
    }
}

impl ManifestLoggingConfig {
    fn is_specified(&self) -> bool {
        self.level.is_some() || self.endpoint.is_some() || self.echo_stdout.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpMethod {
    Delete,
    Get,
    Head,
    Options,
    Patch,
    Post,
    Put,
}

impl HttpMethod {
    #[must_use]
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Patch => "PATCH",
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }
}

// Serde's `Deserialize` trait has an optional `deserialize_in_place` method
// that defaults to `*place = Self::deserialize(deserializer)?`. For these
// small Copy/clone enums there is nothing to gain from spelling out an
// override — the default already does exactly the right thing.
#[expect(
    clippy::missing_trait_methods,
    reason = "default deserialize_in_place is identical to what we would write manually"
)]
impl<'de> Deserialize<'de> for HttpMethod {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "DELETE" => Ok(Self::Delete),
            "PATCH" => Ok(Self::Patch),
            "OPTIONS" => Ok(Self::Options),
            "HEAD" => Ok(Self::Head),
            other => Err(DeError::custom(format!(
                "unsupported HTTP method `{other}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BodyMode {
    Buffered,
    Stream,
}

// Serde's `Deserialize` trait has an optional `deserialize_in_place` method
// that defaults to `*place = Self::deserialize(deserializer)?`. For these
// small Copy/clone enums there is nothing to gain from spelling out an
// override — the default already does exactly the right thing.
#[expect(
    clippy::missing_trait_methods,
    reason = "default deserialize_in_place is identical to what we would write manually"
)]
impl<'de> Deserialize<'de> for BodyMode {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "buffered" => Ok(Self::Buffered),
            "stream" => Ok(Self::Stream),
            other => Err(DeError::custom(format!("unsupported body mode `{other}`"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
#[non_exhaustive]
pub enum LogLevel {
    Debug,
    Error,
    #[default]
    Info,
    Off,
    Trace,
    Warn,
}

impl LogLevel {
    #[must_use]
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Off => "off",
        }
    }
}

impl From<LogLevel> for LevelFilter {
    #[inline]
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => LevelFilter::Trace,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Error => LevelFilter::Error,
            LogLevel::Off => LevelFilter::Off,
        }
    }
}

// Serde's `Deserialize` trait has an optional `deserialize_in_place` method
// that defaults to `*place = Self::deserialize(deserializer)?`. For these
// small Copy/clone enums there is nothing to gain from spelling out an
// override — the default already does exactly the right thing.
#[expect(
    clippy::missing_trait_methods,
    reason = "default deserialize_in_place is identical to what we would write manually"
)]
impl<'de> Deserialize<'de> for LogLevel {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            "off" => Ok(Self::Off),
            other => Err(DeError::custom(format!(
                "logging level must be trace, debug, info, warn, error, or off (got `{other}`)"
            ))),
        }
    }
}

fn resolve_root_path(path: &Path, cwd: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => cwd.to_path_buf(),
        Some(parent) if parent.is_relative() => cwd.join(parent),
        Some(parent) => parent.to_path_buf(),
        None => cwd.to_path_buf(),
    }
}

/// Validates a single `[adapters.<name>.adapter]` block. The portable
/// manifest model lists the declared fields explicitly; an unknown key
/// would otherwise be silently dropped by serde, so we surface it as a
/// hard load error with the migration-guide pointer (consistent with the
/// hard-cutoff on `[stores.<kind>]` and `[adapters.<name>.<sub>]`).
fn validate_manifest_adapter_definition(
    definition: &ManifestAdapterDefinition,
) -> Result<(), ValidationError> {
    if !definition.legacy.is_empty() {
        let mut keys = definition.legacy.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut error = ValidationError::new("legacy_adapter_definition_schema");
        error.message = Some(
            format!(
                "unknown field(s) under `[adapters.<name>.adapter]`: {}. The portable \
                 manifest has no per-adapter runtime tuning surface beyond \
                 `component`, `crate`, `host`, `manifest`, `port` -- see \
                 docs/guide/manifest-store-migration.md",
                keys.join(", ")
            )
            .into(),
        );
        return Err(error);
    }
    Ok(())
}

/// Validates a single `[adapters.<name>]` block. The portable manifest model
/// has no per-adapter store / runtime tuning surface — all of that moved to
/// `EDGEZERO__*` env vars. The pre-rewrite
/// `[adapters.<name>.stores.<kind>]` tables and the legacy
/// `[adapters.<name>.adapter] runtime` block were silently ignored by the
/// deserializer before this hard-cutoff, so projects could carry over
/// stale entries without noticing.
fn validate_manifest_adapter(adapter: &ManifestAdapter) -> Result<(), ValidationError> {
    if !adapter.legacy.is_empty() {
        let mut keys = adapter.legacy.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut error = ValidationError::new("legacy_adapter_schema");
        error.message = Some(
            format!(
                "the pre-rewrite `[adapters.<name>.<key>]` subtables are no longer \
                 supported (offending field(s): {}); per-adapter store / runtime \
                 tuning moved to `EDGEZERO__*` env vars -- see \
                 docs/guide/manifest-store-migration.md",
                keys.join(", ")
            )
            .into(),
        );
        return Err(error);
    }
    Ok(())
}

/// Validates a single `[stores.<kind>]` declaration against the portable
/// schema.
///
/// Rejects the pre-rewrite store fields (`name`, `enabled`, `adapters`,
/// `defaults`) with an error pointing at the migration guide, and enforces the
/// `ids` / `default` invariants.
fn validate_store_declaration(declaration: &StoreDeclaration) -> Result<(), ValidationError> {
    if !declaration.legacy.is_empty() {
        let mut keys = declaration.legacy.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut error = ValidationError::new("legacy_store_schema");
        error.message = Some(
            format!(
                "the pre-rewrite `[stores.<kind>]` schema is no longer supported \
                 (offending field(s): {}); migrate to the portable `ids` / `default` \
                 form -- see docs/guide/manifest-store-migration.md",
                keys.join(", ")
            )
            .into(),
        );
        return Err(error);
    }

    if declaration.ids.is_empty() {
        let mut error = ValidationError::new("store_ids_empty");
        error.message =
            Some("`[stores.<kind>].ids` must declare at least one logical store id".into());
        return Err(error);
    }

    if declaration.ids.len() > 1 && declaration.default.is_none() {
        let mut error = ValidationError::new("store_default_required");
        error.message = Some(
            "`default` is required when `[stores.<kind>]` declares more than one id \
             -- see docs/guide/manifest-store-migration.md"
                .into(),
        );
        return Err(error);
    }

    if let Some(default) = declaration.default.as_deref() {
        if !declaration.ids.iter().any(|id| id == default) {
            let mut error = ValidationError::new("store_default_unknown");
            error.message =
                Some(format!("`default` (`{default}`) must be one of the declared `ids`").into());
            return Err(error);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process;
    use tempfile::{tempdir, tempdir_in, NamedTempFile};

    const SAMPLE: &str = r#"
[app]
name = "demo"
entry = "crates/demo-core"

[[triggers.http]]
path = "/"
methods = ["GET"]
handler = "demo::root"

[[triggers.http]]
path = "/echo"
methods = ["POST"]
handler = "demo::echo"

[environment]

[[environment.variables]]
name = "API_BASE_URL"
value = "https://example.com"
adapters = ["fastly"]

[[environment.secrets]]
name = "API_TOKEN"
env = "APP_TOKEN"
"#;

    #[test]
    fn parse_manifest_sample() {
        let loader = ManifestLoader::load_from_str(SAMPLE);
        let manifest = loader.manifest();
        assert_eq!(manifest.triggers.http.len(), 2);
        assert_eq!(manifest.app.name.as_deref(), Some("demo"));
    }

    #[test]
    fn try_load_from_str_rejects_invalid_toml() {
        let err = ManifestLoader::try_load_from_str("not a [valid manifest\n")
            .err()
            .expect("expected err");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().to_lowercase().contains("toml")
                || err.to_string().to_lowercase().contains("expected"),
            "expected toml-parse error message, got: {err}"
        );
    }

    #[test]
    fn try_load_from_str_rejects_failed_validation() {
        // `[stores.config]` requires a non-empty `ids` list; an empty list
        // trips `validator` and surfaces as InvalidData.
        let err = ManifestLoader::try_load_from_str(
            r#"
[app]
name = "demo"

[stores.config]
ids = []
"#,
        )
        .err()
        .expect("expected err");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn environment_resolves_for_adapters() {
        let loader = ManifestLoader::load_from_str(SAMPLE);
        let manifest = loader.manifest();

        let fastly = manifest.environment_for("fastly");
        assert_eq!(fastly.variables.len(), 1);
        assert_eq!(fastly.variables[0].env, "API_BASE_URL");
        assert_eq!(
            fastly.variables[0].value.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(fastly.secrets.len(), 1);
        assert_eq!(fastly.secrets[0].env, "APP_TOKEN");

        let cloudflare = manifest.environment_for("cloudflare");
        assert!(cloudflare.variables.is_empty());
        assert_eq!(cloudflare.secrets.len(), 1);
        assert_eq!(cloudflare.secrets[0].env, "APP_TOKEN");

        let env = manifest.environment();
        assert_eq!(env.variables.len(), 1);
    }

    #[test]
    fn manifest_from_path_sets_root_for_absolute_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("edgezero.toml");
        fs::write(&path, "").unwrap();

        let loader = ManifestLoader::from_path(&path).expect("manifest");
        assert_eq!(loader.manifest().root(), Some(dir.path()));
    }

    #[test]
    fn manifest_from_path_handles_relative_parent() {
        let cwd = env::current_dir().unwrap();
        let dir = tempdir_in(&cwd).unwrap();
        let path = dir.path().join("edgezero.toml");
        fs::write(&path, "").unwrap();

        let relative = path.strip_prefix(&cwd).unwrap().to_path_buf();
        let loader = ManifestLoader::from_path(&relative).expect("manifest");
        let expected = cwd.join(relative.parent().unwrap());
        assert_eq!(loader.manifest().root(), Some(expected.as_path()));
    }

    #[test]
    fn manifest_from_path_uses_cwd_for_empty_parent() {
        let cwd = env::current_dir().unwrap();
        let file = NamedTempFile::new_in(&cwd).unwrap();
        fs::write(file.path(), "").unwrap();
        let file_name = file.path().file_name().unwrap();
        let path = PathBuf::from(file_name);

        let loader = ManifestLoader::from_path(&path).expect("manifest");
        assert_eq!(loader.manifest().root(), Some(cwd.as_path()));
    }

    #[test]
    fn manifest_from_path_uses_cwd_when_parent_is_none() {
        let cwd = env::current_dir().unwrap();
        let file_name = format!("edgezero-test-manifest-{}.toml", process::id());
        let path = cwd.join(&file_name);
        fs::write(&path, "").unwrap();

        let loader = ManifestLoader::from_path(&PathBuf::from(&file_name)).expect("manifest");
        assert_eq!(loader.manifest().root(), Some(cwd.as_path()));

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn manifest_from_path_reports_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.toml");

        let err = ManifestLoader::from_path(&path)
            .err()
            .expect("missing manifest");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn manifest_from_path_reports_invalid_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("edgezero.toml");
        fs::write(&path, "[[triggers.http]]\npath = \"\"").unwrap();

        let err = ManifestLoader::from_path(&path)
            .err()
            .expect("invalid manifest");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn resolve_root_path_uses_cwd_when_parent_is_none() {
        let dir = tempdir().unwrap();
        let cwd = dir.path();
        let root = resolve_root_path(Path::new(""), cwd);
        assert_eq!(root, cwd);
    }

    #[test]
    fn manifest_from_path_reports_invalid_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("edgezero.toml");
        fs::write(&path, "not = [").unwrap();

        let err = ManifestLoader::from_path(&path)
            .err()
            .expect("invalid manifest");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn log_level_converts_to_level_filter() {
        let cases = [
            (LogLevel::Trace, LevelFilter::Trace),
            (LogLevel::Debug, LevelFilter::Debug),
            (LogLevel::Info, LevelFilter::Info),
            (LogLevel::Warn, LevelFilter::Warn),
            (LogLevel::Error, LevelFilter::Error),
            (LogLevel::Off, LevelFilter::Off),
        ];

        for (level, expected) in cases {
            assert_eq!(log::LevelFilter::from(level), expected);
        }
    }

    // HttpMethod parsing tests
    #[test]
    fn http_method_parses_all_variants() {
        let manifest = r#"
[[triggers.http]]
path = "/get"
methods = ["GET"]

[[triggers.http]]
path = "/post"
methods = ["POST"]

[[triggers.http]]
path = "/put"
methods = ["PUT"]

[[triggers.http]]
path = "/delete"
methods = ["DELETE"]

[[triggers.http]]
path = "/patch"
methods = ["PATCH"]

[[triggers.http]]
path = "/options"
methods = ["OPTIONS"]

[[triggers.http]]
path = "/head"
methods = ["HEAD"]
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(mfest.triggers.http.len(), 7);
        assert_eq!(mfest.triggers.http[0].methods(), vec!["GET"]);
        assert_eq!(mfest.triggers.http[1].methods(), vec!["POST"]);
        assert_eq!(mfest.triggers.http[2].methods(), vec!["PUT"]);
        assert_eq!(mfest.triggers.http[3].methods(), vec!["DELETE"]);
        assert_eq!(mfest.triggers.http[4].methods(), vec!["PATCH"]);
        assert_eq!(mfest.triggers.http[5].methods(), vec!["OPTIONS"]);
        assert_eq!(mfest.triggers.http[6].methods(), vec!["HEAD"]);
    }

    #[test]
    fn http_method_rejects_invalid_value() {
        let err = toml::from_str::<ManifestHttpTrigger>("path = \"/\"\nmethods = [\"FETCH\"]")
            .expect_err("invalid method");
        assert!(err.to_string().contains("unsupported HTTP method"));
    }

    #[test]
    fn http_method_rejects_non_string_value() {
        let err = toml::from_str::<ManifestHttpTrigger>("path = \"/\"\nmethods = [1]")
            .expect_err("invalid method");
        assert!(err.to_string().contains("invalid type"));
    }

    #[test]
    fn http_method_is_case_insensitive() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
methods = ["get", "Post", "PUT"]
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(mfest.triggers.http[0].methods(), vec!["GET", "POST", "PUT"]);
    }

    #[test]
    fn http_trigger_defaults_to_get() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(mfest.triggers.http[0].methods(), vec!["GET"]);
    }

    // BodyMode parsing tests
    #[test]
    fn body_mode_parses_buffered() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
body-mode = "buffered"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(mfest.triggers.http[0].body_mode, Some(BodyMode::Buffered));
    }

    #[test]
    fn body_mode_parses_stream() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
body-mode = "stream"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(mfest.triggers.http[0].body_mode, Some(BodyMode::Stream));
    }

    #[test]
    fn body_mode_rejects_invalid_value() {
        let err = toml::from_str::<ManifestHttpTrigger>("path = \"/\"\nbody-mode = \"chunked\"")
            .expect_err("invalid body mode");
        assert!(err.to_string().contains("unsupported body mode"));
    }

    #[test]
    fn body_mode_rejects_non_string_value() {
        let err = toml::from_str::<ManifestHttpTrigger>("path = \"/\"\nbody-mode = 1")
            .expect_err("invalid body mode");
        assert!(err.to_string().contains("invalid type"));
    }

    // LogLevel parsing tests
    #[test]
    fn log_level_parses_all_variants() {
        let manifest = r#"
[logging.adapter1]
level = "trace"

[logging.adapter2]
level = "debug"

[logging.adapter3]
level = "info"

[logging.adapter4]
level = "warn"

[logging.adapter5]
level = "error"

[logging.adapter6]
level = "off"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert_eq!(
            mfest.logging_for("adapter1").unwrap().level,
            LogLevel::Trace
        );
        assert_eq!(
            mfest.logging_for("adapter2").unwrap().level,
            LogLevel::Debug
        );
        assert_eq!(mfest.logging_for("adapter3").unwrap().level, LogLevel::Info);
        assert_eq!(mfest.logging_for("adapter4").unwrap().level, LogLevel::Warn);
        assert_eq!(
            mfest.logging_for("adapter5").unwrap().level,
            LogLevel::Error
        );
        assert_eq!(mfest.logging_for("adapter6").unwrap().level, LogLevel::Off);
    }

    #[test]
    fn log_level_rejects_invalid_value() {
        let err = toml::from_str::<ManifestLoggingConfig>("level = \"loud\"")
            .expect_err("invalid log level");
        assert!(err
            .to_string()
            .contains("logging level must be trace, debug, info, warn, error, or off"));
    }

    #[test]
    fn log_level_rejects_non_string_value() {
        let err =
            toml::from_str::<ManifestLoggingConfig>("level = 123").expect_err("invalid log level");
        assert!(err.to_string().contains("invalid type"));
    }

    #[test]
    fn log_level_as_str() {
        assert_eq!(LogLevel::Trace.as_str(), "trace");
        assert_eq!(LogLevel::Debug.as_str(), "debug");
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Warn.as_str(), "warn");
        assert_eq!(LogLevel::Error.as_str(), "error");
        assert_eq!(LogLevel::Off.as_str(), "off");
    }

    #[test]
    fn log_level_default_is_info() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    // Logging configuration tests
    #[test]
    fn logging_or_default_returns_default_when_missing() {
        let manifest = r#"
[app]
name = "test"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let logging = mfest.logging_or_default("unknown");
        assert_eq!(logging.level, LogLevel::Info);
        assert!(logging.endpoint.is_none());
        assert!(logging.echo_stdout.is_none());
    }

    #[test]
    fn resolved_logging_config_applies_level() {
        let cfg = ManifestLoggingConfig {
            level: Some(LogLevel::Warn),
            ..Default::default()
        };
        let resolved = ResolvedLoggingConfig::from_manifest(&cfg);
        assert_eq!(resolved.level, LogLevel::Warn);
    }

    #[test]
    fn logging_config_with_endpoint_and_echo() {
        let manifest = r#"
[logging.axum]
level = "debug"
endpoint = "https://logs.example.com"
echo_stdout = true
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let logging = mfest.logging_for("axum").unwrap();
        assert_eq!(logging.level, LogLevel::Debug);
        assert_eq!(
            logging.endpoint.as_deref(),
            Some("https://logs.example.com")
        );
        assert_eq!(logging.echo_stdout, Some(true));
    }

    #[test]
    fn adapter_logging_config_overrides_global() {
        let manifest = r#"
[adapters.fastly.logging]
level = "error"
endpoint = "https://fastly-logs.example.com"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let logging = mfest.logging_for("fastly").unwrap();
        assert_eq!(logging.level, LogLevel::Error);
        assert_eq!(
            logging.endpoint.as_deref(),
            Some("https://fastly-logs.example.com")
        );
    }

    // Environment binding tests
    #[test]
    fn environment_binding_uses_env_key_when_specified() {
        let manifest = r#"
[[environment.variables]]
name = "MY_VAR"
env = "ACTUAL_ENV_KEY"
value = "some-value"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let env = mfest.environment_for("any-adapter");
        assert_eq!(env.variables[0].name, "MY_VAR");
        assert_eq!(env.variables[0].env, "ACTUAL_ENV_KEY");
        assert_eq!(env.variables[0].value.as_deref(), Some("some-value"));
    }

    #[test]
    fn environment_binding_defaults_env_to_name() {
        let manifest = r#"
[[environment.variables]]
name = "API_KEY"
value = "secret"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let env = mfest.environment_for("any-adapter");
        assert_eq!(env.variables[0].name, "API_KEY");
        assert_eq!(env.variables[0].env, "API_KEY");
    }

    #[test]
    fn environment_filters_by_adapter_case_insensitive() {
        let manifest = r#"
[[environment.variables]]
name = "VAR1"
value = "v1"
adapters = ["Fastly"]

[[environment.variables]]
name = "VAR2"
value = "v2"
adapters = ["cloudflare"]

[[environment.variables]]
name = "VAR3"
value = "v3"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();

        let fastly_env = mfest.environment_for("FASTLY");
        assert_eq!(fastly_env.variables.len(), 2); // VAR1 and VAR3
        assert!(fastly_env.variables.iter().any(|var| var.name == "VAR1"));
        assert!(fastly_env.variables.iter().any(|var| var.name == "VAR3"));

        let cf_env = mfest.environment_for("Cloudflare");
        assert_eq!(cf_env.variables.len(), 2); // VAR2 and VAR3
        assert!(cf_env.variables.iter().any(|var| var.name == "VAR2"));
        assert!(cf_env.variables.iter().any(|var| var.name == "VAR3"));
    }

    #[test]
    fn environment_binding_with_description() {
        let manifest = r#"
[[environment.secrets]]
name = "DB_PASSWORD"
description = "Database password for production"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let env = mfest.environment_for("any");
        assert_eq!(
            env.secrets[0].description.as_deref(),
            Some("Database password for production")
        );
    }

    // Adapter configuration tests
    #[test]
    fn adapter_build_config() {
        let manifest = r#"
[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"
features = ["feature1", "feature2"]
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let adapter = &mfest.adapters["fastly"];
        assert_eq!(adapter.build.target.as_deref(), Some("wasm32-wasip1"));
        assert_eq!(adapter.build.profile.as_deref(), Some("release"));
        assert_eq!(adapter.build.features, vec!["feature1", "feature2"]);
    }

    #[test]
    fn adapter_commands_config() {
        let manifest = r#"
[adapters.fastly.commands]
build = "fastly compute build"
serve = "fastly compute serve"
deploy = "fastly compute deploy"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let adapter = &mfest.adapters["fastly"];
        assert_eq!(
            adapter.commands.build.as_deref(),
            Some("fastly compute build")
        );
        assert_eq!(
            adapter.commands.serve.as_deref(),
            Some("fastly compute serve")
        );
        assert_eq!(
            adapter.commands.deploy.as_deref(),
            Some("fastly compute deploy")
        );
    }

    #[test]
    fn adapter_definition_config() {
        let manifest = r#"
[adapters.fastly.adapter]
crate = "crates/fastly-adapter"
manifest = "fastly.toml"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        let adapter = &mfest.adapters["fastly"];
        assert_eq!(
            adapter.adapter.crate_path.as_deref(),
            Some("crates/fastly-adapter")
        );
        assert_eq!(adapter.adapter.manifest.as_deref(), Some("fastly.toml"));
    }

    // Empty/minimal manifest tests
    #[test]
    fn empty_manifest_has_defaults() {
        let manifest = "";
        let loader = ManifestLoader::load_from_str(manifest);
        let mfest = loader.manifest();
        assert!(mfest.app.name.is_none());
        assert!(mfest.app.entry.is_none());
        assert!(mfest.triggers.http.is_empty());
        assert!(mfest.adapters.is_empty());
    }

    #[test]
    fn manifest_root_is_none_when_loaded_from_str() {
        let loader = ManifestLoader::load_from_str(SAMPLE);
        assert!(loader.manifest().root().is_none());
    }

    // HttpMethod as_str tests
    #[test]
    fn http_method_as_str_returns_uppercase() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Post.as_str(), "POST");
        assert_eq!(HttpMethod::Put.as_str(), "PUT");
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
        assert_eq!(HttpMethod::Patch.as_str(), "PATCH");
        assert_eq!(HttpMethod::Options.as_str(), "OPTIONS");
        assert_eq!(HttpMethod::Head.as_str(), "HEAD");
    }

    // -- Portable store declarations ---------------------------------------

    #[test]
    fn store_declaration_round_trips() {
        let toml = r#"
[stores.kv]
ids = ["sessions", "cache"]
default = "sessions"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        let loader = ManifestLoader::load_from_str(toml);
        let stores = &loader.manifest().stores;

        let kv = stores.kv.as_ref().expect("kv declared");
        assert_eq!(kv.ids, ["sessions", "cache"]);
        assert_eq!(kv.default_id(), "sessions");

        let config = stores.config.as_ref().expect("config declared");
        assert_eq!(config.ids, ["app_config"]);
        assert_eq!(config.default_id(), "app_config");
        assert_eq!(config.config_store_name("fastly"), "app_config");

        let secrets = stores.secrets.as_ref().expect("secrets declared");
        assert_eq!(secrets.default_id(), "default");
    }

    #[test]
    fn store_declaration_default_id_falls_back_to_first_id() {
        let loader = ManifestLoader::load_from_str("[stores.kv]\nids = [\"only\"]\n");
        let kv = loader.manifest().stores.kv.as_ref().expect("kv declared");
        assert!(kv.default.is_none());
        assert_eq!(kv.default_id(), "only");
    }

    #[test]
    fn store_declaration_empty_ids_fails_validation() {
        let manifest: Manifest = toml::from_str("[stores.kv]\nids = []\n").expect("should parse");
        assert!(
            manifest.validate().is_err(),
            "empty `ids` list should fail validation"
        );
    }

    #[test]
    fn store_declaration_requires_default_with_multiple_ids() {
        let manifest: Manifest =
            toml::from_str("[stores.kv]\nids = [\"a\", \"b\"]\n").expect("should parse");
        let err = manifest
            .validate()
            .expect_err("missing `default` with >1 id should fail validation");
        assert!(
            err.to_string().contains("default"),
            "error should mention `default`, got: {err}"
        );
    }

    #[test]
    fn store_declaration_default_must_be_a_declared_id() {
        let manifest: Manifest =
            toml::from_str("[stores.kv]\nids = [\"a\", \"b\"]\ndefault = \"c\"\n")
                .expect("should parse");
        let err = manifest
            .validate()
            .expect_err("`default` outside `ids` should fail validation");
        assert!(
            err.to_string().contains("declared `ids`"),
            "error should explain the `default` constraint, got: {err}"
        );
    }

    #[test]
    fn legacy_store_schema_is_a_hard_load_error() {
        for legacy in [
            "[stores.kv]\nname = \"MY_KV\"\n",
            "[stores.config]\nids = [\"app_config\"]\n\n[stores.config.defaults]\nkey = \"value\"\n",
            "[stores.kv]\nids = [\"sessions\"]\n\n[stores.kv.adapters.spin]\nname = \"label\"\n",
            "[stores.secrets]\nids = [\"default\"]\nenabled = false\n",
        ] {
            let err = ManifestLoader::try_load_from_str(legacy)
                .err()
                .unwrap_or_else(|| panic!("legacy manifest must fail to load: {legacy}"));
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(
                err.to_string()
                    .contains("docs/guide/manifest-store-migration.md"),
                "legacy-schema error must reference the migration guide, got: {err}"
            );
        }
    }

    #[test]
    fn legacy_adapter_subtables_are_a_hard_load_error() {
        // Pre-rewrite manifests carried per-adapter store / runtime tuning
        // under `[adapters.<name>.<sub>]`. The portable model moved all of
        // that to `EDGEZERO__*` env vars; stale subtables left in a
        // migrated manifest must surface as a hard load error rather than
        // be silently ignored.
        for legacy in [
            // legacy per-adapter KV-store override (old [stores.kv.adapters.spin] hoisted)
            "[adapters.spin.stores.kv.default]\nname = \"EDGEZERO_KV\"\n",
            "[adapters.fastly.stores.config]\nname = \"app_config\"\n",
            "[adapters.cloudflare.stores.secrets.default]\nname = \"WORKER_SECRETS\"\n",
            // legacy runtime-tuning subtable under [adapters.axum]
            "[adapters.axum.runtime]\nthreads = 4\n",
        ] {
            let err = ManifestLoader::try_load_from_str(legacy)
                .err()
                .unwrap_or_else(|| panic!("legacy adapter subtable must fail to load: {legacy}"));
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(
                err.to_string()
                    .contains("docs/guide/manifest-store-migration.md"),
                "legacy adapter-subtable error must reference the migration guide, got: {err}"
            );
        }
    }

    #[test]
    fn empty_manifest_has_no_config_store() {
        let mfest = ManifestLoader::load_from_str("");
        assert!(mfest.manifest().stores.config.is_none());
    }

    // Multiple triggers test
    #[test]
    fn triggers_with_all_fields() {
        let manifest = r#"
[[triggers.http]]
id = "route-1"
path = "/api/users"
methods = ["GET", "POST"]
handler = "handlers::users"
adapters = ["axum", "fastly"]
description = "User management endpoint"
body-mode = "buffered"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let trigger = &loader.manifest().triggers.http[0];
        assert_eq!(trigger.id.as_deref(), Some("route-1"));
        assert_eq!(trigger.path, "/api/users");
        assert_eq!(trigger.methods(), vec!["GET", "POST"]);
        assert_eq!(trigger.handler.as_deref(), Some("handlers::users"));
        assert_eq!(trigger.adapters, vec!["axum", "fastly"]);
        assert_eq!(
            trigger.description.as_deref(),
            Some("User management endpoint")
        );
        assert_eq!(trigger.body_mode, Some(BodyMode::Buffered));
    }

    // -- KV store config ---------------------------------------------------

    #[test]
    fn kv_store_name_defaults_when_omitted() {
        let loader = ManifestLoader::load_from_str("[app]\nname = \"test\"\n");
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("fastly"), "EDGEZERO_KV");
        assert_eq!(manifest.kv_store_name("cloudflare"), "EDGEZERO_KV");
    }

    #[test]
    fn kv_store_name_resolves_to_default_id() {
        let loader = ManifestLoader::load_from_str(
            "[stores.kv]\nids = [\"sessions\", \"cache\"]\ndefault = \"cache\"\n",
        );
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("fastly"), "cache");
        assert_eq!(manifest.kv_store_name("cloudflare"), "cache");
    }

    // -- Secret store config -----------------------------------------------

    #[test]
    fn secret_store_binding_defaults_to_constant_when_absent() {
        let manifest = ManifestLoader::load_from_str("[app]\nname = \"x\"\n");
        assert_eq!(
            manifest.manifest().secret_store_binding("fastly"),
            DEFAULT_SECRET_STORE_NAME
        );
    }

    #[test]
    fn secret_store_binding_resolves_to_default_id() {
        let manifest = ManifestLoader::load_from_str("[stores.secrets]\nids = [\"MY_SECRETS\"]\n");
        assert_eq!(
            manifest.manifest().secret_store_binding("fastly"),
            "MY_SECRETS"
        );
        assert_eq!(
            manifest.manifest().secret_store_binding("cloudflare"),
            "MY_SECRETS"
        );
    }

    #[test]
    fn secret_store_enabled_is_false_when_absent() {
        let manifest = ManifestLoader::load_from_str("[app]\nname = \"x\"\n");
        assert!(!manifest.manifest().secret_store_enabled("fastly"));
        assert!(!manifest.manifest().secret_store_enabled("cloudflare"));
    }

    #[test]
    fn secret_store_enabled_is_true_when_declared() {
        let manifest = ManifestLoader::load_from_str("[stores.secrets]\nids = [\"default\"]\n");
        assert!(manifest.manifest().stores.secrets.is_some());
        assert!(manifest.manifest().secret_store_enabled("fastly"));
        assert!(manifest.manifest().secret_store_enabled("cloudflare"));
    }

    // -- Adapter host/port config ------------------------------------------

    #[test]
    fn adapter_definition_with_host_and_port() {
        let manifest = r#"
[adapters.axum.adapter]
crate = "crates/axum-adapter"
host = "0.0.0.0"
port = 3000
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let manifest_data = loader.manifest();
        let adapter = &manifest_data.adapters["axum"];
        assert_eq!(adapter.adapter.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(adapter.adapter.port, Some(3000));
    }

    #[test]
    fn adapter_definition_host_and_port_default_to_none() {
        let manifest = r#"
[adapters.axum.adapter]
crate = "crates/axum-adapter"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let manifest_data = loader.manifest();
        let adapter = &manifest_data.adapters["axum"];
        assert!(adapter.adapter.host.is_none());
        assert!(adapter.adapter.port.is_none());
    }

    #[test]
    fn adapter_definition_accepts_spin_component_field() {
        // `component` is the Spin component id used by `provision`
        // and `config push` when `spin.toml` declares multiple
        // `[component.*]`. Documented in docs/guide/adapters/spin.md and
        // must round-trip through the manifest model now even though the
        // runtime ignores it.
        let manifest = r#"
[adapters.spin.adapter]
crate = "crates/my-app-adapter-spin"
manifest = "crates/my-app-adapter-spin/spin.toml"
component = "my-app"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let manifest_data = loader.manifest();
        let adapter = &manifest_data.adapters["spin"];
        assert_eq!(adapter.adapter.component.as_deref(), Some("my-app"));
    }

    #[test]
    fn adapter_definition_rejects_unknown_field_with_migration_pointer() {
        // Hard cutoff: the portable manifest enumerates the per-adapter
        // tuning surface explicitly. Anything else (e.g. a stale
        // pre-rewrite `runtime` knob, or a typo'd `compnent`) is a load
        // error rather than a silent drop.
        let manifest = r#"
[adapters.axum.adapter]
crate = "crates/axum-adapter"
runtime_threads = 4
"#;
        let err = ManifestLoader::try_load_from_str(manifest)
            .err()
            .expect("unknown adapter-definition field must fail to load");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(
            msg.contains("runtime_threads"),
            "error should name the offending field, got: {msg}"
        );
        assert!(
            msg.contains("docs/guide/manifest-store-migration.md"),
            "error should reference the migration guide, got: {msg}"
        );
    }
}
