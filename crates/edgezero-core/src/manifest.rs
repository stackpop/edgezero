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

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
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
    pub stores: ManifestStores,
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

    /// Returns the KV store name for a given adapter.
    ///
    /// Resolution order:
    /// 1. Per-adapter override (`[stores.kv.adapters.<adapter>]`)
    /// 2. Global name (`[stores.kv] name = "..."`)    
    /// 3. Default: `"EDGEZERO_KV"`
    pub fn kv_store_name(&self, adapter: &str) -> &str {
        const DEFAULT: &str = "EDGEZERO_KV";
        match &self.stores.kv {
            Some(kv) => {
                let adapter_lower = adapter.to_ascii_lowercase();
                if let Some(adapter_cfg) = kv
                    .adapters
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&adapter_lower))
                {
                    return &adapter_cfg.1.name;
                }
                &kv.name
            }
            None => DEFAULT,
        }
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

/// Default KV store name used when `[stores.kv]` is omitted.
const DEFAULT_KV_STORE_NAME: &str = "EDGEZERO_KV";

fn default_kv_name() -> String {
    DEFAULT_KV_STORE_NAME.to_string()
}

/// Configuration for external stores (e.g., KV, object storage).
///
/// ```toml
/// [stores.kv]
/// name = "MY_KV"        # global default
///
/// [stores.kv.adapters.cloudflare]
/// name = "CF_BINDING"   # per-adapter override
/// ```
#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestStores {
    /// KV store configuration. When absent, the default
    /// name `EDGEZERO_KV` is used.
    #[serde(default)]
    pub kv: Option<ManifestKvConfig>,
}

/// Global KV store configuration.
#[derive(Debug, Deserialize, Validate)]
pub struct ManifestKvConfig {
    /// Store / binding name (default: `"EDGEZERO_KV"`).
    #[serde(default = "default_kv_name")]
    #[validate(length(min = 1))]
    pub name: String,

    /// Per-adapter name overrides.
    #[serde(default)]
    pub adapters: BTreeMap<String, ManifestKvAdapterConfig>,
}

/// Per-adapter KV binding / store name override.
#[derive(Debug, Deserialize, Validate)]
pub struct ManifestKvAdapterConfig {
    #[validate(length(min = 1))]
    pub name: String,
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
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
    use std::fs;
    use std::path::PathBuf;
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
        let cwd = std::env::current_dir().unwrap();
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
        let cwd = std::env::current_dir().unwrap();
        let file = NamedTempFile::new_in(&cwd).unwrap();
        fs::write(file.path(), "").unwrap();
        let file_name = file.path().file_name().unwrap();
        let path = PathBuf::from(file_name);

        let loader = ManifestLoader::from_path(&path).expect("manifest");
        assert_eq!(loader.manifest().root(), Some(cwd.as_path()));
    }

    #[test]
    fn manifest_from_path_uses_cwd_when_parent_is_none() {
        let cwd = std::env::current_dir().unwrap();
        let file_name = format!("edgezero-test-manifest-{}.toml", std::process::id());
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
        let m = loader.manifest();
        assert_eq!(m.triggers.http.len(), 7);
        assert_eq!(m.triggers.http[0].methods(), vec!["GET"]);
        assert_eq!(m.triggers.http[1].methods(), vec!["POST"]);
        assert_eq!(m.triggers.http[2].methods(), vec!["PUT"]);
        assert_eq!(m.triggers.http[3].methods(), vec!["DELETE"]);
        assert_eq!(m.triggers.http[4].methods(), vec!["PATCH"]);
        assert_eq!(m.triggers.http[5].methods(), vec!["OPTIONS"]);
        assert_eq!(m.triggers.http[6].methods(), vec!["HEAD"]);
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
        let m = loader.manifest();
        assert_eq!(m.triggers.http[0].methods(), vec!["GET", "POST", "PUT"]);
    }

    #[test]
    fn http_trigger_defaults_to_get() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let m = loader.manifest();
        assert_eq!(m.triggers.http[0].methods(), vec!["GET"]);
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
        let m = loader.manifest();
        assert_eq!(m.triggers.http[0].body_mode, Some(BodyMode::Buffered));
    }

    #[test]
    fn body_mode_parses_stream() {
        let manifest = r#"
[[triggers.http]]
path = "/test"
body-mode = "stream"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let m = loader.manifest();
        assert_eq!(m.triggers.http[0].body_mode, Some(BodyMode::Stream));
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
        let m = loader.manifest();
        assert_eq!(m.logging_for("adapter1").unwrap().level, LogLevel::Trace);
        assert_eq!(m.logging_for("adapter2").unwrap().level, LogLevel::Debug);
        assert_eq!(m.logging_for("adapter3").unwrap().level, LogLevel::Info);
        assert_eq!(m.logging_for("adapter4").unwrap().level, LogLevel::Warn);
        assert_eq!(m.logging_for("adapter5").unwrap().level, LogLevel::Error);
        assert_eq!(m.logging_for("adapter6").unwrap().level, LogLevel::Off);
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
        let m = loader.manifest();
        let logging = m.logging_or_default("unknown");
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
        let m = loader.manifest();
        let logging = m.logging_for("axum").unwrap();
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
        let m = loader.manifest();
        let logging = m.logging_for("fastly").unwrap();
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
        let m = loader.manifest();
        let env = m.environment_for("any-adapter");
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
        let m = loader.manifest();
        let env = m.environment_for("any-adapter");
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
        let m = loader.manifest();

        let fastly_env = m.environment_for("FASTLY");
        assert_eq!(fastly_env.variables.len(), 2); // VAR1 and VAR3
        assert!(fastly_env.variables.iter().any(|v| v.name == "VAR1"));
        assert!(fastly_env.variables.iter().any(|v| v.name == "VAR3"));

        let cf_env = m.environment_for("Cloudflare");
        assert_eq!(cf_env.variables.len(), 2); // VAR2 and VAR3
        assert!(cf_env.variables.iter().any(|v| v.name == "VAR2"));
        assert!(cf_env.variables.iter().any(|v| v.name == "VAR3"));
    }

    #[test]
    fn environment_binding_with_description() {
        let manifest = r#"
[[environment.secrets]]
name = "DB_PASSWORD"
description = "Database password for production"
"#;
        let loader = ManifestLoader::load_from_str(manifest);
        let m = loader.manifest();
        let env = m.environment_for("any");
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
        let m = loader.manifest();
        let adapter = m.adapters.get("fastly").unwrap();
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
        let m = loader.manifest();
        let adapter = m.adapters.get("fastly").unwrap();
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
        let m = loader.manifest();
        let adapter = m.adapters.get("fastly").unwrap();
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
        let m = loader.manifest();
        assert!(m.app.name.is_none());
        assert!(m.app.entry.is_none());
        assert!(m.triggers.http.is_empty());
        assert!(m.adapters.is_empty());
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
        let toml_str = r#"
[app]
name = "test"
"#;
        let loader = ManifestLoader::load_from_str(toml_str);
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("fastly"), "EDGEZERO_KV");
        assert_eq!(manifest.kv_store_name("cloudflare"), "EDGEZERO_KV");
    }

    #[test]
    fn kv_store_name_uses_global_name() {
        let toml_str = r#"
[app]
name = "test"

[stores.kv]
name = "MY_KV"
"#;
        let loader = ManifestLoader::load_from_str(toml_str);
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("fastly"), "MY_KV");
        assert_eq!(manifest.kv_store_name("cloudflare"), "MY_KV");
    }

    #[test]
    fn kv_store_name_adapter_override() {
        let toml_str = r#"
[app]
name = "test"

[stores.kv]
name = "GLOBAL_KV"

[stores.kv.adapters.cloudflare]
name = "CF_BINDING"
"#;
        let loader = ManifestLoader::load_from_str(toml_str);
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("cloudflare"), "CF_BINDING");
        assert_eq!(manifest.kv_store_name("fastly"), "GLOBAL_KV");
    }

    #[test]
    fn kv_store_name_case_insensitive() {
        let toml_str = r#"
[app]
name = "test"

[stores.kv]
name = "DEFAULT"

[stores.kv.adapters.Fastly]
name = "FASTLY_STORE"
"#;
        let loader = ManifestLoader::load_from_str(toml_str);
        let manifest = loader.manifest();
        assert_eq!(manifest.kv_store_name("fastly"), "FASTLY_STORE");
        assert_eq!(manifest.kv_store_name("FASTLY"), "FASTLY_STORE");
    }
}
