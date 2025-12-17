use log::LevelFilter;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use validator::Validate;

pub struct ManifestLoader {
    manifest: Arc<Manifest>,
}

impl ManifestLoader {
    pub fn load_from_str(contents: &str) -> Self {
        let mut manifest: Manifest =
            toml::from_str(contents).expect("edgezero manifest should be valid");
        manifest
            .validate()
            .expect("edgezero manifest failed validation");
        manifest.finalize();
        Self {
            manifest: Arc::new(manifest),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, io::Error> {
        let contents = std::fs::read_to_string(path)?;
        let mut manifest: Manifest = toml::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let cwd = std::env::current_dir()?;
        let root_path = match path.parent() {
            Some(parent) if parent.as_os_str().is_empty() => cwd.clone(),
            Some(parent) if parent.is_relative() => cwd.join(parent),
            Some(parent) => parent.to_path_buf(),
            None => cwd,
        };
        manifest.root = Some(root_path);
        manifest
            .validate()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        manifest.finalize();
        Ok(Self {
            manifest: Arc::new(manifest),
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

#[derive(Debug, Deserialize, Validate)]
pub struct Manifest {
    #[serde(default)]
    #[validate(nested)]
    pub app: ManifestApp,
    #[serde(default)]
    #[validate(nested)]
    pub triggers: ManifestTriggers,
    #[serde(default)]
    #[validate(nested)]
    pub environment: ManifestEnvironment,
    #[serde(default)]
    #[validate(nested)]
    pub adapters: BTreeMap<String, ManifestAdapter>,
    #[serde(default)]
    #[validate(nested)]
    pub logging: ManifestLogging,
    #[serde(skip)]
    pub(crate) root: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) logging_resolved: BTreeMap<String, ResolvedLoggingConfig>,
}

impl Manifest {
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn logging_for(&self, adapter: &str) -> Option<&ResolvedLoggingConfig> {
        self.logging_resolved.get(adapter)
    }

    pub fn logging_or_default(&self, adapter: &str) -> ResolvedLoggingConfig {
        self.logging_for(adapter).cloned().unwrap_or_default()
    }

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

        ResolvedEnvironment { variables, secrets }
    }

    pub fn environment(&self) -> &ManifestEnvironment {
        &self.environment
    }

    fn finalize(&mut self) {
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
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestApp {
    #[serde(default)]
    #[validate(length(min = 1))]
    pub name: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub entry: Option<String>,
    #[serde(default)]
    pub middleware: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestTriggers {
    #[serde(default)]
    #[validate(nested)]
    pub http: Vec<ManifestHttpTrigger>,
}

#[derive(Clone, Debug, Deserialize, Validate)]
pub struct ManifestHttpTrigger {
    #[serde(default)]
    #[validate(length(min = 1))]
    pub id: Option<String>,
    #[validate(length(min = 1))]
    pub path: String,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub handler: Option<String>,
    #[serde(default)]
    pub methods: Vec<HttpMethod>,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub description: Option<String>,
    #[serde(rename = "body-mode")]
    #[serde(default)]
    pub body_mode: Option<BodyMode>,
}

impl ManifestHttpTrigger {
    pub fn methods(&self) -> Vec<&str> {
        if self.methods.is_empty() {
            vec!["GET"]
        } else {
            self.methods.iter().map(|m| m.as_str()).collect()
        }
    }
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestEnvironment {
    #[serde(default)]
    #[validate(nested)]
    pub variables: Vec<ManifestBinding>,
    #[serde(default)]
    #[validate(nested)]
    pub secrets: Vec<ManifestBinding>,
}

#[derive(Debug, Deserialize, Validate)]
pub struct ManifestBinding {
    #[validate(length(min = 1))]
    pub name: String,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub description: Option<String>,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub env: Option<String>,
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
    pub name: String,
    pub description: Option<String>,
    pub env: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedEnvironment {
    pub variables: Vec<ResolvedEnvironmentBinding>,
    pub secrets: Vec<ResolvedEnvironmentBinding>,
}

#[derive(Debug, Default, Deserialize, Validate)]
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
    #[serde(default)]
    #[validate(nested)]
    pub logging: ManifestLoggingConfig,
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestAdapterDefinition {
    #[serde(rename = "crate")]
    #[serde(default)]
    #[validate(length(min = 1))]
    pub crate_path: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub manifest: Option<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestAdapterBuild {
    #[serde(default)]
    #[validate(length(min = 1))]
    pub target: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub profile: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestAdapterCommands {
    #[serde(default)]
    #[validate(length(min = 1))]
    pub build: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub serve: Option<String>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub deploy: Option<String>,
}

#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestLogging {
    #[serde(flatten)]
    #[validate(nested)]
    pub adapters: BTreeMap<String, ManifestLoggingConfig>,
}

#[derive(Debug, Default, Deserialize, Clone, Validate)]
pub struct ManifestLoggingConfig {
    #[serde(default)]
    pub level: Option<LogLevel>,
    #[serde(default)]
    #[validate(length(min = 1))]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub echo_stdout: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ResolvedLoggingConfig {
    pub level: LogLevel,
    pub endpoint: Option<String>,
    pub echo_stdout: Option<bool>,
}

impl Default for ResolvedLoggingConfig {
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
        if let Some(endpoint) = &cfg.endpoint {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Options,
    Head,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Options => "OPTIONS",
            Self::Head => "HEAD",
        }
    }
}

impl<'de> Deserialize<'de> for HttpMethod {
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
            other => Err(serde::de::Error::custom(format!(
                "unsupported HTTP method `{}`",
                other
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub enum BodyMode {
    Buffered,
    Stream,
}

impl<'de> Deserialize<'de> for BodyMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "buffered" => Ok(Self::Buffered),
            "stream" => Ok(Self::Stream),
            other => Err(serde::de::Error::custom(format!(
                "unsupported body mode `{}`",
                other
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
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

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl<'de> Deserialize<'de> for LogLevel {
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
            other => Err(serde::de::Error::custom(format!(
                "logging level must be trace, debug, info, warn, error, or off (got `{}`)",
                other
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
