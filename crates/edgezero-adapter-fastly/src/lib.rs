//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

#[cfg(feature = "cli")]
pub mod cli;
mod context;
#[cfg(feature = "fastly")]
pub mod key_value_store;
#[cfg(feature = "fastly")]
mod logger;
#[cfg(feature = "fastly")]
mod proxy;
#[cfg(feature = "fastly")]
mod request;
#[cfg(feature = "fastly")]
mod response;
#[cfg(feature = "fastly")]
pub mod secret_store;

pub use context::FastlyRequestContext;
#[cfg(feature = "fastly")]
pub use proxy::FastlyProxyClient;
#[cfg(feature = "fastly")]
pub use request::{
    dispatch, dispatch_with_kv, dispatch_with_kv_and_secrets, dispatch_with_secrets,
    into_core_request, DEFAULT_KV_STORE_NAME,
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

#[cfg(feature = "fastly")]
pub fn init_logger(
    endpoint: &str,
    level: log::LevelFilter,
    echo_stdout: bool,
) -> Result<(), log::SetLoggerError> {
    logger::init_logger(endpoint, level, echo_stdout)
}

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
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error>;
}

#[cfg(feature = "fastly")]
impl AppExt for edgezero_core::app::App {
    fn dispatch(&self, req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
        dispatch(self, req)
    }
}

#[cfg(feature = "fastly")]
pub fn run_app<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: fastly::Request,
) -> Result<fastly::Response, fastly::Error> {
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let manifest = manifest_loader.manifest();
    let logging = manifest.logging_or_default("fastly");
    let kv_name = manifest.kv_store_name("fastly").to_string();
    let kv_required = manifest.stores.kv.is_some();
    let secret_name = manifest.secret_store_name("fastly").to_string();
    let secrets_required = manifest.stores.secrets.is_some();
    run_app_with_logging::<A>(
        logging.into(),
        req,
        &kv_name,
        kv_required,
        &secret_name,
        secrets_required,
    )
}

#[cfg(feature = "fastly")]
pub(crate) fn run_app_with_logging<A: edgezero_core::app::Hooks>(
    logging: FastlyLogging,
    req: fastly::Request,
    kv_store_name: &str,
    kv_required: bool,
    secret_store_name: &str,
    secrets_required: bool,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout).expect("init fastly logger");
    }

    let app = A::build_app();
    dispatch_with_kv_and_secrets(
        &app,
        req,
        kv_store_name,
        kv_required,
        secret_store_name,
        secrets_required,
    )
}

#[cfg(all(test, feature = "fastly"))]
mod tests {
    use super::*;

    #[test]
    fn fastly_logging_from_manifest_converts_defaults() {
        let config = edgezero_core::manifest::ResolvedLoggingConfig {
            endpoint: Some("endpoint".to_string()),
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
