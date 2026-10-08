//! Native bootstrap schema and pure per-ID source decisions, without service clients.

#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "schema, bootstrap I/O and source validation are grouped"
)]

#[cfg(unix)]
use libc::O_NONBLOCK;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::iter::empty;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use edgezero_core::app::{StoreMetadata, StoresMetadata};
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{ManifestStores, StoreDeclaration};
use edgezero_core::secret_store::MAX_NAME_LEN;
use edgezero_store_aws::settings::{
    AppConfigAgentSettings, AwsPreparationLimits, SecretsManagerSettings,
};
use serde::Deserialize;

/// Closed bootstrap failures. File, TOML and provider inputs never become source chains.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NativeBindingsError {
    #[error("duplicate binding source")]
    Duplicate,
    #[error("invalid native bindings")]
    Invalid,
    #[error("invalid declared metadata")]
    InvalidMetadata,
    #[error("explicit binding conflicts with NAME/KEY overlay")]
    OverlayConflict,
    #[error("undeclared binding")]
    Undeclared,
    #[error("malformed native bindings")]
    Malformed,
    #[error("native bindings size limit")]
    SizeLimit,
    #[error("native bindings file unavailable")]
    Unavailable,
    #[error("provider not compiled")]
    UnsupportedProvider,
}

/// Trusted bootstrap bounds. The file cannot raise its own read/ID limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeBindingsLimits {
    pub max_file_bytes: usize,
    pub max_explicit_ids: usize,
}

impl Default for NativeBindingsLimits {
    #[inline]
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1_024,
            max_explicit_ids: 128,
        }
    }
}

impl NativeBindingsLimits {
    /// Checks positive counters and the cap+1 overflow probe.
    ///
    /// # Errors
    /// Rejects zero or unusable counter values.
    #[inline]
    pub fn validate(&self) -> Result<(), NativeBindingsError> {
        if self.max_file_bytes == 0
            || self.max_explicit_ids == 0
            || self.max_file_bytes.checked_add(1).is_none()
            || u64::try_from(self.max_file_bytes).is_err()
        {
            return Err(NativeBindingsError::Invalid);
        }
        Ok(())
    }
}

/// Providers present in a resolved target, independent of the running CLI's features.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeProviderCapabilities {
    pub appconfig_agent: bool,
    pub secrets_manager: bool,
}

/// Strict configuration dispatch. Provider-specific fields are validated by AWS settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "provider", deny_unknown_fields)]
pub enum NativeConfigBinding {
    #[serde(rename = "aws-appconfig-agent")]
    AppConfigAgent(AppConfigAgentSettings),
}

/// Strict secret dispatch. No arbitrary remote names or per-request discovery.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "provider", deny_unknown_fields)]
pub enum NativeSecretBinding {
    #[serde(rename = "aws-secrets-manager")]
    SecretsManager(SecretsManagerSettings),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsTable {
    aws: AwsPreparationLimits,
}

/// Version 1 native document. Logical IDs and lookup keys remain case-sensitive.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeStoreBindings {
    version: u32,
    #[serde(default)]
    config: BTreeMap<String, NativeConfigBinding>,
    #[serde(default)]
    secrets: BTreeMap<String, NativeSecretBinding>,
    limits: Option<LimitsTable>,
}

impl fmt::Debug for NativeStoreBindings {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeStoreBindings")
            .finish_non_exhaustive()
    }
}

impl NativeStoreBindings {
    /// Builds a typed document with no file or ambient environment access.
    #[inline]
    #[must_use]
    pub fn new(
        config: BTreeMap<String, NativeConfigBinding>,
        secrets: BTreeMap<String, NativeSecretBinding>,
    ) -> Self {
        Self {
            version: 1,
            config,
            secrets,
            limits: None,
        }
    }

    /// Parses strict TOML and validates resource shapes and trusted metadata bounds.
    ///
    /// # Errors
    /// Returns fixed categories, never TOML excerpts or raw settings.
    #[inline]
    pub fn parse(input: &str, limits: NativeBindingsLimits) -> Result<Self, NativeBindingsError> {
        Self::parse_with_policy(input, limits, None)
    }

    /// Parses using exactly one document or explicit remote policy source.
    ///
    /// # Errors
    /// Rejects competing policies and malformed/invalid/over-limit documents.
    #[inline]
    pub fn parse_with_policy(
        input: &str,
        limits: NativeBindingsLimits,
        aws: Option<AwsPreparationLimits>,
    ) -> Result<Self, NativeBindingsError> {
        limits.validate()?;
        if input.len() > limits.max_file_bytes {
            return Err(NativeBindingsError::SizeLimit);
        }
        let bindings: Self =
            toml::from_str(input).map_err(|_error| NativeBindingsError::Malformed)?;
        bindings.validate(limits, aws)?;
        Ok(bindings)
    }

    /// Reads one explicitly selected absolute regular file, capped before parsing.
    ///
    /// # Errors
    /// Missing, non-file, malformed and cap+1 inputs fail without echoing paths/contents.
    #[inline]
    pub fn read(path: &Path, limits: NativeBindingsLimits) -> Result<Self, NativeBindingsError> {
        Self::read_with_policy(path, limits, None)
    }

    /// Captures one capped file under an explicit remote limits policy if supplied.
    ///
    /// # Errors
    /// Rejects competing policies, unusable files and malformed/over-limit documents.
    #[inline]
    pub fn read_with_policy(
        path: &Path,
        limits: NativeBindingsLimits,
        aws: Option<AwsPreparationLimits>,
    ) -> Result<Self, NativeBindingsError> {
        limits.validate()?;
        if !path.is_absolute() {
            return Err(NativeBindingsError::Invalid);
        }
        let mut open = OpenOptions::new();
        open.read(true);
        // A named pipe must not block before metadata can reject it. The check
        // uses the opened handle, so replacing a path cannot bypass its type.
        #[cfg(unix)]
        open.custom_flags(O_NONBLOCK);
        let file = open
            .open(path)
            .map_err(|_error| NativeBindingsError::Unavailable)?;
        if !file
            .metadata()
            .map_err(|_error| NativeBindingsError::Unavailable)?
            .is_file()
        {
            return Err(NativeBindingsError::Unavailable);
        }
        let probe = limits
            .max_file_bytes
            .checked_add(1)
            .ok_or(NativeBindingsError::Invalid)?;
        let mut bytes = Vec::new();
        file.take(u64::try_from(probe).map_err(|_error| NativeBindingsError::Invalid)?)
            .read_to_end(&mut bytes)
            .map_err(|_error| NativeBindingsError::Unavailable)?;
        if bytes.len() > limits.max_file_bytes {
            return Err(NativeBindingsError::SizeLimit);
        }
        let input = String::from_utf8(bytes).map_err(|_error| NativeBindingsError::Malformed)?;
        Self::parse_with_policy(&input, limits, aws)
    }

    /// Checks the whole settings document before constructing providers.
    ///
    /// # Errors
    /// Rejects unsupported schema, mappings, limits and competing policy sources.
    #[inline]
    pub fn validate(
        &self,
        limits: NativeBindingsLimits,
        explicit_aws: Option<AwsPreparationLimits>,
    ) -> Result<AwsPreparationLimits, NativeBindingsError> {
        limits.validate()?;
        if self.version != 1 {
            return Err(NativeBindingsError::Invalid);
        }
        if self
            .config
            .len()
            .checked_add(self.secrets.len())
            .ok_or(NativeBindingsError::SizeLimit)?
            > limits.max_explicit_ids
        {
            return Err(NativeBindingsError::SizeLimit);
        }
        if self.limits.is_some() && explicit_aws.is_some() {
            return Err(NativeBindingsError::Duplicate);
        }
        let policy = explicit_aws
            .or_else(|| self.limits.as_ref().map(|table| table.aws))
            .unwrap_or_default();
        policy
            .validate()
            .map_err(|_category| NativeBindingsError::Invalid)?;
        let mut mappings = 0_u64;
        for binding in self.config.values() {
            let NativeConfigBinding::AppConfigAgent(settings) = binding;
            settings
                .validate()
                .map_err(|_category| NativeBindingsError::Invalid)?;
            mappings = mappings
                .checked_add(
                    u64::try_from(settings.documents.len())
                        .map_err(|_error| NativeBindingsError::SizeLimit)?,
                )
                .ok_or(NativeBindingsError::SizeLimit)?;
        }
        for binding in self.secrets.values() {
            let NativeSecretBinding::SecretsManager(settings) = binding;
            settings
                .validate()
                .map_err(|_category| NativeBindingsError::Invalid)?;
            mappings = mappings
                .checked_add(
                    u64::try_from(settings.entries.len())
                        .map_err(|_error| NativeBindingsError::SizeLimit)?,
                )
                .ok_or(NativeBindingsError::SizeLimit)?;
        }
        if mappings > policy.max_remote_values {
            return Err(NativeBindingsError::SizeLimit);
        }
        Ok(policy)
    }

    /// Explicit configuration mappings.
    #[inline]
    #[must_use]
    pub fn config(&self) -> &BTreeMap<String, NativeConfigBinding> {
        &self.config
    }

    /// Explicit secret mappings.
    #[inline]
    #[must_use]
    pub fn secrets(&self) -> &BTreeMap<String, NativeSecretBinding> {
        &self.secrets
    }

    /// Selects one typed remote limits policy; it does not merge policy sources.
    #[inline]
    #[must_use]
    pub fn with_aws_limits(mut self, limits: AwsPreparationLimits) -> Self {
        self.limits = Some(LimitsTable { aws: limits });
        self
    }
}

/// Cloneable bootstrap selection. File contents are captured once at selection.
#[derive(Clone, Default)]
pub enum NativeStoreBindingsInput {
    #[default]
    LocalDefaults,
    File {
        path: PathBuf,
        bindings: NativeStoreBindings,
    },
    Inline(NativeStoreBindings),
}

impl NativeStoreBindingsInput {
    /// Captured settings, without reopening the selected file.
    #[inline]
    #[must_use]
    pub fn document(&self) -> Option<&NativeStoreBindings> {
        match self {
            Self::LocalDefaults => None,
            Self::File { bindings, .. } | Self::Inline(bindings) => Some(bindings),
        }
    }
}

impl fmt::Debug for NativeStoreBindingsInput {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::LocalDefaults => "LocalDefaults",
            Self::File { .. } => "File([redacted])",
            Self::Inline(_) => "Inline([redacted])",
        })
    }
}

/// Pure source classification. Custom handles are not inspected or quota-accounted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeBindingSource {
    Local,
    Supplied,
    BuiltIn,
}

/// Supplied IDs only. This carries no handles/futures and is usable by offline CLI checks.
#[derive(Default)]
pub struct NativeSuppliedIds {
    pub config: BTreeSet<String>,
    pub kv: BTreeSet<String>,
    pub secrets: BTreeSet<String>,
}

/// Borrowed declarations for compiled metadata or a CLI-owned portable manifest.
pub struct NativeStoreDeclarations<'declarations> {
    pub config: Option<NativeStoreDeclaration<'declarations>>,
    pub kv: Option<NativeStoreDeclaration<'declarations>>,
    pub secrets: Option<NativeStoreDeclaration<'declarations>>,
}

/// Names remain borrowed from the caller; validation does not leak or intern IDs.
pub struct NativeStoreDeclaration<'declarations> {
    pub ids: Vec<&'declarations str>,
    pub default: &'declarations str,
}

impl From<StoreMetadata> for NativeStoreDeclaration<'static> {
    #[inline]
    fn from(metadata: StoreMetadata) -> Self {
        Self {
            ids: metadata.ids.to_vec(),
            default: metadata.default,
        }
    }
}

impl From<StoresMetadata> for NativeStoreDeclarations<'static> {
    #[inline]
    fn from(metadata: StoresMetadata) -> Self {
        Self {
            config: metadata.config.map(Into::into),
            kv: metadata.kv.map(Into::into),
            secrets: metadata.secrets.map(Into::into),
        }
    }
}

impl<'declarations> TryFrom<&'declarations StoreDeclaration>
    for NativeStoreDeclaration<'declarations>
{
    type Error = NativeBindingsError;

    #[inline]
    fn try_from(declaration: &'declarations StoreDeclaration) -> Result<Self, Self::Error> {
        if !declaration.legacy.is_empty()
            || (declaration.ids.len() > 1 && declaration.default.is_none())
        {
            return Err(NativeBindingsError::InvalidMetadata);
        }
        Ok(Self {
            ids: declaration.ids.iter().map(String::as_str).collect(),
            default: declaration.default_id(),
        })
    }
}

impl<'declarations> TryFrom<&'declarations ManifestStores>
    for NativeStoreDeclarations<'declarations>
{
    type Error = NativeBindingsError;

    #[inline]
    fn try_from(stores: &'declarations ManifestStores) -> Result<Self, Self::Error> {
        Ok(Self {
            config: stores
                .config
                .as_ref()
                .map(NativeStoreDeclaration::try_from)
                .transpose()?,
            kv: stores
                .kv
                .as_ref()
                .map(NativeStoreDeclaration::try_from)
                .transpose()?,
            secrets: stores
                .secrets
                .as_ref()
                .map(NativeStoreDeclaration::try_from)
                .transpose()?,
        })
    }
}

/// Complete per-ID decisions. Registries keep their original metadata/defaults.
pub struct NativeSourceSelection<'declarations> {
    pub config: BTreeMap<&'declarations str, NativeBindingSource>,
    pub kv: BTreeMap<&'declarations str, NativeBindingSource>,
    pub secrets: BTreeMap<&'declarations str, NativeBindingSource>,
}

impl<'declarations> NativeSourceSelection<'declarations> {
    /// Validates metadata, every source and overlay, then checks resolved capabilities.
    ///
    /// # Errors
    /// Duplicate/undeclared sources, scalar collisions and absent target providers fail closed.
    #[inline]
    pub fn validate<Declarations: Into<NativeStoreDeclarations<'declarations>>>(
        metadata: Declarations,
        bindings: &NativeStoreBindings,
        supplied: &NativeSuppliedIds,
        env: &EnvConfig,
        capabilities: NativeProviderCapabilities,
        limits: NativeBindingsLimits,
        aws: Option<AwsPreparationLimits>,
    ) -> Result<Self, NativeBindingsError> {
        bindings.validate(limits, aws)?;
        let declarations = metadata.into();
        let selection = Self {
            config: resolve(
                declarations.config,
                &supplied.config,
                bindings.config.keys(),
                env,
                "config",
            )?,
            kv: resolve(declarations.kv, &supplied.kv, empty(), env, "kv")?,
            secrets: resolve(
                declarations.secrets,
                &supplied.secrets,
                bindings.secrets.keys(),
                env,
                "secrets",
            )?,
        };
        if (!bindings.config.is_empty() && !capabilities.appconfig_agent)
            || (!bindings.secrets.is_empty() && !capabilities.secrets_manager)
        {
            return Err(NativeBindingsError::UnsupportedProvider);
        }
        Ok(selection)
    }
}

fn resolve<'declarations, 'settings, Ids: Iterator<Item = &'settings String>>(
    metadata: Option<NativeStoreDeclaration<'declarations>>,
    supplied: &BTreeSet<String>,
    bound: Ids,
    env: &EnvConfig,
    kind: &str,
) -> Result<BTreeMap<&'declarations str, NativeBindingSource>, NativeBindingsError> {
    let settings: BTreeSet<&str> = bound.map(String::as_str).collect();
    let Some(meta) = metadata else {
        return if supplied.is_empty() && settings.is_empty() {
            Ok(BTreeMap::new())
        } else {
            Err(NativeBindingsError::Undeclared)
        };
    };
    if meta.ids.is_empty()
        || !meta.ids.contains(&meta.default)
        || meta.ids.iter().enumerate().any(|(index, id)| {
            id.is_empty()
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || meta.ids.iter().take(index).any(|previous| previous == id)
        })
    {
        return Err(NativeBindingsError::InvalidMetadata);
    }
    if supplied.iter().any(|id| !meta.ids.contains(&id.as_str()))
        || settings.iter().any(|id| !meta.ids.contains(id))
    {
        return Err(NativeBindingsError::Undeclared);
    }
    let mut result = BTreeMap::new();
    for id in meta.ids {
        let source = match (supplied.contains(id), settings.contains(id)) {
            (true, true) => return Err(NativeBindingsError::Duplicate),
            (true, false) => NativeBindingSource::Supplied,
            (false, true) => NativeBindingSource::BuiltIn,
            (false, false) => NativeBindingSource::Local,
        };
        if source != NativeBindingSource::Local
            && ["name", "key"]
                .iter()
                .any(|field| env.store_setting(kind, id, field).is_some())
        {
            return Err(NativeBindingsError::OverlayConflict);
        }
        if source == NativeBindingSource::Local
            && ["name", "key"].iter().any(|field| {
                env.store_setting(kind, id, field).is_some_and(|value| {
                    value.trim().is_empty() || value.chars().any(char::is_control)
                })
            })
        {
            return Err(NativeBindingsError::Invalid);
        }
        if kind == "secrets" && source != NativeBindingSource::Supplied {
            let namespace = if source == NativeBindingSource::Local {
                env.store_setting(kind, id, "name").map_or(id, str::trim)
            } else {
                id
            };
            if namespace.len() > MAX_NAME_LEN {
                return Err(
                    if source == NativeBindingSource::Local
                        && env.store_setting(kind, id, "name").is_some()
                    {
                        NativeBindingsError::Invalid
                    } else {
                        NativeBindingsError::InvalidMetadata
                    },
                );
            }
        }
        result.insert(id, source);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::assertions_on_result_states,
        reason = "strict parser tests compare accepted/rejected input matrices"
    )]
    #![expect(
        clippy::panic_in_result_fn,
        reason = "tests use fallible setup and assertions"
    )]

    use std::collections::{BTreeMap, BTreeSet};
    use std::error::Error;
    use std::fmt::Write as _;
    use std::fs;
    use std::path::Path;

    use edgezero_core::app::{StoreMetadata, StoresMetadata};
    use edgezero_core::env_config::EnvConfig;
    use edgezero_core::manifest::ManifestStores;
    use edgezero_store_aws::settings::AwsPreparationLimits;

    use super::{
        NativeBindingSource, NativeBindingsError, NativeBindingsLimits, NativeProviderCapabilities,
        NativeSourceSelection, NativeStoreBindings, NativeStoreDeclarations, NativeSuppliedIds,
    };

    const CONFIG: &str = "version = 1\n[config.app]\nprovider = 'aws-appconfig-agent'\ndefault_key = 'settings'\nendpoint = 'http://127.0.0.1:2772'\n[config.app.documents.settings]\napplication = 'example-app'\nenvironment = 'production'\nprofile = 'envelope'\n";

    fn metadata() -> StoresMetadata {
        StoresMetadata {
            config: Some(StoreMetadata {
                ids: &["app", "local"],
                default: "app",
            }),
            kv: Some(StoreMetadata {
                ids: &["state"],
                default: "state",
            }),
            secrets: None,
        }
    }

    #[test]
    fn manifest_ids_remain_borrowed_and_invalid_declarations_fail() -> Result<(), Box<dyn Error>> {
        let owned: ManifestStores =
            toml::from_str("[config]\nids = ['app', 'local']\ndefault = 'app'\n")?;
        let bindings = NativeStoreBindings::parse(CONFIG, NativeBindingsLimits::default())?;
        let selection = NativeSourceSelection::validate(
            NativeStoreDeclarations::try_from(&owned)?,
            &bindings,
            &NativeSuppliedIds::default(),
            &EnvConfig::default(),
            NativeProviderCapabilities {
                appconfig_agent: true,
                secrets_manager: false,
            },
            NativeBindingsLimits::default(),
            None,
        )?;
        assert_eq!(
            selection.config.get("app"),
            Some(&NativeBindingSource::BuiltIn)
        );
        let stored_id = selection
            .config
            .get_key_value("app")
            .ok_or(NativeBindingsError::InvalidMetadata)?
            .0;
        let original_id = &owned
            .config
            .as_ref()
            .ok_or(NativeBindingsError::InvalidMetadata)?
            .ids[0];
        assert_eq!(stored_id.as_ptr(), original_id.as_ptr());
        for input in [
            "[config]\nids = ['app', 'local']\n",
            "[config]\nids = ['app']\nname = 'legacy'\n",
        ] {
            let invalid: ManifestStores = toml::from_str(input)?;
            assert!(matches!(
                NativeStoreDeclarations::try_from(&invalid),
                Err(NativeBindingsError::InvalidMetadata)
            ));
        }
        Ok(())
    }

    #[test]
    fn secret_namespace_limits_are_checked_before_preparation() {
        let metadata = StoresMetadata {
            config: None,
            kv: None,
            secrets: Some(StoreMetadata {
                ids: &["signing"],
                default: "signing",
            }),
        };
        let bindings = NativeStoreBindings::new(BTreeMap::new(), BTreeMap::new());
        for (bytes, accepted) in [(512, true), (513, false)] {
            let env = EnvConfig::from_vars([(
                "EDGEZERO__STORES__SECRETS__SIGNING__NAME",
                "x".repeat(bytes),
            )]);
            let result = NativeSourceSelection::validate(
                metadata,
                &bindings,
                &NativeSuppliedIds::default(),
                &env,
                NativeProviderCapabilities::default(),
                NativeBindingsLimits::default(),
                None,
            );
            assert_eq!(result.is_ok(), accepted);
        }
    }

    #[test]
    fn strict_tagged_schema_and_case_sensitive_mappings() {
        let limits = NativeBindingsLimits::default();
        assert!(NativeStoreBindings::parse(CONFIG, limits).is_ok());
        for input in [
            CONFIG.replace("version = 1", "version = 2"),
            CONFIG.replace("version = 1", "version = 1\nunknown = 'sentinel'"),
            CONFIG.replace("aws-appconfig-agent", "dynamodb"),
            CONFIG.replace("default_key = 'settings'", "default_key = 'Settings'"),
            CONFIG.replace("endpoint =", "unknown = 'sentinel'\nendpoint ="),
            CONFIG.replace("application =", "unknown = 'sentinel'\napplication ="),
            CONFIG.replace("[config.app.documents.settings]", "[config.app.documents]"),
            format!("{CONFIG}\n[kv.state]\nprovider = 'dynamodb'"),
            format!("{CONFIG}\n[unknown]\nvalue = 'sentinel'"),
            CONFIG.replace("version = 1", "version = 1\nversion = 1"),
            format!("{CONFIG}\n[config.app]\nprovider = 'aws-appconfig-agent'"),
            format!("{CONFIG}\n[config.app.documents.settings]\nprofile = 'sentinel'"),
        ] {
            let error = NativeStoreBindings::parse(&input, limits).err();
            assert!(error.is_some(), "accepted malformed bindings");
            assert!(!format!("{error:?}").contains("sentinel"));
        }
    }

    #[test]
    fn capped_file_read_and_trusted_id_limit() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sentinel.toml");
        let limits = NativeBindingsLimits {
            max_file_bytes: CONFIG.len(),
            max_explicit_ids: 1,
        };
        fs::write(&path, CONFIG)?;
        assert!(NativeStoreBindings::read(&path, limits).is_ok());
        fs::write(&path, format!("{CONFIG} "))?;
        assert_eq!(
            NativeStoreBindings::read(&path, limits).err(),
            Some(NativeBindingsError::SizeLimit)
        );
        fs::write(&path, [0xff])?;
        let malformed = NativeStoreBindings::read(&path, limits)
            .err()
            .ok_or(NativeBindingsError::Invalid)?;
        assert_eq!(malformed, NativeBindingsError::Malformed);
        assert!(!format!("{malformed:?}").contains("sentinel"));
        assert!(malformed.source().is_none());
        assert_eq!(
            NativeStoreBindings::read(directory.path(), limits).err(),
            Some(NativeBindingsError::Unavailable)
        );
        assert_eq!(
            NativeStoreBindings::read(Path::new("relative.toml"), limits).err(),
            Some(NativeBindingsError::Invalid)
        );
        let mut many = String::from("version = 1\n");
        for index in 0..=128_u32 {
            many.push_str(
                &CONFIG
                    .replace("version = 1\n", "")
                    .replace("config.app", &format!("config.store{index}")),
            );
            if index == 127 {
                assert!(NativeStoreBindings::parse(&many, NativeBindingsLimits::default()).is_ok());
            }
        }
        assert_eq!(
            NativeStoreBindings::parse(&many, NativeBindingsLimits::default()).err(),
            Some(NativeBindingsError::SizeLimit)
        );
        Ok(())
    }

    #[test]
    fn whole_source_validation_and_target_capabilities() -> Result<(), NativeBindingsError> {
        let limits = NativeBindingsLimits::default();
        let bindings = NativeStoreBindings::parse(CONFIG, limits)?;
        let available = NativeProviderCapabilities {
            appconfig_agent: true,
            secrets_manager: false,
        };
        let mut supplied = NativeSuppliedIds::default();
        let env = EnvConfig::default();
        let selection = NativeSourceSelection::validate(
            metadata(),
            &bindings,
            &supplied,
            &env,
            available,
            limits,
            None,
        )?;
        assert_eq!(
            selection.config.get("app"),
            Some(&NativeBindingSource::BuiltIn)
        );
        assert_eq!(
            selection.config.get("local"),
            Some(&NativeBindingSource::Local)
        );
        assert_eq!(selection.kv.get("state"), Some(&NativeBindingSource::Local));
        assert!(matches!(
            NativeSourceSelection::validate(
                metadata(),
                &bindings,
                &supplied,
                &env,
                NativeProviderCapabilities::default(),
                limits,
                None
            ),
            Err(NativeBindingsError::UnsupportedProvider)
        ));
        supplied.config = BTreeSet::from(["app".to_owned()]);
        assert!(matches!(
            NativeSourceSelection::validate(
                metadata(),
                &bindings,
                &supplied,
                &env,
                available,
                limits,
                None
            ),
            Err(NativeBindingsError::Duplicate)
        ));
        supplied.config = BTreeSet::from(["APP".to_owned()]);
        assert!(matches!(
            NativeSourceSelection::validate(
                metadata(),
                &bindings,
                &supplied,
                &env,
                available,
                limits,
                None
            ),
            Err(NativeBindingsError::Undeclared)
        ));
        supplied.config.clear();
        for field in ["NAME", "KEY"] {
            let overlay =
                EnvConfig::from_vars([(format!("EDGEZERO__STORES__CONFIG__APP__{field}"), "")]);
            assert!(matches!(
                NativeSourceSelection::validate(
                    metadata(),
                    &bindings,
                    &supplied,
                    &overlay,
                    available,
                    limits,
                    None
                ),
                Err(NativeBindingsError::OverlayConflict)
            ));
        }
        Ok(())
    }

    #[test]
    fn explicit_policy_can_raise_mapping_caps_before_file_parsing() -> Result<(), Box<dyn Error>> {
        let mut input = CONFIG.to_owned();
        for index in 0..1_024_u32 {
            writeln!(
                input,
                "\n[config.app.documents.alias{index}]\napplication = 'example-app'\nenvironment = 'production'\nprofile = 'envelope'"
            )?;
        }
        let limits = NativeBindingsLimits::default();
        assert_eq!(
            NativeStoreBindings::parse(&input, limits).err(),
            Some(NativeBindingsError::SizeLimit)
        );
        let aws = AwsPreparationLimits {
            max_remote_values: 2_048,
            ..AwsPreparationLimits::default()
        };
        let root = tempfile::tempdir()?;
        let path = root.path().join("bindings.toml");
        fs::write(&path, input)?;
        let bindings = NativeStoreBindings::read_with_policy(&path, limits, Some(aws))?;
        assert_eq!(
            bindings.validate(limits, Some(aws))?.max_remote_values,
            2_048
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_is_rejected_without_waiting_for_a_writer() -> Result<(), Box<dyn Error>> {
        use std::process::Command;
        let root = tempfile::tempdir()?;
        let path = root.path().join("bindings.fifo");
        assert!(Command::new("mkfifo").arg(&path).status()?.success());
        assert_eq!(
            NativeStoreBindings::read(&path, NativeBindingsLimits::default()).err(),
            Some(NativeBindingsError::Unavailable)
        );
        Ok(())
    }

    #[test]
    fn secret_schema_rejects_irrelevant_fields_and_preserves_selectors()
    -> Result<(), NativeBindingsError> {
        let input = "version = 1\n[secrets.signing]\nprovider = 'aws-secrets-manager'\nregion = 'us-east-1'\n[secrets.signing.entries.active]\nsecret_id = 'arn:aws:secretsmanager:us-east-1:111122223333:secret:SENTINEL-AbCdEf'\nversion_stage = 'AWSCURRENT'\n";
        let limits = NativeBindingsLimits::default();
        let document = NativeStoreBindings::parse(input, limits)?;
        assert_eq!(document.secrets().len(), 1);
        assert!(!format!("{document:?}").contains("SENTINEL"));
        for bad in [
            input.replace("region =", "endpoint = 'http://127.0.0.1'\nregion ="),
            input.replace("version_stage =", "unknown = 'SENTINEL'\nversion_stage ="),
            input.replace("us-east-1'\n[secrets", "us-west-2'\n[secrets"),
            input.replace(
                "version_stage =",
                "version_id = '12345678901234567890123456789012'\nversion_stage =",
            ),
            input.replace("secret_id =", "secret_id = 'partial-name'\nother ="),
        ] {
            let error = NativeStoreBindings::parse(&bad, limits)
                .err()
                .ok_or(NativeBindingsError::Invalid)?;
            assert!(!format!("{error:?}").contains("SENTINEL"));
            assert!(error.source().is_none());
        }
        Ok(())
    }

    #[test]
    fn exact_mapping_boundary_uses_one_file_or_explicit_policy() -> Result<(), NativeBindingsError>
    {
        let limits = NativeBindingsLimits::default();
        let aws = AwsPreparationLimits {
            max_remote_values: 1,
            ..AwsPreparationLimits::default()
        };
        let explicit = NativeStoreBindings::parse_with_policy(CONFIG, limits, Some(aws))?;
        assert_eq!(explicit.validate(limits, Some(aws))?, aws);
        let file = NativeStoreBindings::parse(
            &format!("{CONFIG}\n[limits.aws]\nmax_remote_values = 1"),
            limits,
        )?;
        assert_eq!(file.validate(limits, None)?, aws);
        let alias = "\n[config.app.documents.alias]\napplication = 'example-app'\nenvironment = 'production'\nprofile = 'envelope'\n";
        assert_eq!(
            NativeStoreBindings::parse_with_policy(&format!("{CONFIG}{alias}"), limits, Some(aws))
                .err(),
            Some(NativeBindingsError::SizeLimit)
        );
        assert_eq!(
            NativeStoreBindings::parse(
                &format!("{CONFIG}{alias}\n[limits.aws]\nmax_remote_values = 1"),
                limits
            )
            .err(),
            Some(NativeBindingsError::SizeLimit)
        );
        Ok(())
    }

    #[test]
    fn limit_policy_sources_do_not_merge_and_aliases_count_before_dedup()
    -> Result<(), NativeBindingsError> {
        let limits = NativeBindingsLimits::default();
        let with_policy = format!("{CONFIG}\n[limits.aws]\nmax_snapshot_bytes = 16777216");
        let bindings = NativeStoreBindings::parse(&with_policy, limits)?;
        assert_eq!(
            bindings.validate(limits, Some(AwsPreparationLimits::default())),
            Err(NativeBindingsError::Duplicate)
        );
        let aliases = format!(
            "{CONFIG}\n[config.app.documents.alias]\napplication = 'example-app'\nenvironment = 'production'\nprofile = 'envelope'\n[limits.aws]\nmax_remote_values = 1"
        );
        assert_eq!(
            NativeStoreBindings::parse(&aliases, limits).err(),
            Some(NativeBindingsError::SizeLimit)
        );
        Ok(())
    }
}
