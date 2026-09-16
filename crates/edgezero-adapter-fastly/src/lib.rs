//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

// Only compiled where it is actually used (the CLI push/GC path and the Fastly
// runtime resolver). Gating it keeps a `--no-default-features` build dead-code
// clean instead of dragging in helpers no feature references.
#[cfg(any(feature = "cli", feature = "fastly", test))]
pub(crate) mod chunked_config;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "fastly")]
pub mod config_store;
pub mod context;
#[cfg(feature = "fastly")]
pub mod key_value_store;
#[cfg(feature = "fastly")]
pub mod logger;
#[cfg(feature = "fastly")]
pub mod proxy;
#[cfg(feature = "fastly")]
pub mod request;
#[cfg(feature = "fastly")]
pub mod response;
#[cfg(any(feature = "cli", feature = "fastly", test))]
#[cfg_attr(
    all(feature = "cli", not(feature = "fastly"), not(test)),
    expect(
        dead_code,
        reason = "CLI descriptor consumers land in the managed Fastly deploy task"
    )
)]
pub(crate) mod runtime_descriptor;
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(any(feature = "fastly", test))]
use std::error::Error;

#[cfg(any(feature = "cli", feature = "fastly", test))]
use chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
#[cfg(feature = "fastly")]
use edgezero_core::app::Hooks;
#[cfg(feature = "fastly")]
use edgezero_core::app::StoresMetadata;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::env_config::EnvConfig;
#[cfg(feature = "fastly")]
use edgezero_core::http::Extensions;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::manifest::ResolvedLoggingConfig;
/// Name of the Fastly Config Store the runtime opens for `EDGEZERO__*`
/// overrides.
///
/// The fixed name is load-bearing: a staged deploy creates a per-service
/// staging twin and links it into the staged version under THIS name, which is
/// how the runtime resolves staged selectors without knowing the twin exists.
pub const RUNTIME_ENV_STORE_NAME: &str = "edgezero_runtime_env";

/// Errors produced while serializing, parsing, or validating a Fastly runtime
/// descriptor.
#[cfg(any(feature = "cli", feature = "fastly", test))]
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RuntimeDescriptorError {
    /// The descriptor exceeds Fastly's per-entry byte limit.
    #[error("runtime descriptor exceeds Fastly's {FASTLY_CONFIG_ENTRY_LIMIT}-byte entry limit")]
    EntryTooLarge,
    /// The descriptor is not valid format-1 JSON.
    #[error("runtime descriptor JSON is invalid")]
    InvalidJson,
    /// A descriptor entry contains an invalid value.
    #[error("runtime descriptor contains an invalid value for {key:?}")]
    InvalidValue { key: String },
    /// A descriptor entry is not allowed for this application.
    #[error("runtime descriptor contains unsupported entry {key:?}")]
    UnsupportedEntry { key: String },
    /// The descriptor format requires a newer `EdgeZero` runtime.
    #[error(
        "runtime descriptor format {format} is unsupported; upgrade EdgeZero to a version that supports it"
    )]
    UnsupportedFormat { format: u64 },
}

/// Errors produced while loading the current Fastly service-version runtime
/// descriptor.
#[cfg(any(feature = "fastly", test))]
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RuntimeEnvConfigError {
    /// The service-version descriptor is absent from the selector store.
    #[error("Fastly runtime descriptor {descriptor_key:?} is absent")]
    DescriptorAbsent { descriptor_key: String },
    /// The descriptor could not be parsed or did not pass runtime validation.
    #[error("Fastly runtime descriptor {descriptor_key:?} is invalid: {source}")]
    InvalidDescriptor {
        descriptor_key: String,
        #[source]
        source: RuntimeDescriptorError,
    },
    /// The selector store could not be opened or queried.
    #[error("Fastly runtime descriptor lookup failed for {descriptor_key:?}")]
    LookupFailure {
        descriptor_key: String,
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
    /// The linked selector Config Store is absent.
    #[error(
        "Fastly selector Config Store `edgezero_runtime_env` is absent for runtime descriptor {descriptor_key:?}"
    )]
    SelectorStoreAbsent { descriptor_key: String },
}

#[cfg(any(feature = "fastly", test))]
#[derive(Debug, Clone)]
pub struct FastlyLogging {
    pub echo_stdout: bool,
    pub endpoint: Option<String>,
    pub level: log::LevelFilter,
    pub use_fastly_logger: bool,
}

#[cfg(any(feature = "fastly", test))]
impl From<ResolvedLoggingConfig> for FastlyLogging {
    #[inline]
    fn from(config: ResolvedLoggingConfig) -> Self {
        Self {
            echo_stdout: config.echo_stdout.unwrap_or(true),
            endpoint: config.endpoint,
            level: config.level.into(),
            use_fastly_logger: true,
        }
    }
}

/// Resolve [`FastlyLogging`] from the `EDGEZERO__LOGGING__*` overlay.
///
/// Three rules live here rather than in the caller. An unset or unparseable
/// `EDGEZERO__LOGGING__LEVEL` falls back to [`log::LevelFilter::Info`], and
/// `use_fastly_logger` is DERIVED from `endpoint.is_some()` so a Viceroy run
/// with no endpoint is never handed the reserved `stdout` name. `echo_stdout`
/// is always `true` on this path: `EDGEZERO__LOGGING__ECHO_STDOUT` is resolved
/// into the [`EnvConfig`] for downstream readers but is not applied here.
#[cfg(any(feature = "fastly", test))]
impl From<&EnvConfig> for FastlyLogging {
    #[inline]
    fn from(env: &EnvConfig) -> Self {
        use std::str::FromStr as _;

        let level = env
            .logging_level()
            .and_then(|raw| log::LevelFilter::from_str(raw).ok())
            .unwrap_or(log::LevelFilter::Info);
        // Only attach Fastly's named-endpoint logger when `EDGEZERO__LOGGING__ENDPOINT`
        // is set. Production deployments set it to a real `[log_endpoints]` entry from
        // `fastly.toml`; local Viceroy runs leave it unset and avoid the
        // "endpoint not found, or is reserved" error that fires when the adapter
        // would otherwise fall back to a reserved name like `stdout`.
        let endpoint = env.logging_endpoint().map(str::to_owned);
        let use_fastly_logger = endpoint.is_some();
        Self {
            echo_stdout: true,
            endpoint,
            level,
            use_fastly_logger,
        }
    }
}

/// # Errors
/// Returns [`logger::InitLoggerError::Build`] if the underlying logger
/// builder rejects its inputs (e.g. an empty endpoint), or
/// [`logger::InitLoggerError::SetLogger`] if a global logger is already
/// installed.
#[cfg(feature = "fastly")]
#[inline]
pub fn init_logger(
    endpoint: &str,
    level: log::LevelFilter,
    echo_stdout: bool,
) -> Result<(), logger::InitLoggerError> {
    logger::init_logger(endpoint, level, echo_stdout)
}

/// # Errors
/// Never; this is a no-op stub on builds without the `fastly` feature.
#[cfg(not(feature = "fastly"))]
#[inline]
pub fn init_logger(
    _endpoint: &str,
    _level: log::LevelFilter,
    _echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Entry point for a Fastly Compute application.
///
/// Portable store config is baked into `A` by the `app!` macro; adapter-specific
/// values (platform store names, logging level) are read at runtime from
/// `EDGEZERO__*` environment variables. No `edgezero.toml` is required.
///
/// # Errors
/// Selector-store lookup failures, missing required runtime descriptors, and
/// invalid runtime descriptors propagate before the app is constructed. Logger
/// setup failures and unavailable required stores also return errors.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app<A: Hooks>(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    run_app_with_request_extensions::<A, _>(req, |_req, _extensions| {})
}

/// Like [`run_app`], but runs `extend` against a scratch
/// [`Extensions`] populated from the raw
/// `fastly::Request` (TLS JA4, H2 fingerprint, client IP, …) before the request
/// is converted; the scratch values are merged into the core request's
/// extensions and are visible to middleware and the `State`/extractor layer.
///
/// # Errors
/// Selector-store lookup failures, missing required runtime descriptors, and
/// invalid runtime descriptors propagate before the app is constructed. Logger
/// setup failures and unavailable required stores also return errors.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_request_extensions<A, F>(
    req: fastly::Request,
    extend: F,
) -> Result<fastly::Response, fastly::Error>
where
    A: Hooks,
    F: FnOnce(&fastly::Request, &mut Extensions),
{
    let stores = A::stores();
    let env = runtime_env_config(stores)?;
    let logging = FastlyLogging::from(&env);
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores, &env, extend)
}

/// Build an [`EnvConfig`] from the current service-version descriptor in the
/// `edgezero_runtime_env` Fastly Config Store.
///
/// Compute@Edge has no process env, so the `EDGEZERO__*` runtime overrides
/// come from one immutable descriptor selected using Fastly's current service
/// ID and version. The descriptor is parsed and validated before any values are
/// exposed through [`EnvConfig`].
///
/// [`run_app`] and [`run_app_with_request_extensions`] call this themselves.
/// [`run_app_with_config`] does NOT, and neither does a hand-built
/// [`FastlyService`](request::FastlyService). A custom entry point on either path
/// must call this explicitly.
///
/// The `stores` argument must name the app's logical store ids. A handwritten
/// [`Hooks`] impl inherits the empty [`StoresMetadata::default`] and must
/// override `stores()` or pass explicit metadata here.
///
/// A store-free app may use its baked-in defaults when the selector store or
/// descriptor is absent. Apps declaring Config, KV, or Secret stores fail
/// closed on missing data. Lookup failures and invalid descriptors always fail.
///
/// # Errors
/// Returns [`RuntimeEnvConfigError`] when the selector store cannot be opened or
/// queried, a required descriptor is absent, or the descriptor is invalid.
#[cfg(feature = "fastly")]
#[inline]
pub fn runtime_env_config(stores: StoresMetadata) -> Result<EnvConfig, RuntimeEnvConfigError> {
    use fastly::ConfigStore;
    use fastly::compute_runtime::{service_id, service_version};
    use fastly::config_store::OpenError;
    use runtime_descriptor::{
        RuntimeEnvLookupError, runtime_descriptor_key, runtime_env_config_from_lookup,
    };
    use std::io::Error as IoError;

    let current_service_id = service_id();
    let current_version = service_version();
    let selector_store = match ConfigStore::try_open(RUNTIME_ENV_STORE_NAME) {
        Ok(store) => Some(store),
        Err(OpenError::ConfigStoreDoesNotExist) => None,
        Err(source) => {
            return Err(RuntimeEnvConfigError::LookupFailure {
                descriptor_key: runtime_descriptor_key(current_service_id, current_version),
                source: Box::new(source),
            });
        }
    };
    let resolution = match selector_store {
        Some(opened_store) => {
            runtime_env_config_from_lookup(current_service_id, current_version, stores, |key| {
                opened_store
                    .try_get(key)
                    .map_err(RuntimeEnvLookupError::Lookup)
            })?
        }
        None => {
            runtime_env_config_from_lookup(current_service_id, current_version, stores, |_key| {
                Err::<Option<String>, _>(RuntimeEnvLookupError::<IoError>::SelectorStoreAbsent)
            })?
        }
    };
    let (env, fallback) = resolution.into_parts();
    if let Some(diagnostic) = fallback {
        log::warn!("{diagnostic}");
    }
    Ok(env)
}

/// Dispatch with a config store wired explicitly. This path does NOT apply the
/// [`EnvConfig`] overlay: the store name comes directly from
/// `config_store_name`, and its default key is always `"default"`, so staged or
/// overridden `__NAME` / `__KEY` selectors are ignored. Use
/// [`runtime_env_config`] with [`request::dispatch_with_registries`] for the
/// same selector resolution as [`run_app`]. KV is not auto-injected on this
/// path; chain `.with_kv(name)` on a [`request::FastlyService`] builder if you
/// need KV alongside the config store.
///
/// # Errors
/// Returns an error if logger setup fails or the underlying handler returns an error.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_config<A: Hooks>(
    logging: &FastlyLogging,
    req: fastly::Request,
    config_store_name: Option<&str>,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    let mut service = request::FastlyService::new(&app);
    if let Some(name) = config_store_name {
        service = service.with_config(name);
    }
    service.dispatch(req)
}

#[cfg(test)]
mod fastly_logging_tests {
    use super::*;
    use edgezero_core::manifest::LogLevel;

    #[test]
    fn fastly_logging_from_manifest_converts_defaults() {
        let config = ResolvedLoggingConfig {
            echo_stdout: Some(false),
            endpoint: Some("endpoint".to_owned()),
            level: LogLevel::Debug,
        };

        let logging: FastlyLogging = config.into();
        assert_eq!(logging.endpoint.as_deref(), Some("endpoint"));
        assert_eq!(logging.level, log::LevelFilter::Debug);
        assert!(!logging.echo_stdout);
        assert!(logging.use_fastly_logger);
    }

    #[test]
    fn fastly_logging_from_env_falls_back_without_an_endpoint() {
        let env = EnvConfig::from_vars([
            ("EDGEZERO__LOGGING__LEVEL", "not-a-level"),
            ("EDGEZERO__LOGGING__ECHO_STDOUT", "false"),
        ]);

        let logging = FastlyLogging::from(&env);

        assert_eq!(logging.level, log::LevelFilter::Info);
        assert_eq!(logging.endpoint, None);
        assert!(!logging.use_fastly_logger);
        assert!(logging.echo_stdout);
    }

    #[test]
    fn fastly_logging_from_env_enables_the_named_endpoint_logger() {
        let env = EnvConfig::from_vars([
            ("EDGEZERO__LOGGING__LEVEL", "debug"),
            ("EDGEZERO__LOGGING__ENDPOINT", "edgezero-logs"),
        ]);

        let logging = FastlyLogging::from(&env);

        assert_eq!(logging.level, log::LevelFilter::Debug);
        assert_eq!(logging.endpoint.as_deref(), Some("edgezero-logs"));
        assert!(logging.use_fastly_logger);
        assert!(logging.echo_stdout);
    }
}
