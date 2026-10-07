//! Utilities for bridging Fastly Compute@Edge requests into the
//! `edgezero-core` service abstractions.

#![cfg_attr(
    feature = "fastly",
    expect(
        clippy::pub_use,
        reason = "reuse the platform serving limits and summary"
    )
)]

#[cfg(feature = "fastly")]
use anyhow::Context as _;

// Only compiled where it is actually used (the CLI push/GC path and the Fastly
// runtime resolver). Gating it keeps a `--no-default-features` build dead-code
// clean instead of dragging in helpers no feature references.
#[cfg(any(
    feature = "fastly",
    test,
    all(feature = "cli", not(target_arch = "wasm32"))
))]
pub(crate) mod chunked_config;
#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod cli;
#[cfg(feature = "fastly")]
pub mod config_store;
pub mod context;
#[cfg(feature = "fastly")]
pub mod key_value_store;
pub mod lifecycle;
#[cfg(any(feature = "fastly", all(test, not(target_arch = "wasm32"))))]
pub mod logger;
#[cfg(any(test, feature = "test-utils", feature = "fastly"))]
pub mod outbound;
#[cfg(feature = "fastly")]
pub mod request;
#[cfg(any(test, feature = "fastly"))]
pub mod response;
#[cfg(feature = "fastly")]
pub mod secret_store;

#[cfg(any(feature = "fastly", test))]
use edgezero_core::app::StoresMetadata;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::app::{App, Hooks};
#[cfg(any(feature = "fastly", test))]
use edgezero_core::env_config::EnvConfig;
#[cfg(any(feature = "fastly", test))]
use edgezero_core::error::EdgeError;
#[cfg(feature = "fastly")]
use edgezero_core::http::{Extensions, Response};
#[cfg(any(feature = "fastly", test))]
use edgezero_core::manifest::ResolvedLoggingConfig;
#[cfg(feature = "fastly")]
use fastly::compute_runtime::service_id;
#[cfg(feature = "fastly")]
pub use fastly::http::serve::{Serve, ServeSummary};
#[cfg(any(feature = "fastly", test))]
use std::mem;
use std::num::NonZeroU32;
#[cfg(feature = "fastly")]
use std::sync::Once;

/// Fastly Compute's published per-execution heap and stack limits.
pub const FASTLY_PLATFORM: edgezero_core::PlatformMetadata = edgezero_core::PlatformMetadata::new(
    edgezero_core::PlatformFact::known(
        edgezero_core::MemoryCeiling::new(
            128_000_000,
            edgezero_core::MemoryCeilingScope::PerExecution,
            Some(1_000_000),
        ),
        edgezero_core::PlatformResourceSource::PlatformLimit {
            provider: "Fastly Compute",
        },
    ),
    edgezero_core::PlatformFact::known(
        edgezero_core::InboundRequestPopulationBound::new(NonZeroU32::MIN),
        edgezero_core::PlatformResourceSource::PlatformLimit {
            provider: "Fastly Compute",
        },
    ),
    edgezero_core::PlatformFact::unknown(edgezero_core::PlatformUnknownReason::ProviderUnpublished),
);

#[cfg(any(feature = "fastly", all(feature = "cli", not(target_arch = "wasm32"))))]
const RUNTIME_ENV_PREFIX: &str = "EDGEZERO__";

#[cfg(any(feature = "fastly", test))]
const RUNTIME_ENV_WARNING: &str = "Fastly Config Store `edgezero_runtime_env` could not be opened; \
     EDGEZERO__* runtime overrides will use baked-in defaults. \
     Run `edgezero provision --adapter fastly` to create the store, \
     then populate per-environment override keys with \
     `fastly config-store-entry update --upsert`.";

/// Name of the Fastly Config Store the runtime opens for `EDGEZERO__*`
/// overrides.
///
/// The fixed name is load-bearing: a staged deploy creates a per-service
/// staging twin and links it into the staged version under THIS name, which is
/// how the runtime resolves staged selectors without knowing the twin exists.
pub const RUNTIME_ENV_STORE_NAME: &str = "edgezero_runtime_env";

#[cfg(feature = "fastly")]
static FASTLY_ABI_INIT: Once = Once::new();

#[cfg(any(feature = "fastly", test))]
#[derive(Debug, Clone)]
pub struct FastlyLogging {
    pub echo_stdout: bool,
    pub endpoint: Option<String>,
    pub level: log::LevelFilter,
    pub use_fastly_logger: bool,
}

/// Resolved runtime overrides and bounded diagnostics deferred until logging is ready.
#[cfg(any(feature = "fastly", test))]
#[derive(Debug)]
#[expect(
    clippy::partial_pub_fields,
    reason = "runtime overrides are public but one-shot diagnostic state must remain private"
)]
pub struct FastlyRuntimeConfig {
    /// Canonical `EDGEZERO__*` overrides for logging and request dispatch.
    pub env: EnvConfig,
    runtime_env_unavailable: bool,
}

#[cfg(any(feature = "fastly", test))]
impl FastlyRuntimeConfig {
    /// Emit pending boot warnings at most once, after installing a logging backend.
    /// Without a backend the warning is not observable; this does not install one.
    #[inline]
    pub fn emit_boot_diagnostics(&mut self) {
        if let Some(warning) = self.take_boot_warning() {
            log::warn!(target: edgezero_core::BOOT_LOG_TARGET, "{warning}");
        }
    }

    fn take_boot_warning(&mut self) -> Option<&'static str> {
        mem::take(&mut self.runtime_env_unavailable).then_some(RUNTIME_ENV_WARNING)
    }
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
        let level = edgezero_core::resolve_logging_level(env);
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

#[cfg(feature = "fastly")]
fn build_app_for_dispatch<A: Hooks>() -> Result<App, fastly::Error> {
    build_target_app::<A>().context("application configuration failed")
}

#[cfg(any(feature = "fastly", test))]
fn build_target_app<A: Hooks>() -> Result<App, EdgeError> {
    App::build::<A>(FASTLY_PLATFORM)
}

/// Test seam for the production application-assembly error mapping.
#[cfg(all(feature = "test-utils", feature = "fastly"))]
#[doc(hidden)]
#[inline]
pub fn build_app_for_test<A: Hooks>() -> Result<App, fastly::Error> {
    build_app_for_dispatch::<A>()
}

#[cfg(feature = "fastly")]
fn init_fastly_abi() {
    FASTLY_ABI_INIT.call_once(fastly::init);
}

/// Prefix a canonical `EDGEZERO__*` key with its owning Fastly service.
///
/// The shared `edgezero_runtime_env` Config Store is account-wide. Service
/// scoping prevents two linked services that declare the same logical store id
/// from overwriting one another's runtime mappings.
#[cfg(any(feature = "fastly", all(feature = "cli", not(target_arch = "wasm32"))))]
fn service_scoped_runtime_env_key(service_id: &str, canonical_key: &str) -> String {
    let suffix = canonical_key
        .strip_prefix(RUNTIME_ENV_PREFIX)
        .unwrap_or(canonical_key);
    format!("{RUNTIME_ENV_PREFIX}SERVICES__{service_id}__{suffix}")
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
pub fn run_app<A: Hooks>() -> Result<(), fastly::Error> {
    run_app_with_hooks::<A, _, _, ()>(|_req, _extensions| {}, |_response| ()).map(|_state| ())
}

/// Runs the manifest-wired app through Fastly's closed request/response lifecycle.
///
/// `prepare` borrows the raw request and extension bag before conversion. `finalize` borrows only
/// routed responses before response-egress policy and framing. `EdgeZero` retains transmission
/// ownership; routed hook state is returned only after terminal delivery, while detached ingress
/// responses skip `finalize` and return `None`.
///
/// # Errors
/// Returns an error if logger setup fails or any required store cannot be opened.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_hooks<A, Prepare, Finalize, State>(
    prepare: Prepare,
    finalize: Finalize,
) -> Result<Option<State>, fastly::Error>
where
    A: Hooks,
    Prepare: FnOnce(&mut fastly::Request, &mut Extensions),
    Finalize: FnOnce(&mut Response) -> State,
{
    init_fastly_abi();
    let stores = A::stores();
    let mut config = runtime_env_config(stores);
    let logging = FastlyLogging::from(&config.env);
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = build_app_for_dispatch::<A>();
    config.emit_boot_diagnostics();
    request::send_with_registries_and_hooks(&app?, stores, &config.env, prepare, finalize)
}

/// Retain a successfully built app while the SDK serves sequential requests.
///
/// Each callback resolves fresh runtime configuration and store registries. Logging
/// uses the first snapshot, and the adapter owns response delivery without SDK resends.
#[cfg(feature = "fastly")]
#[must_use = "inspect the summary or handle its terminal error with into_result()"]
#[inline]
pub fn serve_app<A: Hooks>(serve: Serve) -> ServeSummary<fastly::Error> {
    serve_app_with_hooks::<A, _, _, _>(serve, |_request, _extensions| {}, |_response| ())
}

/// Retained serving with request-local preparation and routed response finalization.
///
/// The hooks follow [`run_app_with_hooks`]. Preparation runs before admission;
/// finalization runs only for routed responses, before adapter-owned delivery.
/// Captures persist across callbacks and must not accidentally retain request resources.
#[cfg(feature = "fastly")]
#[must_use = "inspect the summary or handle its terminal error with into_result()"]
#[inline]
pub fn serve_app_with_hooks<A, Prepare, Finalize, State>(
    serve: Serve,
    mut prepare: Prepare,
    mut finalize: Finalize,
) -> ServeSummary<fastly::Error>
where
    A: Hooks,
    Prepare: FnMut(&mut fastly::Request, &mut Extensions),
    Finalize: FnMut(&mut Response) -> State,
{
    init_fastly_abi();
    let stores = A::stores();
    lifecycle::serve_custom(serve, move |req, retained: &mut lifecycle::Sandbox<App>| {
        let mut runtime = runtime_env_config(stores);
        retained.setup_once(|| {
            let logging = FastlyLogging::from(&runtime.env);
            if logging.use_fastly_logger && !A::owns_logging() {
                init_logger(
                    logging.endpoint.as_deref().unwrap_or("stdout"),
                    logging.level,
                    logging.echo_stdout,
                )?;
            }
            Ok::<(), fastly::Error>(())
        })?;
        runtime.emit_boot_diagnostics();
        retained.initialize(build_app_for_dispatch::<A>)?;
        let app = retained.state().ok_or_else(|| {
            fastly::Error::msg("application initialization did not retain an app")
        })?;
        request::send_request_with_registries_and_hooks(
            app,
            stores,
            req,
            &runtime.env,
            &mut prepare,
            &mut finalize,
        )
        .map(|_state| ())
    })
}

/// Build an [`EnvConfig`] from the optional `edgezero_runtime_env`
/// Fastly Config Store.
///
/// Compute@Edge has no process env, so the `EDGEZERO__*` runtime overrides
/// come from the Config Store. The function reads a fixed allowlist: adapter
/// host and port, logging settings, `__NAME` entries for declared stores, and
/// `__KEY` entries for declared config stores.
///
/// Each lookup uses the current Fastly service's
/// `EDGEZERO__SERVICES__<SERVICE_ID>__*` key. Legacy unscoped entries are not
/// read because they have no safe owner when this Config Store is linked to more
/// than one service. The returned [`EnvConfig`] contains canonical unscoped keys.
///
/// [`run_app`] and [`run_app_with_hooks`] call this themselves.
/// [`run_app_with_config`] does NOT, and neither does a hand-built
/// [`FastlyService`](request::FastlyService). A custom entry point on either path
/// must call this explicitly.
///
/// The `stores` argument must name the app's logical store ids. A handwritten
/// [`Hooks`] impl inherits the empty [`StoresMetadata::default`] and must
/// override `stores()` or pass explicit metadata here.
///
/// If the store cannot be opened, the result contains an empty [`EnvConfig`] and
/// one deferred warning. Custom entrypoints must initialize their logger, then
/// call [`FastlyRuntimeConfig::emit_boot_diagnostics`]. No logging occurs here.
#[cfg(feature = "fastly")]
#[must_use]
#[inline]
pub fn runtime_env_config(stores: StoresMetadata) -> FastlyRuntimeConfig {
    use fastly::ConfigStore;
    let Ok(dict) = ConfigStore::try_open(RUNTIME_ENV_STORE_NAME) else {
        return FastlyRuntimeConfig {
            env: EnvConfig::default(),
            runtime_env_unavailable: true,
        };
    };
    let current_service_id = service_id();
    let vars = runtime_env_vars_for_service(stores, current_service_id, |key| dict.get(key));
    FastlyRuntimeConfig {
        env: EnvConfig::from_vars(vars),
        runtime_env_unavailable: false,
    }
}

#[cfg(any(
    feature = "fastly",
    all(feature = "cli", test, not(target_arch = "wasm32"))
))]
fn runtime_env_vars_for_service<F>(
    stores: StoresMetadata,
    service_id: &str,
    mut get: F,
) -> Vec<(String, String)>
where
    F: FnMut(&str) -> Option<String>,
{
    runtime_env_keys(stores)
        .into_iter()
        .filter_map(|canonical_key| {
            let scoped_key = service_scoped_runtime_env_key(service_id, &canonical_key);
            get(&scoped_key).map(|value| (canonical_key, value))
        })
        .collect()
}

/// The `EDGEZERO__*` keys resolved from the store into the [`EnvConfig`]: the
/// fixed adapter and logging settings, plus a `__NAME` selector for every
/// declared store id and a `__KEY` selector for config-store ids only.
// The `test` arm keeps the key derivation tests in default workspace tests.
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
/// [`runtime_env_config`] with [`request::send_with_registries_and_hooks`] for the
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
    config_store_name: Option<&str>,
) -> Result<(), fastly::Error> {
    init_fastly_abi();
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = build_app_for_dispatch::<A>()?;
    let mut service = request::FastlyService::new(&app);
    if let Some(name) = config_store_name {
        service = service.with_config(name);
    }
    service.send()
}

#[cfg(test)]
mod fastly_logging_tests {
    use super::*;
    use edgezero_core::manifest::LogLevel;

    #[test]
    fn runtime_env_warning_is_deferred_and_taken_once() {
        let mut config = FastlyRuntimeConfig {
            env: EnvConfig::default(),
            runtime_env_unavailable: true,
        };
        assert!(config.env.logging_level().is_none());
        assert_eq!(config.take_boot_warning(), Some(RUNTIME_ENV_WARNING));
        assert!(config.take_boot_warning().is_none());
    }

    #[test]
    fn available_empty_runtime_env_has_no_boot_warning() {
        let mut config = FastlyRuntimeConfig {
            env: EnvConfig::default(),
            runtime_env_unavailable: false,
        };
        assert!(config.take_boot_warning().is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn public_boot_diagnostic_emission_is_once_and_empty_store_is_silent() {
        let guard = log_fastly::reset_logger(
            log_fastly::Logger::builder()
                .default_endpoint("diagnostic-test")
                .max_level(log::LevelFilter::Warn)
                .build()
                .expect("SDK capture"),
        );
        let mut config = FastlyRuntimeConfig {
            env: EnvConfig::default(),
            runtime_env_unavailable: true,
        };
        config.emit_boot_diagnostics();
        guard.assert_contains("diagnostic-test", RUNTIME_ENV_WARNING);
        log::warn!("diagnostic emission sentinel");
        config.emit_boot_diagnostics();
        guard.assert_contains("diagnostic-test", "diagnostic emission sentinel");
        let mut available = FastlyRuntimeConfig {
            env: EnvConfig::default(),
            runtime_env_unavailable: false,
        };
        available.emit_boot_diagnostics();
        guard.assert_contains("diagnostic-test", "diagnostic emission sentinel");
    }

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

#[cfg(test)]
mod platform_tests {
    use edgezero_core::app::{App, Hooks};
    use edgezero_core::error::EdgeError;
    use edgezero_core::router::RouterService;

    struct PlatformAwareConfiguration;

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook exercises only target metadata propagation"
    )]
    impl Hooks for PlatformAwareConfiguration {
        fn configure(app: &mut App) -> Result<(), EdgeError> {
            if app.platform() == crate::FASTLY_PLATFORM {
                Ok(())
            } else {
                Err(EdgeError::service_unavailable("wrong application platform"))
            }
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[test]
    fn application_configuration_receives_fastly_platform_metadata() {
        let app = super::build_target_app::<PlatformAwareConfiguration>()
            .expect("configured application");

        assert_eq!(app.platform(), crate::FASTLY_PLATFORM);
    }
}
