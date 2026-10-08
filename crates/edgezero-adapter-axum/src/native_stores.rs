//! Native per-ID provider inputs. Deferred opening belongs inside the future body.

#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "typed inputs and per-kind insertion methods precede source validation and preparation"
)]

use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use edgezero_core::app::StoresMetadata;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::store_registry::{BoundSecretStore, ConfigStoreBinding};
#[cfg(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager"))]
use edgezero_core::time::MonotonicClock;
#[cfg(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager"))]
use edgezero_store_aws::AwsPreparation;
use edgezero_store_aws::settings::{
    AgentAccessToken, AppConfigAgentSettings, AwsPreparationLimits, SecretsManagerSettings,
};

use crate::config_store_limits::ConfigStoreLimits;
use crate::native_bindings::{
    NativeBindingSource, NativeConfigBinding, NativeProviderCapabilities, NativeSecretBinding,
    NativeSourceSelection, NativeStoreBindings, NativeSuppliedIds,
};
use crate::run_options::{AxumRunOptions, failure};

/// Closed preparation categories. Never wrap provider errors, settings or payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NativeStorePreparationError {
    #[error("invalid binding")]
    InvalidBinding,
    #[error("duplicate binding")]
    DuplicateBinding,
    #[error("provider unavailable")]
    Unavailable,
    #[error("preparation deadline")]
    Deadline,
    #[error("size limit")]
    SizeLimit,
    #[error("malformed value")]
    Malformed,
}

/// Owned native setup. No `Send` bound; provider handles retain their core bounds.
pub type NativeStoreFuture<H> =
    Pin<Box<dyn Future<Output = Result<H, NativeStorePreparationError>> + 'static>>;

pub(crate) enum NativeStoreSource<H> {
    Local,
    Ready(H),
    Prepare(NativeStoreFuture<H>),
    BuiltIn(NativeBuiltInSource),
}

pub(crate) enum NativeBuiltInSource {
    #[cfg_attr(
        not(feature = "aws-appconfig-agent"),
        expect(
            dead_code,
            reason = "typed settings remain SDK-free; payload is read only by the enabled provider"
        )
    )]
    AppConfigAgent(AppConfigAgentSettings, Option<AgentAccessToken>),
    #[cfg_attr(
        not(feature = "aws-secrets-manager"),
        expect(
            dead_code,
            reason = "typed settings remain SDK-free; payload is read only by the enabled provider"
        )
    )]
    SecretsManager(SecretsManagerSettings),
}

#[cfg(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager"))]
pub(crate) type NativeAwsSession = Option<AwsPreparation>;
#[cfg(not(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager")))]
pub(crate) struct NativeAwsSession;

/// Move-only inputs, separate from cloneable hosting options.
///
/// Deferred construction must be side-effect-free. The runner promises not to poll
/// setup until source validation succeeds; it cannot undo earlier caller-owned work.
#[derive(Default)]
pub struct NativeStoreOverrides {
    config: BTreeMap<String, NativeStoreSource<ConfigStoreBinding>>,
    kv: BTreeMap<String, NativeStoreSource<KvHandle>>,
    secrets: BTreeMap<String, NativeStoreSource<BoundSecretStore>>,
}

impl fmt::Debug for NativeStoreOverrides {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeStoreOverrides")
            .finish_non_exhaustive()
    }
}

impl NativeStoreOverrides {
    /// Supplies an existing config binding, including its own default key.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same config ID.
    #[inline]
    pub fn insert_config<Id: Into<String>>(
        &mut self,
        id: Id,
        binding: ConfigStoreBinding,
    ) -> Result<(), NativeStorePreparationError> {
        insert(
            &mut self.config,
            id.into(),
            NativeStoreSource::Ready(binding),
        )
    }

    /// Defers config opening until the runner has validated all sources.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same config ID.
    #[inline]
    pub fn prepare_config<Id, Setup>(
        &mut self,
        id: Id,
        future: Setup,
    ) -> Result<(), NativeStorePreparationError>
    where
        Id: Into<String>,
        Setup: Future<Output = Result<ConfigStoreBinding, NativeStorePreparationError>> + 'static,
    {
        insert(
            &mut self.config,
            id.into(),
            NativeStoreSource::Prepare(Box::pin(future)),
        )
    }

    /// Supplies an already-open KV handle.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same KV ID.
    #[inline]
    pub fn insert_kv<Id: Into<String>>(
        &mut self,
        id: Id,
        handle: KvHandle,
    ) -> Result<(), NativeStorePreparationError> {
        insert(&mut self.kv, id.into(), NativeStoreSource::Ready(handle))
    }

    /// Defers KV opening until the runner has validated all sources.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same KV ID.
    #[inline]
    pub fn prepare_kv<Id, Setup>(
        &mut self,
        id: Id,
        future: Setup,
    ) -> Result<(), NativeStorePreparationError>
    where
        Id: Into<String>,
        Setup: Future<Output = Result<KvHandle, NativeStorePreparationError>> + 'static,
    {
        insert(
            &mut self.kv,
            id.into(),
            NativeStoreSource::Prepare(Box::pin(future)),
        )
    }

    /// Supplies a secret binding, preserving its explicit namespace.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same secret ID.
    #[inline]
    pub fn insert_secrets<Id: Into<String>>(
        &mut self,
        id: Id,
        binding: BoundSecretStore,
    ) -> Result<(), NativeStorePreparationError> {
        insert(
            &mut self.secrets,
            id.into(),
            NativeStoreSource::Ready(binding),
        )
    }

    /// Defers secret opening until the runner has validated all sources.
    ///
    /// # Errors
    /// Rejects a second supplied input for the same secret ID.
    #[inline]
    pub fn prepare_secrets<Id, Setup>(
        &mut self,
        id: Id,
        future: Setup,
    ) -> Result<(), NativeStorePreparationError>
    where
        Id: Into<String>,
        Setup: Future<Output = Result<BoundSecretStore, NativeStorePreparationError>> + 'static,
    {
        insert(
            &mut self.secrets,
            id.into(),
            NativeStoreSource::Prepare(Box::pin(future)),
        )
    }
}

fn insert<H>(
    map: &mut BTreeMap<String, NativeStoreSource<H>>,
    id: String,
    source: NativeStoreSource<H>,
) -> Result<(), NativeStorePreparationError> {
    match map.entry(id) {
        Entry::Vacant(entry) => {
            entry.insert(source);
            Ok(())
        }
        Entry::Occupied(_) => Err(NativeStorePreparationError::DuplicateBinding),
    }
}

pub(crate) struct NativeSourcePlan {
    pub config_limits: ConfigStoreLimits,
    pub aws_limits: AwsPreparationLimits,
    pub config: BTreeMap<&'static str, NativeStoreSource<ConfigStoreBinding>>,
    pub kv: BTreeMap<&'static str, NativeStoreSource<KvHandle>>,
    pub secrets: BTreeMap<&'static str, NativeStoreSource<BoundSecretStore>>,
}

impl NativeSourcePlan {
    pub(crate) fn validate(
        metadata: StoresMetadata,
        options: &AxumRunOptions,
        overrides: NativeStoreOverrides,
    ) -> anyhow::Result<Self> {
        options.validate_store_overlays(metadata)?;
        let local_defaults = NativeStoreBindings::new(BTreeMap::new(), BTreeMap::new());
        let bindings = options.bindings().document().unwrap_or(&local_defaults);
        let supplied = NativeSuppliedIds {
            config: overrides.config.keys().cloned().collect(),
            kv: overrides.kv.keys().cloned().collect(),
            secrets: overrides.secrets.keys().cloned().collect(),
        };
        let selection = NativeSourceSelection::validate(
            metadata,
            bindings,
            &supplied,
            options.env_config(),
            NativeProviderCapabilities {
                appconfig_agent: cfg!(feature = "aws-appconfig-agent"),
                secrets_manager: cfg!(feature = "aws-secrets-manager"),
            },
            options.bindings_limits(),
            options.aws_limits(),
        )
        .map_err(|category| failure("sources", "bindings", &category.to_string()))?;
        for binding in bindings.config().values() {
            let NativeConfigBinding::AppConfigAgent(settings) = binding;
            if let Some(name) = &settings.access_token_env {
                options.agent_token(name)?;
            }
        }
        let plan = Self {
            config_limits: ConfigStoreLimits::from_env(options.env_config())?,
            aws_limits: bindings
                .validate(options.bindings_limits(), options.aws_limits())
                .map_err(|category| failure("sources", "bindings", &category.to_string()))?,
            config: resolve(selection.config, overrides.config, "config", |id| {
                let NativeConfigBinding::AppConfigAgent(settings) = bindings
                    .config()
                    .get(id)
                    .ok_or_else(|| failure("config", id, "invalid binding"))?;
                let token = settings
                    .access_token_env
                    .as_ref()
                    .map(|name| options.agent_token(name))
                    .transpose()?;
                Ok(NativeBuiltInSource::AppConfigAgent(settings.clone(), token))
            })?,
            kv: resolve(selection.kv, overrides.kv, "kv", |id| {
                Err(failure("kv", id, "invalid binding"))
            })?,
            secrets: resolve(selection.secrets, overrides.secrets, "secrets", |id| {
                let NativeSecretBinding::SecretsManager(settings) = bindings
                    .secrets()
                    .get(id)
                    .ok_or_else(|| failure("secrets", id, "invalid binding"))?;
                Ok(NativeBuiltInSource::SecretsManager(settings.clone()))
            })?,
        };
        options.validate_local_roots(
            plan.config
                .values()
                .any(|source| matches!(source, NativeStoreSource::Local)),
            plan.kv
                .values()
                .any(|source| matches!(source, NativeStoreSource::Local)),
        )?;
        Ok(plan)
    }

    #[cfg_attr(
        not(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager")),
        expect(
            clippy::unnecessary_wraps,
            reason = "the same fallible session boundary is used when native services are compiled"
        )
    )]
    pub(crate) fn session(&self) -> anyhow::Result<NativeAwsSession> {
        #[cfg(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager"))]
        {
            let selected = self
                .config
                .values()
                .any(|source| matches!(source, NativeStoreSource::BuiltIn(_)))
                || self
                    .secrets
                    .values()
                    .any(|source| matches!(source, NativeStoreSource::BuiltIn(_)));
            if selected {
                return AwsPreparation::new(MonotonicClock::default(), self.aws_limits)
                    .map(Some)
                    .map_err(|category| failure("sources", "bindings", &category.to_string()));
            }
            Ok(None)
        }
        #[cfg(not(any(feature = "aws-appconfig-agent", feature = "aws-secrets-manager")))]
        {
            let _: AwsPreparationLimits = self.aws_limits;
            Ok(NativeAwsSession)
        }
    }
}

fn resolve<H, BuiltIn: Fn(&str) -> anyhow::Result<NativeBuiltInSource>>(
    selection: BTreeMap<&'static str, NativeBindingSource>,
    mut supplied: BTreeMap<String, NativeStoreSource<H>>,
    kind: &str,
    builtin: BuiltIn,
) -> anyhow::Result<BTreeMap<&'static str, NativeStoreSource<H>>> {
    selection
        .into_iter()
        .map(|(id, selected)| {
            let source = match selected {
                NativeBindingSource::Local => NativeStoreSource::Local,
                NativeBindingSource::Supplied => supplied
                    .remove(id)
                    .ok_or_else(|| failure(kind, id, "invalid binding"))?,
                NativeBindingSource::BuiltIn => NativeStoreSource::BuiltIn(builtin(id)?),
            };
            Ok((id, source))
        })
        .collect()
}

pub(crate) async fn prepare<H, Open>(
    source: NativeStoreSource<H>,
    kind: &str,
    id: &str,
    local: Open,
) -> anyhow::Result<H>
where
    Open: FnOnce() -> anyhow::Result<H>,
{
    match source {
        NativeStoreSource::Local => local(),
        NativeStoreSource::Ready(handle) => Ok(handle),
        NativeStoreSource::Prepare(future) => future
            .await
            .map_err(|category| failure(kind, id, &category.to_string())),
        NativeStoreSource::BuiltIn(_) => Err(failure(kind, id, "provider not compiled")),
    }
}

#[cfg_attr(
    not(feature = "aws-appconfig-agent"),
    expect(
        clippy::unused_async,
        reason = "compiled-out provider retains the same preparation future boundary"
    )
)]
pub(crate) async fn prepare_aws_config(
    builtin: NativeBuiltInSource,
    session: &mut NativeAwsSession,
    id: &str,
) -> anyhow::Result<ConfigStoreBinding> {
    #[cfg(feature = "aws-appconfig-agent")]
    {
        match builtin {
            NativeBuiltInSource::AppConfigAgent(settings, token) => {
                let handle = session
                    .as_mut()
                    .ok_or_else(|| failure("config", id, "invalid preparation session"))?
                    .prepare_appconfig(&settings, token.as_ref())
                    .await
                    .map_err(|category| failure("config", id, &category.to_string()))?;
                Ok(ConfigStoreBinding {
                    default_key: settings.default_key,
                    handle,
                })
            }
            NativeBuiltInSource::SecretsManager(_) => Err(failure("config", id, "invalid binding")),
        }
    }
    #[cfg(not(feature = "aws-appconfig-agent"))]
    {
        let _: (NativeBuiltInSource, &mut NativeAwsSession) = (builtin, session);
        Err(failure("config", id, "provider not compiled"))
    }
}

#[cfg_attr(
    not(feature = "aws-secrets-manager"),
    expect(
        clippy::unused_async,
        reason = "compiled-out provider retains the same preparation future boundary"
    )
)]
pub(crate) async fn prepare_aws_secrets(
    builtin: NativeBuiltInSource,
    session: &mut NativeAwsSession,
    id: &str,
) -> anyhow::Result<BoundSecretStore> {
    #[cfg(feature = "aws-secrets-manager")]
    {
        match builtin {
            NativeBuiltInSource::SecretsManager(settings) => {
                let handle = session
                    .as_mut()
                    .ok_or_else(|| failure("secrets", id, "invalid preparation session"))?
                    .prepare_secrets(&settings, id)
                    .await
                    .map_err(|category| failure("secrets", id, &category.to_string()))?;
                Ok(BoundSecretStore::new(handle, id.to_owned()))
            }
            NativeBuiltInSource::AppConfigAgent(_, _) => {
                Err(failure("secrets", id, "invalid binding"))
            }
        }
    }
    #[cfg(not(feature = "aws-secrets-manager"))]
    {
        let _: (NativeBuiltInSource, &mut NativeAwsSession) = (builtin, session);
        Err(failure("secrets", id, "provider not compiled"))
    }
}
