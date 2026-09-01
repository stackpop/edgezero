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
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(feature = "fastly")]
use edgezero_core::app::Hooks;
#[cfg(any(feature = "fastly", test))]
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
/// Returns an error if logger setup fails or any required store cannot be opened.
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
/// Returns an error if logger setup fails or any required store cannot be opened.
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
    let env = runtime_env_config(stores);
    let logging = FastlyLogging::from(&env);
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores, &env, extend)
}

/// Build an [`EnvConfig`] from the optional `edgezero_runtime_env`
/// Fastly Config Store.
///
/// Compute@Edge has no process env, so the `EDGEZERO__*` runtime overrides
/// (logging settings, per-store platform names, the config-store `__KEY`
/// selector) come from a Config Store the operator pre-populates: locally via
/// `fastly.toml`'s `[local_server.config_stores.edgezero_runtime_env]` block,
/// remotely via a `fastly config-store` named `edgezero_runtime_env`.
///
/// If the store is missing or empty, returns an empty `EnvConfig` and the rest
/// of the runtime uses its baked-in defaults.
///
/// [`run_app`] and [`run_app_with_request_extensions`] call this themselves.
/// [`run_app_with_config`] does NOT, and neither does a hand-built
/// [`FastlyService`](request::FastlyService): a custom entry point on either of
/// those paths must call this explicitly, or staged and overridden store
/// selectors silently fall back to baked defaults.
///
/// The `stores` argument must name the app's logical store ids. A handwritten
/// [`Hooks`] impl inherits the default `stores()`, which is EMPTY
/// ([`StoresMetadata::default`]), and empty metadata derives no
/// `EDGEZERO__STORES__*` keys at all — every selector override silently never
/// resolves. Such an impl must override `stores()`, or pass explicit
/// [`StoresMetadata`] here.
///
/// ```rust,ignore
/// use edgezero_adapter_fastly::{FastlyLogging, init_logger, runtime_env_config};
/// use edgezero_adapter_fastly::request::dispatch_with_registries;
/// use edgezero_core::app::{StoreMetadata, StoresMetadata};
///
/// let stores = StoresMetadata {
///     config: Some(StoreMetadata {
///         default: "app_config",
///         ids: &["app_config"],
///     }),
///     ..StoresMetadata::default()
/// };
/// let env = runtime_env_config(stores);
/// let logging = FastlyLogging::from(&env);
/// if logging.use_fastly_logger {
///     let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
///     init_logger(endpoint, logging.level, logging.echo_stdout)?;
/// }
/// let app = MyApp::build_app();
/// let _response =
///     dispatch_with_registries(&app, req, stores, &env, |_req, _extensions| {})?;
/// ```
#[cfg(feature = "fastly")]
#[must_use]
#[inline]
pub fn runtime_env_config(stores: StoresMetadata) -> EnvConfig {
    use fastly::ConfigStore;
    use std::iter::empty;
    let Ok(dict) = ConfigStore::try_open(RUNTIME_ENV_STORE_NAME) else {
        // The store is optional -- a clean cutover deploy with all
        // baked-in defaults works without it. But the absence means
        // EDGEZERO__* runtime overrides (spec 5.4 __KEY, spec 5.2
        // __NAME) will silently fall back to baked defaults. Log
        // once at request time so operators can spot the gap in
        // their Fastly logs and run `edgezero provision --adapter fastly`
        // to create the store.
        log::warn!(
            "Fastly Config Store `edgezero_runtime_env` not found; \
             EDGEZERO__* runtime overrides will use baked-in defaults. \
             Run `edgezero provision --adapter fastly` to create the store, \
             then populate per-environment override keys with \
             `fastly config-store-entry update --upsert`."
        );
        return EnvConfig::from_vars(empty::<(String, String)>());
    };
    let vars = runtime_env_keys(stores)
        .into_iter()
        .filter_map(|key| dict.get(&key).map(|value| (key, value)));
    EnvConfig::from_vars(vars)
}

/// The `EDGEZERO__*` keys resolved from the store into the [`EnvConfig`]: the
/// fixed adapter and logging settings, plus a `__NAME` selector for every
/// declared store id and a `__KEY` selector for config-store ids only.
///
/// The Fastly runtime itself consumes only the logging level / endpoint and the
/// per-store selectors; the rest are resolved so downstream readers can fetch
/// them from the returned `EnvConfig`.
// The `test` arm is load-bearing: the crate's default features exclude
// `fastly`, so gating on the feature alone would keep this helper and the test
// pinning its key-derivation rules out of a plain `cargo test --workspace`.
#[cfg(any(feature = "fastly", test))]
fn runtime_env_keys(stores: StoresMetadata) -> Vec<String> {
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
    keys
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

#[cfg(test)]
mod runtime_env_key_tests {
    use super::runtime_env_keys;
    use edgezero_core::app::{StoreMetadata, StoresMetadata};

    #[test]
    fn runtime_env_keys_name_every_store_and_key_only_config_stores() {
        let stores = StoresMetadata {
            config: Some(StoreMetadata {
                default: "main",
                ids: &["main", "edge"],
            }),
            kv: Some(StoreMetadata {
                default: "cache",
                ids: &["cache"],
            }),
            secrets: Some(StoreMetadata {
                default: "vault",
                ids: &["vault"],
            }),
        };

        let mut keys = runtime_env_keys(stores);
        keys.sort();

        assert_eq!(
            keys,
            vec![
                "EDGEZERO__ADAPTER__HOST",
                "EDGEZERO__ADAPTER__PORT",
                "EDGEZERO__LOGGING__ECHO_STDOUT",
                "EDGEZERO__LOGGING__ENDPOINT",
                "EDGEZERO__LOGGING__LEVEL",
                "EDGEZERO__LOGGING__USE_FASTLY_LOGGER",
                "EDGEZERO__STORES__CONFIG__EDGE__KEY",
                "EDGEZERO__STORES__CONFIG__EDGE__NAME",
                "EDGEZERO__STORES__CONFIG__MAIN__KEY",
                "EDGEZERO__STORES__CONFIG__MAIN__NAME",
                "EDGEZERO__STORES__KV__CACHE__NAME",
                "EDGEZERO__STORES__SECRETS__VAULT__NAME",
            ]
        );
    }

    #[test]
    fn runtime_env_keys_without_declared_stores_are_the_fixed_keys_only() {
        let mut keys = runtime_env_keys(StoresMetadata::default());
        keys.sort();

        assert_eq!(
            keys,
            vec![
                "EDGEZERO__ADAPTER__HOST",
                "EDGEZERO__ADAPTER__PORT",
                "EDGEZERO__LOGGING__ECHO_STDOUT",
                "EDGEZERO__LOGGING__ENDPOINT",
                "EDGEZERO__LOGGING__LEVEL",
                "EDGEZERO__LOGGING__USE_FASTLY_LOGGER",
            ]
        );
    }
}
