//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

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
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(feature = "fastly")]
use edgezero_core::app::{Hooks, StoresMetadata};
#[cfg(feature = "fastly")]
use edgezero_core::env_config::EnvConfig;
#[cfg(feature = "fastly")]
use edgezero_core::manifest::ResolvedLoggingConfig;
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
    // Only attach Fastly's named-endpoint logger when `EDGEZERO__LOGGING__ENDPOINT`
    // is set. Production deployments set it to a real `[log_endpoints]` entry from
    // `fastly.toml`; local Viceroy runs leave it unset and avoid the
    // "endpoint not found, or is reserved" error that fires when the adapter
    // would otherwise fall back to a reserved name like `stdout`.
    let endpoint = env.logging_endpoint().map(str::to_owned);
    let use_fastly_logger = endpoint.is_some();
    FastlyLogging {
        echo_stdout: true,
        endpoint,
        level,
        use_fastly_logger,
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
    let stores = A::stores();
    let env = env_config_from_runtime_dictionary(stores);
    let logging = logging_from_env(&env);
    if logging.use_fastly_logger {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores.config, stores.kv, stores.secrets, &env)
}

/// Build an [`EnvConfig`] from the optional `edgezero_runtime_env`
/// Fastly Config Store. Compute@Edge has no process env -- the
/// `EDGEZERO__*` runtime overrides spec 5.2/5.4 expects must come
/// from a Config Store the operator pre-populates (locally via
/// `fastly.toml`'s `[local_server.config_stores.edgezero_runtime_env]`
/// block; remotely via a `fastly config-store` named `edgezero_runtime_env`).
///
/// The Cloudflare adapter does the same thing through `env.var(...)`
/// (lib.rs:55) -- Workers also have no `std::env`. Mirroring the
/// approach here closes the spec 12.7 gap where `__KEY` runtime
/// overrides silently fell back to the binding's default id.
///
/// If the store is missing or empty, returns an empty `EnvConfig` --
/// the rest of the runtime then uses the baked-in defaults (which is
/// what the pre-fix code did, just without the env-driven override
/// path the spec promises).
#[cfg(feature = "fastly")]
fn env_config_from_runtime_dictionary(stores: StoresMetadata) -> EnvConfig {
    use fastly::ConfigStore;
    use std::iter::empty;
    let Ok(dict) = ConfigStore::try_open("edgezero_runtime_env") else {
        return EnvConfig::from_vars(empty::<(String, String)>());
    };
    let mut keys: Vec<String> = vec![
        "EDGEZERO__ADAPTER__HOST".to_owned(),
        "EDGEZERO__ADAPTER__PORT".to_owned(),
        "EDGEZERO__LOGGING__LEVEL".to_owned(),
        "EDGEZERO__LOGGING__ENDPOINT".to_owned(),
        "EDGEZERO__LOGGING__USE_FASTLY_LOGGER".to_owned(),
        "EDGEZERO__LOGGING__ECHO_STDOUT".to_owned(),
    ];
    for (kind, store_meta) in [
        ("CONFIG", stores.config),
        ("KV", stores.kv),
        ("SECRETS", stores.secrets),
    ] {
        if let Some(meta) = store_meta {
            for id in meta.ids {
                let id_upper = id.to_ascii_uppercase();
                keys.push(format!("EDGEZERO__STORES__{kind}__{id_upper}__NAME"));
                if kind == "CONFIG" {
                    keys.push(format!("EDGEZERO__STORES__{kind}__{id_upper}__KEY"));
                }
            }
        }
    }
    let vars = keys
        .into_iter()
        .filter_map(|key| dict.get(&key).map(|value| (key, value)));
    EnvConfig::from_vars(vars)
}

/// Dispatch with a config store wired explicitly. Use `run_app` for
/// the manifest-driven flow that resolves stores automatically. KV
/// is NOT auto-injected on this path; chain `.with_kv(name)` on a
/// `FastlyService` builder if you need KV alongside the config store.
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
    if logging.use_fastly_logger {
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
