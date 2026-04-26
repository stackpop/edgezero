//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

#[cfg(feature = "fastly")]
use edgezero_core::app::{Hooks, FASTLY_ADAPTER};
#[cfg(feature = "fastly")]
use edgezero_core::manifest::ManifestLoader;

#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "fastly")]
pub mod config_store;
mod context;
#[cfg(feature = "fastly")]
pub mod key_value_store;
#[cfg(feature = "fastly")]
pub mod logger;
#[cfg(feature = "fastly")]
mod proxy;
#[cfg(feature = "fastly")]
mod request;
#[cfg(feature = "fastly")]
mod response;
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(feature = "fastly")]
pub use config_store::FastlyConfigStore;
pub use context::FastlyRequestContext;
#[cfg(feature = "fastly")]
pub use proxy::FastlyProxyClient;
#[cfg(feature = "fastly")]
#[expect(
    deprecated,
    reason = "re-exporting deprecated entry points for back-compat"
)]
pub use request::{
    dispatch, dispatch_with_config, dispatch_with_config_handle, dispatch_with_kv,
    dispatch_with_kv_and_secrets, dispatch_with_secrets, into_core_request, DEFAULT_KV_STORE_NAME,
};
#[cfg(feature = "fastly")]
pub use response::from_core_response;
#[cfg(feature = "fastly")]
pub use secret_store::FastlySecretStore;

#[cfg(feature = "fastly")]
#[derive(Debug, Clone)]
pub struct FastlyLogging {
    pub endpoint: Option<String>,
    pub level: log::LevelFilter,
    pub echo_stdout: bool,
    pub use_fastly_logger: bool,
}

#[cfg(feature = "fastly")]
impl From<edgezero_core::manifest::ResolvedLoggingConfig> for FastlyLogging {
    fn from(config: edgezero_core::manifest::ResolvedLoggingConfig) -> Self {
        Self {
            endpoint: config.endpoint,
            level: config.level.into(),
            echo_stdout: config.echo_stdout.unwrap_or(true),
            use_fastly_logger: true,
        }
    }
}

/// # Errors
/// Returns [`logger::InitLoggerError::Build`] if the underlying logger
/// builder rejects its inputs (e.g. an empty endpoint), or
/// [`logger::InitLoggerError::SetLogger`] if a global logger is already
/// installed.
#[cfg(feature = "fastly")]
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
pub fn init_logger(
    _endpoint: &str,
    _level: log::LevelFilter,
    _echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    Ok(())
}

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
impl AppExt for edgezero_core::app::App {
    #[allow(
        deprecated,
        reason = "implementing the deprecated trait method requires calling it"
    )]
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
        crate::request::dispatch_raw(self, req)
    }
}

/// Entry point for a Fastly Compute application.
///
/// **Breaking change (pre-1.0):** `manifest_src` is now a required parameter.
///
/// # Errors
/// Returns an error if the manifest is invalid or any required store cannot be opened.
#[cfg(feature = "fastly")]
pub fn run_app<A: Hooks>(
    manifest_src: &str,
    req: fastly::Request,
) -> Result<fastly::Response, fastly::Error> {
    let manifest_loader = ManifestLoader::try_load_from_str(manifest_src)
        .map_err(|err| fastly::Error::msg(err.to_string()))?;
    let manifest = manifest_loader.manifest();
    let resolved_logging = manifest.logging_or_default(FASTLY_ADAPTER);
    // Two-path resolution: `A::config_store()` is set at compile time by the
    // `#[app]` macro and is the common case. The manifest fallback handles
    // callers that implement `Hooks` manually without the macro — in that case
    // `A::config_store()` returns `None` while `[stores.config]` in
    // `edgezero.toml` may still be present.
    let config_name = A::config_store()
        .map(|cfg| cfg.name_for_adapter(FASTLY_ADAPTER).to_owned())
        .or_else(|| {
            manifest
                .stores
                .config
                .as_ref()
                .map(|cfg| cfg.config_store_name(FASTLY_ADAPTER).to_owned())
        });
    let kv_name = manifest.kv_store_name(FASTLY_ADAPTER).to_owned();
    let requirements = StoreRequirements {
        kv_required: manifest.stores.kv.is_some(),
        secrets_required: manifest.secret_store_enabled("fastly"),
    };
    let logging: FastlyLogging = resolved_logging.into();
    run_app_with_stores::<A>(
        &logging,
        req,
        config_name.as_deref(),
        &kv_name,
        &requirements,
    )
}

/// Dispatch with a config store. Prefer this over `run_app_with_logging` for new code.
///
/// # Errors
/// Returns an error if logger setup fails or the underlying handler returns an error.
#[cfg(feature = "fastly")]
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
    crate::request::dispatch_with_store_names(
        &app,
        req,
        config_store_name,
        kv_store_name,
        requirements.kv_required,
        requirements.secrets_required,
    )
}

#[cfg(all(test, feature = "fastly"))]
mod tests {
    use super::*;

    #[test]
    fn fastly_logging_from_manifest_converts_defaults() {
        let config = edgezero_core::manifest::ResolvedLoggingConfig {
            endpoint: Some("endpoint".to_owned()),
            echo_stdout: Some(false),
            level: edgezero_core::manifest::LogLevel::Debug,
        };

        let logging: FastlyLogging = config.into();
        assert_eq!(logging.endpoint.as_deref(), Some("endpoint"));
        assert_eq!(logging.level, log::LevelFilter::Debug);
        assert!(!logging.echo_stdout);
        assert!(logging.use_fastly_logger);
    }
}
