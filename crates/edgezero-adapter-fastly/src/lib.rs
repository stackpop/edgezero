//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

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
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(feature = "fastly")]
use edgezero_core::app::{App, Hooks};
#[cfg(feature = "fastly")]
use edgezero_core::env_config::EnvConfig;
#[cfg(feature = "fastly")]
use edgezero_core::manifest::ResolvedLoggingConfig;
#[cfg(feature = "fastly")]
use request::DEFAULT_KV_STORE_NAME;

#[cfg(feature = "fastly")]
pub trait AppExt {
    #[deprecated(
        note = "AppExt::dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_handle()"
    )]
    /// # Errors
    /// Returns an error if the underlying handler returns an error or the response cannot be converted into a Fastly response.
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error>;
}

#[cfg(feature = "fastly")]
impl AppExt for App {
    #[inline]
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
        request::dispatch_raw(self, req)
    }
}

#[cfg(feature = "fastly")]
#[derive(Debug, Clone)]
pub struct FastlyLogging {
    pub echo_stdout: bool,
    pub endpoint: Option<String>,
    pub level: log::LevelFilter,
    pub use_fastly_logger: bool,
}

#[cfg(feature = "fastly")]
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

/// Whether each optional store is required to be present at startup.
///
/// Using a named struct instead of positional `bool` arguments prevents
/// accidental parameter swaps between `kv_required` and `secrets_required`.
#[cfg(feature = "fastly")]
#[derive(Default)]
struct StoreRequirements {
    kv_required: bool,
    secrets_required: bool,
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

/// Resolve [`FastlyLogging`] from `EDGEZERO__LOGGING__LEVEL`, falling back to
/// the adapter default when the variable is unset or unparseable.
#[cfg(feature = "fastly")]
fn logging_from_env(env: &EnvConfig) -> FastlyLogging {
    use std::str::FromStr as _;

    let level = env
        .logging_level()
        .and_then(|raw| log::LevelFilter::from_str(raw).ok())
        .unwrap_or(log::LevelFilter::Info);
    FastlyLogging {
        echo_stdout: true,
        endpoint: None,
        level,
        use_fastly_logger: true,
    }
}

/// Entry point for a Fastly Compute application.
///
/// Portable store config is baked into `A` by the `app!` macro; adapter-specific
/// values (platform store names, logging level) are read at runtime from
/// `EDGEZERO__*` environment variables. No `edgezero.toml` is required.
///
/// # Errors
/// Returns an error if logger setup fails or any required store cannot be opened.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app<A: Hooks>(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    let env = EnvConfig::from_env();
    let stores = A::stores();
    let logging = logging_from_env(&env);
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores.config, stores.kv, stores.secrets, &env)
}

/// Dispatch with a config store. Prefer this over `run_app_with_logging` for new code.
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
    run_app_with_stores::<A>(
        logging,
        req,
        config_store_name,
        DEFAULT_KV_STORE_NAME,
        &StoreRequirements::default(),
    )
}

/// Compatibility wrapper for callers that do not use a config store.
///
/// # Errors
/// Returns an error if logger setup fails or the underlying handler returns an error.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_logging<A: Hooks>(
    logging: &FastlyLogging,
    req: fastly::Request,
) -> Result<fastly::Response, fastly::Error> {
    run_app_with_stores::<A>(
        logging,
        req,
        None,
        DEFAULT_KV_STORE_NAME,
        &StoreRequirements::default(),
    )
}

#[cfg(feature = "fastly")]
fn run_app_with_stores<A: Hooks>(
    logging: &FastlyLogging,
    req: fastly::Request,
    config_store_name: Option<&str>,
    kv_store_name: &str,
    requirements: &StoreRequirements,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }

    let app = A::build_app();
    request::dispatch_with_store_names(
        &app,
        req,
        config_store_name,
        kv_store_name,
        requirements.kv_required,
        requirements.secrets_required,
    )
}

#[cfg(test)]
#[cfg(feature = "fastly")]
mod tests {
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
}
