//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

#[cfg(feature = "cli")]
pub mod cli;
mod context;
#[cfg(feature = "fastly")]
mod logger;
#[cfg(feature = "fastly")]
mod proxy;
#[cfg(feature = "fastly")]
mod request;
#[cfg(feature = "fastly")]
mod response;

pub use context::FastlyRequestContext;
#[cfg(feature = "fastly")]
pub use proxy::FastlyProxyClient;
#[cfg(feature = "fastly")]
pub use request::{dispatch, into_core_request};
#[cfg(feature = "fastly")]
pub use response::from_core_response;

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
    let logging = manifest_loader.manifest().logging_or_default("fastly");
    run_app_with_logging::<A>(logging.into(), req)
}

#[cfg(feature = "fastly")]
pub fn run_app_with_logging<A: edgezero_core::app::Hooks>(
    logging: FastlyLogging,
    req: fastly::Request,
) -> Result<fastly::Response, fastly::Error> {
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout).expect("init fastly logger");
    }

    let app = A::build_app();
    dispatch(&app, req)
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
