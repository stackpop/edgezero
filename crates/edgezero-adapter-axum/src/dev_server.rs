use std::any::Any;
use std::fs;
use std::future;
#[cfg(test)]
use std::iter;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
use tokio::sync::oneshot::{Receiver as ShutdownReceiver, Sender as ShutdownSender, channel};
use tokio::task::{LocalSet, spawn_blocking};

use edgezero_core::addr;
use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::config_store::{ConfigStoreError, ConfigStoreHandle, ConfigStoreOpenFailure};
use edgezero_core::env_config::EnvConfig;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::logging::{BOOT_LOG_LEVEL, BOOT_LOG_TARGET, resolve_logging_level};
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry, StoreRegistry,
};
use simple_logger::SimpleLogger;
use std::collections::BTreeMap;

use crate::config_store::AxumConfigStore;
use crate::config_store_limits::ConfigStoreLimits;
use crate::connection::{ConnectionExit, serve_http1};
use crate::ingress_config::AxumIngressConfig;
use crate::key_value_store::PersistentKvStore;
use crate::response::EgressConnection;
use crate::secret_store::EnvSecretStore;
use crate::service::AxumServiceState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KvInitRequirement {
    Optional,
    Required,
}

type ConnectionResult = Result<Result<ConnectionExit, hyper::Error>, Box<dyn Any + Send>>;

/// Configuration used when running the dev server embedding `EdgeZero` into Axum.
#[derive(Clone)]
pub struct AxumDevServerConfig {
    pub addr: SocketAddr,
    pub enable_ctrl_c: bool,
    /// Validated listener, parser and idle-plus-header limits.
    pub ingress: AxumIngressConfig,
}

impl Default for AxumDevServerConfig {
    #[inline]
    fn default() -> Self {
        Self {
            addr: SocketAddr::from((addr::DEFAULT_HOST, addr::DEFAULT_PORT)),
            enable_ctrl_c: true,
            ingress: AxumIngressConfig::default(),
        }
    }
}

/// Optional store handles attached to every request processed by the dev server.
///
/// Both single-handle fields and registry fields can be set; the service inserts
/// whichever are present. Registries take precedence in `RequestContext`.
#[derive(Default)]
struct Stores {
    config_registry: Option<ConfigRegistry>,
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    secret_registry: Option<SecretRegistry>,
    secrets: Option<SecretHandle>,
}

/// Blocking dev server runner used by the `EdgeZero` CLI.
pub struct AxumDevServer {
    config: AxumDevServerConfig,
    router: RouterService,
    stores: Stores,
}

impl AxumDevServer {
    #[must_use]
    #[inline]
    pub fn new(router: RouterService) -> Self {
        Self {
            config: AxumDevServerConfig::default(),
            router,
            stores: Stores::default(),
        }
    }

    /// # Errors
    /// Returns an error if the dev server fails to bind, the Tokio runtime fails to start, or the underlying request loop returns an error.
    #[inline]
    pub fn run(self) -> anyhow::Result<()> {
        let runtime = RuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime")?;

        runtime.block_on(async move { self.run_async().await })
    }

    async fn run_async(self) -> anyhow::Result<()> {
        let AxumDevServer {
            router,
            config,
            stores,
        } = self;

        // Allow binding to already-open listener if caller created one to surface errors early.
        let std_listener = StdTcpListener::bind(config.addr)
            .with_context(|| format!("failed to bind dev server to {}", config.addr))?;
        std_listener
            .set_nonblocking(true)
            .context("failed to set listener to non-blocking")?;

        let listener = TokioTcpListener::from_std(std_listener)
            .context("failed to adopt std listener into tokio")?;

        serve_with_stores(
            App::new(router),
            listener,
            config.enable_ctrl_c,
            config.ingress,
            stores,
        )
        .await
    }

    #[cfg(test)]
    async fn run_with_listener(self, listener: TokioTcpListener) -> anyhow::Result<()> {
        let AxumDevServer {
            router,
            config,
            stores,
        } = self;
        serve_with_stores(
            App::new(router),
            listener,
            config.enable_ctrl_c,
            config.ingress,
            stores,
        )
        .await
    }

    #[must_use]
    #[inline]
    pub fn with_config(router: RouterService, config: AxumDevServerConfig) -> Self {
        Self {
            config,
            router,
            stores: Stores::default(),
        }
    }

    #[must_use]
    #[inline]
    pub fn with_config_registry(mut self, registry: ConfigRegistry) -> Self {
        self.stores.config_registry = Some(registry);
        self
    }

    #[must_use]
    #[inline]
    pub fn with_config_store(mut self, handle: ConfigStoreHandle) -> Self {
        self.stores.config_store = Some(handle);
        self
    }

    /// Attach a KV store to the dev server.
    ///
    /// The handle is shared across all requests, making the `Kv` extractor
    /// available in handlers.
    #[must_use]
    #[inline]
    pub fn with_kv_handle(mut self, handle: KvHandle) -> Self {
        self.stores.kv = Some(handle);
        self
    }

    /// Attach an id-keyed KV registry to the dev server.
    #[must_use]
    #[inline]
    pub fn with_kv_registry(mut self, registry: KvRegistry) -> Self {
        self.stores.kv_registry = Some(registry);
        self
    }

    /// Attach a secret store to the dev server.
    ///
    /// The handle is shared across all requests, making the `Secrets` extractor
    /// available in handlers.
    #[must_use]
    #[inline]
    pub fn with_secret_handle(mut self, handle: SecretHandle) -> Self {
        self.stores.secrets = Some(handle);
        self
    }

    /// Attach an id-keyed secret registry to the dev server.
    #[must_use]
    #[inline]
    pub fn with_secret_registry(mut self, registry: SecretRegistry) -> Self {
        self.stores.secret_registry = Some(registry);
        self
    }
}

fn kv_init_requirement(stores: StoresMetadata) -> KvInitRequirement {
    if stores.kv.is_some() {
        KvInitRequirement::Required
    } else {
        KvInitRequirement::Optional
    }
}

fn kv_store_path(store_name: &str) -> PathBuf {
    // Every declared id gets its own slug-based filename. The
    // pre-rewrite hard-coded `.edgezero/kv.redb` shortcut for
    // store_name == "EDGEZERO_KV" is gone -- the runtime no longer
    // hands out a default name; if you reach here you have a real
    // declared id and the slug encoding handles every shape
    // uniformly.
    PathBuf::from(".edgezero").join(format!(
        "kv-{}-{:016x}.redb",
        store_name_slug(store_name),
        stable_store_name_hash(store_name)
    ))
}

fn store_name_slug(store_name: &str) -> String {
    const MAX_SLUG_LEN: usize = 24;

    let mut slug = String::with_capacity(MAX_SLUG_LEN);
    let mut last_was_separator = false;
    for ch in store_name.chars() {
        let mapped = ch.is_ascii_alphanumeric().then(|| ch.to_ascii_lowercase());

        match mapped {
            Some(lower_ch) => {
                if slug.len() == MAX_SLUG_LEN {
                    break;
                }
                slug.push(lower_ch);
                last_was_separator = false;
            }
            None if !slug.is_empty() && !last_was_separator => {
                if slug.len() == MAX_SLUG_LEN {
                    break;
                }
                slug.push('-');
                last_was_separator = true;
            }
            None => {}
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "store".to_owned()
    } else {
        slug
    }
}

fn stable_store_name_hash(store_name: &str) -> u64 {
    // Deterministic FNV-1a keeps local KV file names stable across processes.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in store_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0001_0000_01b3);
    }
    hash
}

fn kv_handle_from_path(kv_path: &Path) -> anyhow::Result<KvHandle> {
    if let Some(parent) = kv_path.parent() {
        fs::create_dir_all(parent).context("failed to create KV store directory")?;
    }
    let kv_store = Arc::new(PersistentKvStore::new(kv_path).context("failed to create KV store")?);
    log::info!("KV store: {}", kv_path.display());
    Ok(KvHandle::new(kv_store))
}

async fn serve_with_stores(
    app: App,
    listener: TokioTcpListener,
    enable_ctrl_c: bool,
    ingress: AxumIngressConfig,
    stores: Stores,
) -> anyhow::Result<()> {
    struct ShutdownOnDrop(Option<ShutdownSender<()>>);

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _sent = sender.send(());
            }
        }
    }

    let native_listener = listener
        .into_std()
        .context("failed to release tokio listener")?;
    let (shutdown_sender, shutdown_receiver) = channel();
    let shutdown_guard = ShutdownOnDrop(Some(shutdown_sender));
    let worker = spawn_blocking(move || {
        serve_local(
            app,
            native_listener,
            enable_ctrl_c,
            ingress,
            stores,
            shutdown_receiver,
        )
    });
    let result = worker.await.context("axum server thread failed")?;
    drop(shutdown_guard);
    result
}

fn serve_local(
    app: App,
    std_listener: StdTcpListener,
    enable_ctrl_c: bool,
    ingress: AxumIngressConfig,
    stores: Stores,
    shutdown_receiver: ShutdownReceiver<()>,
) -> anyhow::Result<()> {
    let mut service = AxumServiceState::from_app(app);
    if let Some(registry) = stores.config_registry {
        service = service.with_config_registry(registry);
    }
    if let Some(handle) = stores.config_store {
        service = service.with_config_store_handle(handle);
    }
    if let Some(registry) = stores.kv_registry {
        service = service.with_kv_registry(registry);
    }
    if let Some(handle) = stores.kv {
        service = service.with_kv_handle(handle);
    }
    if let Some(registry) = stores.secret_registry {
        service = service.with_secret_registry(registry);
    }
    if let Some(handle) = stores.secrets {
        service = service.with_secret_handle(handle);
    }

    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build axum connection runtime")?;
    let local = LocalSet::new();
    #[expect(
        clippy::integer_division_remainder_used,
        reason = "tokio::select! expands to internal randomized branch selection arithmetic"
    )]
    let serve = async move {
        let listener = TokioTcpListener::from_std(std_listener)
            .context("failed to adopt listener on axum connection runtime")?;
        let shutdown = async move {
            let _closed = shutdown_receiver.await;
        };
        let ctrl_c = async move {
            if enable_ctrl_c {
                let _signal = signal::ctrl_c().await;
            } else {
                future::pending::<()>().await;
            }
        };
        tokio::pin!(shutdown);
        tokio::pin!(ctrl_c);
        let mut connections = FuturesUnordered::new();

        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                () = &mut ctrl_c => break,
                Some(result) = connections.next(), if !connections.is_empty() => {
                    report_connection_exit(&result);
                }
                accepted = listener.accept() => {
                    let (stream, remote_addr) = accepted.context("axum listener accept failed")?;
                    // One transient accepted socket; no parser, task or waiter on refusal.
                    if connections.len() >= ingress.max_connections() {
                        drop(stream);
                        continue;
                    }
                    let responses = EgressConnection::default();
                    let connection_service = service.for_connection(remote_addr, responses.clone());
                    connections.push(AssertUnwindSafe(serve_http1(stream, connection_service, responses, ingress)).catch_unwind());
                }
            }
        }
        // Dropping the bounded owner releases sockets, parser buffers and egress attempts.
        drop(connections);
        Ok(())
    };
    runtime.block_on(local.run_until(serve))
}

fn report_connection_exit(result: &ConnectionResult) {
    if !matches!(result, Ok(Ok(_))) {
        log::debug!("axum HTTP/1 connection rejected or disconnected");
    }
}

/// Entry point for an Axum dev-server application.
///
/// Portable store config is baked into `A` by the `app!` macro; adapter-specific
/// values (platform store names, bind host/port, logging level) are read at
/// runtime from `EDGEZERO__*` environment variables. No `edgezero.toml` is
/// required.
///
/// # Errors
/// Returns an error if the dev server fails to bind or any required store handle cannot be initialised.
fn build_app_for_dispatch<A: Hooks>() -> anyhow::Result<App> {
    App::build::<A>(crate::AXUM_PLATFORM).context("application configuration failed")
}

fn app_logger(env: &EnvConfig) -> SimpleLogger {
    SimpleLogger::new()
        .with_level(resolve_logging_level(env))
        .with_module_level(BOOT_LOG_TARGET, BOOT_LOG_LEVEL)
}

/// Runs an application with the Axum development server.
///
/// # Errors
/// Returns an error if logger setup, application configuration, runtime setup, store initialization, listener
/// binding, or connection serving fails.
#[inline]
pub fn run_app<A: Hooks>() -> anyhow::Result<()> {
    run_app_with_preflight::<A, _>(|| Ok(()))
}

/// Run a startup check after logger initialization and before application configuration.
///
/// Useful for capability validation in embedded runners. Applications that own logging
/// must install their backend and filters before calling this function.
///
/// # Errors
/// Returns an error if logger setup, `preflight`, application configuration, runtime setup,
/// store initialization, listener binding, or connection serving fails.
#[inline]
pub fn run_app_with_preflight<A, F>(preflight: F) -> anyhow::Result<()>
where
    A: Hooks,
    F: FnOnce() -> anyhow::Result<()>,
{
    let env = EnvConfig::from_env();
    if !A::owns_logging() {
        app_logger(&env)
            .init()
            .context("failed to initialize application logger")?;
    }
    let ingress = AxumIngressConfig::from_env(&env)?;
    let config_limits = ConfigStoreLimits::from_env(&env)?;
    log::info!("[edgezero] axum HTTP/1 ingress limits: {ingress:?}");
    log::info!("[edgezero] axum config snapshot limits: {config_limits:?}");
    preflight()?;
    let app = build_app_for_dispatch::<A>()?;
    let stores = A::stores();
    let kv_init_requirement = kv_init_requirement(stores);
    let kv_registry = build_kv_registry(stores.kv, &env, kv_init_requirement)?;
    let config_registry = build_config_registry(stores.config, &env, config_limits)?;
    let secret_registry = build_secret_registry(stores.secrets, &env);

    let resolution = resolve_addr(&env);
    for warning in &resolution.warnings {
        log::warn!(target: BOOT_LOG_TARGET, "{warning}");
    }
    let addr = resolution.addr;
    log::info!("[edgezero] starting axum server on http://{addr}");

    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move {
        let std_listener = StdTcpListener::bind(addr)
            .with_context(|| format!("failed to bind dev server to {addr}"))?;
        std_listener
            .set_nonblocking(true)
            .context("failed to set listener to non-blocking")?;
        let listener = TokioTcpListener::from_std(std_listener)
            .context("failed to adopt std listener into tokio")?;

        let request_stores = Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Stores::default()
        };
        serve_with_stores(app, listener, true, ingress, request_stores).await
    })
}

/// Build the per-request KV registry from baked store metadata.
///
/// Each declared id resolves to a [`PersistentKvStore`] at
/// `.edgezero/kv-<slug>-<hash>.redb`, where the file name is derived from the
/// platform store name (`EDGEZERO__STORES__KV__<ID>__NAME` or the id default).
fn build_kv_registry(
    kv_meta: Option<StoreMetadata>,
    env: &EnvConfig,
    init: KvInitRequirement,
) -> anyhow::Result<Option<KvRegistry>> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };

    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("kv", id);
        let kv_path = kv_store_path(&store_name);
        let handle = match kv_handle_from_path(&kv_path) {
            Ok(handle) => handle,
            Err(err) => match init {
                KvInitRequirement::Optional => {
                    log::warn!(
                        target: BOOT_LOG_TARGET,
                        "KV store '{}' (id `{}`) could not be initialized at {}: {}",
                        store_name,
                        id,
                        kv_path.display(),
                        err
                    );
                    continue;
                }
                KvInitRequirement::Required => {
                    return Err(err.context(format!(
                        "KV store '{}' (id `{}`) is explicitly configured for axum but could not be initialized at {}",
                        store_name,
                        id,
                        kv_path.display()
                    )));
                }
            },
        };
        by_id.insert((*id).to_owned(), handle);
    }

    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            target: BOOT_LOG_TARGET,
            "KV registry default id `{default_id}` failed to initialize; dropping the KV registry — \
             handlers will see no KV store"
        );
    }
    Ok(StoreRegistry::from_parts(by_id, default_id))
}

/// Build the per-request config registry from the per-id local-file stores.
///
/// Each declared id reads `.edgezero/local-config-<id>.json`. A missing
/// file yields an empty store for that id — the dev server stays usable
/// before any `config push` has populated the file. A malformed file logs a
/// warning and preserves a typed failing binding. Allocation violations fail
/// startup before any listener is bound. Snapshots load sequentially.
fn build_config_registry(
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
    limits: ConfigStoreLimits,
) -> Result<Option<ConfigRegistry>, ConfigStoreError> {
    build_config_registry_at(config_meta, env, limits, AxumConfigStore::local_path)
}

fn build_config_registry_at(
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
    limits: ConfigStoreLimits,
    path_for: impl Fn(&str) -> PathBuf,
) -> Result<Option<ConfigRegistry>, ConfigStoreError> {
    let Some(meta) = config_meta else {
        return Ok(None);
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
        return Err(ConfigStoreError::internal(anyhow::anyhow!(
            "invalid declared config-store metadata"
        )));
    }
    let mut by_id: BTreeMap<String, ConfigStoreBinding> = BTreeMap::new();
    let mut resident = 0_usize;
    for id in meta.ids {
        let handle = match AxumConfigStore::load_at_startup(&path_for(id), limits, resident) {
            Ok(store) => {
                resident = resident
                    .checked_add(store.resident_allocation_bytes())
                    .ok_or(ConfigStoreError::ValueTooLarge)?;
                ConfigStoreHandle::new(Arc::new(store))
            }
            Err(ConfigStoreError::ValueTooLarge) => return Err(ConfigStoreError::ValueTooLarge),
            Err(_error) => {
                log::warn!(
                    target: BOOT_LOG_TARGET,
                    "config snapshot unavailable; retaining a failed declared binding"
                );
                ConfigStoreHandle::failed_open(ConfigStoreOpenFailure::Unavailable)
            }
        };
        by_id.insert(
            (*id).to_owned(),
            ConfigStoreBinding {
                handle,
                default_key: env.store_key("config", id),
            },
        );
    }
    let default_id = meta.default.to_owned();
    log::info!("[edgezero] axum config snapshot resident charge: {resident} bytes");
    Ok(StoreRegistry::from_parts(by_id, default_id))
}

/// Build the per-request secret registry. Axum is `Single` for secrets — every
/// declared id maps to the same env-backed [`EnvSecretStore`]. Each binding
/// captures the platform store name resolved from
/// `EDGEZERO__STORES__SECRETS__<ID>__NAME` (defaulting to the logical id);
/// the axum env-secret backend ignores the name on lookup, so the binding
/// is observable only via [`BoundSecretStore::store_name`].
fn build_secret_registry(
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    log::info!("Secret store: reading from environment variables");
    let handle = SecretHandle::new(Arc::new(EnvSecretStore::new()));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    // Secret backends are infallible here, so the default id is always
    // present in `by_id`; `from_parts` keeps the API symmetric with the
    // KV / config builders without changing observable behaviour.
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

/// Resolve the bind address from `EDGEZERO__ADAPTER__*` environment config.
///
/// Precedence (highest wins):
/// 1. `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`
/// 2. Default: `127.0.0.1:8787`
pub(crate) fn resolve_addr(env: &EnvConfig) -> addr::BindAddrResolution {
    addr::resolve_bind_addr(env.adapter_host(), env.adapter_port(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::error::EdgeError;
    use futures::executor::block_on;
    use log::LevelFilter;
    use std::env;
    use std::net::{IpAddr, Ipv4Addr};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingConfiguration;

    struct LoggingConfiguration;

    struct OwnedLoggingConfiguration;

    struct PlatformAwareConfiguration;

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook exercises only adapter startup failure"
    )]
    impl Hooks for FailingConfiguration {
        fn configure(_app: &mut App) -> Result<(), EdgeError> {
            Err(EdgeError::service_unavailable("configuration unavailable"))
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook probes startup logging"
    )]
    impl Hooks for LoggingConfiguration {
        fn configure(_app: &mut App) -> Result<(), EdgeError> {
            log::warn!(target: edgezero_core::BOOT_LOG_TARGET, "boot-warning-probe");
            log::info!(target: edgezero_core::BOOT_LOG_TARGET, "boot-info-probe");
            log::warn!("runtime-warning-probe");
            log::debug!("runtime-debug-probe");
            Err(EdgeError::service_unavailable("stop before listening"))
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook probes application logger ownership"
    )]
    impl Hooks for OwnedLoggingConfiguration {
        fn configure(app: &mut App) -> Result<(), EdgeError> {
            LoggingConfiguration::configure(app)
        }

        fn owns_logging() -> bool {
            true
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook exercises only target metadata propagation"
    )]
    impl Hooks for PlatformAwareConfiguration {
        fn configure(app: &mut App) -> Result<(), EdgeError> {
            if app.platform() == crate::AXUM_PLATFORM {
                Ok(())
            } else {
                Err(EdgeError::service_unavailable("wrong application platform"))
            }
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    fn logging_probe(scenario: &str, level: &str) -> String {
        let output = Command::new(env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "dev_server::tests::startup_logging_child",
                "--nocapture",
            ])
            .env("EDGEZERO_TEST_LOGGING_SCENARIO", scenario)
            .env("EDGEZERO__LOGGING__LEVEL", level)
            .env_remove("EDGEZERO__ADAPTER__INGRESS__MAX_CONNECTIONS")
            .env_remove("EDGEZERO__ADAPTER__INGRESS__MAX_RAW_HEAD_BYTES")
            .env_remove("EDGEZERO__ADAPTER__INGRESS__MAX_HEADER_COUNT")
            .env_remove("EDGEZERO__ADAPTER__INGRESS__HEAD_READ_TIMEOUT_MS")
            .envs(
                (scenario == "invalid-ingress")
                    .then_some(("EDGEZERO__ADAPTER__INGRESS__MAX_CONNECTIONS", "0")),
            )
            .output()
            .expect("isolated logging probe");
        let stdout = String::from_utf8(output.stdout).expect("probe stdout");
        let stderr = String::from_utf8(output.stderr).expect("probe stderr");
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "child probe must execute: {stdout}"
        );
        format!("{stdout}\n{stderr}")
    }

    #[test]
    fn startup_logging_child() {
        let Ok(scenario) = env::var("EDGEZERO_TEST_LOGGING_SCENARIO") else {
            return;
        };
        let result = match scenario.as_str() {
            "invalid-ingress" => {
                let result = run_app_with_preflight::<LoggingConfiguration, _>(|| {
                    panic!(
                        "invalid ingress must fail before preflight or application configuration"
                    )
                });
                assert_eq!(
                    result
                        .expect_err("invalid ingress must stop startup")
                        .to_string(),
                    "invalid Axum ingress setting `max_connections`"
                );
                return;
            }
            "installed" => {
                SimpleLogger::new()
                    .with_level(LevelFilter::Trace)
                    .init()
                    .expect("existing logger");
                let result = run_app_with_preflight::<LoggingConfiguration, _>(|| {
                    log::warn!(target: BOOT_LOG_TARGET, "unexpected-preflight-probe");
                    Ok(())
                });
                assert_eq!(
                    result
                        .expect_err("logger conflict must stop startup")
                        .to_string(),
                    "failed to initialize application logger"
                );
                return;
            }
            "owned" => {
                SimpleLogger::new()
                    .with_level(LevelFilter::Trace)
                    .init()
                    .expect("owned logger");
                log::set_max_level(LevelFilter::Debug);
                let result = run_app::<OwnedLoggingConfiguration>();
                assert_eq!(log::max_level(), LevelFilter::Debug);
                result
            }
            "preflight" => run_app_with_preflight::<LoggingConfiguration, _>(|| {
                log::warn!(target: edgezero_core::BOOT_LOG_TARGET, "preflight-warning-probe");
                anyhow::bail!("preflight stopped startup");
            }),
            _ => run_app::<LoggingConfiguration>(),
        };
        assert!(result.is_err(), "probe must stop before listener setup");
    }

    #[test]
    fn invalid_ingress_stops_before_preflight_configuration_and_binding() {
        let output = logging_probe("invalid-ingress", "info");
        assert!(!output.contains("boot-warning-probe"), "{output}");
    }

    #[test]
    fn logging_is_ready_before_configure_with_separate_boot_filter() {
        for level in ["off", "error", "trace"] {
            let output = logging_probe("managed", level);
            assert!(output.contains("boot-warning-probe"), "{output}");
            assert!(!output.contains("boot-info-probe"), "{output}");
            assert_eq!(
                output.contains("runtime-warning-probe"),
                level == "trace",
                "{output}"
            );
            assert_eq!(
                output.contains("runtime-debug-probe"),
                level == "trace",
                "{output}"
            );
        }
    }

    #[test]
    fn owned_logging_preserves_backend_and_facade_filter() {
        let output = logging_probe("owned", "off");
        assert!(output.contains("runtime-debug-probe"), "{output}");
        assert!(
            output.contains("boot-info-probe"),
            "application owns boot filtering: {output}"
        );
    }

    #[test]
    fn preflight_runs_after_logger_and_before_configuration() {
        let output = logging_probe("preflight", "off");
        assert!(output.contains("preflight-warning-probe"), "{output}");
        assert!(!output.contains("boot-warning-probe"), "{output}");
    }

    #[test]
    fn logger_conflict_prevents_preflight_and_configuration() {
        let output = logging_probe("installed", "trace");
        assert!(!output.contains("unexpected-preflight-probe"), "{output}");
        assert!(!output.contains("boot-warning-probe"), "{output}");
    }

    #[test]
    fn failing_configuration_prevents_listener_boundary() {
        let bind_calls = AtomicUsize::new(0);
        let result = build_app_for_dispatch::<FailingConfiguration>().map(|_app| {
            bind_calls.fetch_add(1, Ordering::SeqCst);
        });

        let error = result.expect_err("configuration must fail before listener bind");
        assert_eq!(error.to_string(), "application configuration failed");
        assert_eq!(bind_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn application_configuration_receives_axum_platform_metadata() {
        let app = build_app_for_dispatch::<PlatformAwareConfiguration>().expect("configured app");

        assert_eq!(app.platform(), crate::AXUM_PLATFORM);
    }

    #[test]
    fn default_config_uses_expected_address() {
        let config = AxumDevServerConfig::default();
        assert_eq!(config.addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(config.addr.port(), 8787);
    }

    #[test]
    fn default_config_enables_ctrl_c() {
        let config = AxumDevServerConfig::default();
        assert!(config.enable_ctrl_c);
    }

    #[test]
    fn config_can_be_cloned() {
        let config = AxumDevServerConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.addr, config.addr);
        assert_eq!(cloned.enable_ctrl_c, config.enable_ctrl_c);
    }

    #[test]
    fn config_with_custom_address() {
        let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
        let config = AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
            ingress: AxumIngressConfig::default(),
        };
        assert_eq!(config.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(config.addr.port(), 3000);
        assert!(!config.enable_ctrl_c);
    }

    #[test]
    fn dev_server_new_uses_default_config() {
        use edgezero_core::router::RouterService;

        let router = RouterService::builder().build();
        let server = AxumDevServer::new(router);
        assert_eq!(server.config.addr.port(), 8787);
        assert!(server.config.enable_ctrl_c);
    }

    #[test]
    fn dev_server_with_config_uses_custom_config() {
        use edgezero_core::router::RouterService;

        let router = RouterService::builder().build();
        let config = AxumDevServerConfig {
            addr: SocketAddr::from(([127, 0, 0, 1], 9000)),
            enable_ctrl_c: false,
            ingress: AxumIngressConfig::default(),
        };
        let server = AxumDevServer::with_config(router, config);
        assert_eq!(server.config.addr.port(), 9000);
        assert!(!server.config.enable_ctrl_c);
    }

    #[test]
    fn every_store_name_gets_a_slug_based_path() {
        // The pre-rewrite shortcut hard-coded `.edgezero/kv.redb`
        // when the store name equalled the legacy `EDGEZERO_KV`
        // constant. Hard cutoff: now every name -- including any
        // historical value an operator might still set -- flows
        // through the slug+hash encoder, so no name gets a
        // special shortcut path.
        let legacy = kv_store_path("EDGEZERO_KV");
        assert_ne!(
            legacy,
            PathBuf::from(".edgezero/kv.redb"),
            "post-cutoff: the legacy default name no longer gets the bare `kv.redb` shortcut: {legacy:?}"
        );
        assert!(
            legacy.to_string_lossy().starts_with(".edgezero/kv-"),
            "legacy name still gets a slug-based path: {legacy:?}"
        );
        let custom = kv_store_path("sessions");
        assert!(
            custom.to_string_lossy().contains("sessions"),
            "regular name gets a slug-based filename: {custom:?}"
        );
        assert_ne!(legacy, custom);
    }

    #[test]
    fn implicit_default_kv_is_optional() {
        assert_eq!(
            kv_init_requirement(StoresMetadata::default()),
            KvInitRequirement::Optional
        );
    }

    #[test]
    fn explicit_kv_config_is_required() {
        use edgezero_core::app::StoreMetadata;

        let stores = StoresMetadata {
            kv: Some(StoreMetadata {
                default: "edgezero_kv",
                ids: &["edgezero_kv"],
            }),
            ..StoresMetadata::default()
        };
        assert_eq!(kv_init_requirement(stores), KvInitRequirement::Required);
    }

    #[test]
    fn custom_store_name_uses_stable_bounded_path() {
        let path = kv_store_path("../Prod KV");
        let expected = format!(
            "kv-prod-kv-{:016x}.redb",
            stable_store_name_hash("../Prod KV")
        );
        assert_eq!(path.parent(), Some(Path::new(".edgezero")));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(expected.as_str())
        );
    }

    #[test]
    fn custom_store_names_remain_distinct_across_case() {
        assert_ne!(kv_store_path("Store"), kv_store_path("store"));
    }

    #[test]
    fn custom_store_path_length_is_bounded() {
        let path = kv_store_path(&"a".repeat(4_096));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("file name");
        assert!(
            file_name.len() <= 64,
            "unexpected file name length: {file_name}"
        );
    }

    #[test]
    fn resolve_addr_defaults_without_env_config() {
        let empty: [(&str, &str); 0] = [];
        let resolution = resolve_addr(&EnvConfig::from_vars(empty));
        assert_eq!(resolution.addr, SocketAddr::from(([127, 0, 0, 1], 8787)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_addr_reads_env_host_and_port() {
        let env = EnvConfig::from_vars([
            ("EDGEZERO__ADAPTER__HOST", "0.0.0.0"),
            ("EDGEZERO__ADAPTER__PORT", "3000"),
        ]);
        let resolution = resolve_addr(&env);
        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 3000)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_addr_partial_env_override() {
        let env = EnvConfig::from_vars([("EDGEZERO__ADAPTER__HOST", "0.0.0.0")]);
        let resolution = resolve_addr(&env);
        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 8787)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_addr_invalid_env_falls_back_to_default() {
        let env = EnvConfig::from_vars([
            ("EDGEZERO__ADAPTER__HOST", "not-an-ip"),
            ("EDGEZERO__ADAPTER__PORT", "abc"),
        ]);
        let resolution = resolve_addr(&env);
        assert_eq!(resolution.addr, SocketAddr::from(([127, 0, 0, 1], 8787)));
        assert_eq!(resolution.warnings.len(), 2);
    }

    /// `build_config_registry` must pack `default_key` from
    /// `EDGEZERO__STORES__CONFIG__<ID>__KEY` (12.7).
    /// A missing local-config file yields an empty store, so the
    /// binding is still created — this test exercises the key-override
    /// path without requiring the file to exist.
    #[test]
    fn build_config_registry_packs_resolved_default_key_from_env() {
        use edgezero_core::app::StoreMetadata;
        let env = EnvConfig::from_vars([(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
            "app_config_staging",
        )]);
        let meta = StoreMetadata {
            default: "app_config",
            ids: &["app_config"],
        };
        let registry = build_config_registry(Some(meta), &env, ConfigStoreLimits::default())
            .expect("load")
            .expect("registry built");
        let binding = registry.named("app_config").expect("binding registered");
        assert_eq!(binding.default_key, "app_config_staging");
    }

    #[test]
    fn build_config_registry_falls_back_to_id_when_env_key_unset() {
        use edgezero_core::app::StoreMetadata;
        let env = EnvConfig::from_vars(iter::empty::<(&str, &str)>());
        let meta = StoreMetadata {
            default: "app_config",
            ids: &["app_config"],
        };
        let registry = build_config_registry(Some(meta), &env, ConfigStoreLimits::default())
            .expect("load")
            .expect("registry built");
        let binding = registry.named("app_config").expect("binding registered");
        assert_eq!(binding.default_key, "app_config");
    }

    #[test]
    fn malformed_declared_snapshot_is_not_an_absent_binding() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.json");
        fs::write(&path, "credential=private invalid JSON").expect("fixture");
        let meta = StoreMetadata {
            default: "config",
            ids: &["config"],
        };
        let registry = build_config_registry_at(
            Some(meta),
            &EnvConfig::default(),
            ConfigStoreLimits::default(),
            |_id| path.clone(),
        )
        .expect("failed binding")
        .expect("registry");
        let binding = registry.named("config").expect("declared binding survives");
        let error = block_on(binding.handle.get("key")).expect_err("unavailable, not absent");
        assert!(matches!(error, ConfigStoreError::Unavailable { .. }));
        assert!(!error.to_string().contains("credential=private"));
    }

    #[test]
    fn aggregate_snapshot_overflow_fails_startup_and_retry_recovers() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.json");
        fs::write(&path, serde_json::json!({"k": "v"}).to_string()).expect("fixture");
        let limits = ConfigStoreLimits::new(128, 1, 1, 1, 4096, 8192).expect("limits");
        let charge = AxumConfigStore::from_path(&path, limits)
            .expect("snapshot")
            .resident_allocation_bytes();
        let capped = ConfigStoreLimits::new(
            128,
            1,
            1,
            1,
            charge.saturating_mul(2).saturating_sub(1),
            8192,
        )
        .expect("limits");
        let meta = StoreMetadata {
            default: "first",
            ids: &["first", "second"],
        };
        assert!(matches!(
            build_config_registry_at(Some(meta), &EnvConfig::default(), capped, |_id| path
                .clone()),
            Err(ConfigStoreError::ValueTooLarge)
        ));
        let registry = build_config_registry_at(Some(meta), &EnvConfig::default(), limits, |_id| {
            path.clone()
        })
        .expect("retry")
        .expect("registry");
        assert!(registry.named("first").is_some());
        assert!(registry.named("second").is_some());
    }

    #[test]
    fn invalid_config_registry_metadata_fails_without_opening_stores() {
        for meta in [
            StoreMetadata {
                default: "first",
                ids: &[],
            },
            StoreMetadata {
                default: "absent",
                ids: &["first"],
            },
            StoreMetadata {
                default: "first",
                ids: &["first", "first"],
            },
            StoreMetadata {
                default: "bad/id",
                ids: &["bad/id"],
            },
        ] {
            let error = build_config_registry_at(
                Some(meta),
                &EnvConfig::default(),
                ConfigStoreLimits::default(),
                |_id| panic!("must not open invalid metadata"),
            )
            .expect_err("invalid metadata");
            assert!(matches!(error, ConfigStoreError::Internal { .. }));
        }
    }
}

#[cfg(test)]
mod bounded_ingress_tests {
    use std::future::pending;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::response_builder;
    use edgezero_core::ingress::{AdmissionDecision, IngressGrant};
    use edgezero_core::response_egress::{ResponseEgressCompletion, ResponseEgressOutcome};
    use edgezero_core::router::RouterService;
    use edgezero_core::{Deadline, MonotonicClock, MonotonicInstant, ResponseEgressPolicy};
    use futures_util::stream::poll_fn;
    use std::task::Poll;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::{JoinHandle, yield_now};
    use tokio::time::{Instant, sleep, timeout};

    use super::{AxumDevServer, AxumDevServerConfig};
    use crate::ingress_config::AxumIngressConfig;

    struct Server {
        address: SocketAddr,
        calls: Arc<AtomicUsize>,
        panics: Arc<AtomicUsize>,
        task: JoinHandle<anyhow::Result<()>>,
    }

    struct DropProbe(Arc<AtomicUsize>);

    #[derive(Clone, Default)]
    struct EgressProbes {
        completions: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
        resources: Arc<AtomicUsize>,
        sources: Arc<AtomicUsize>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn closed(stream: &mut TcpStream) {
        let mut byte = [0];
        let read = timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .expect("connection must close");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "refusal must not send a response"
        );
    }

    async fn healthy(stream: &mut TcpStream) {
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let read = async {
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
                assert!(head.len() < 4096);
            }
            assert!(head.starts_with(b"HTTP/1.1 200"));
            let mut body = [0; 2];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"ok");
        };
        timeout(Duration::from_secs(2), read).await.unwrap();
    }

    fn stalled_egress_app(
        ready: bool,
        expected: ResponseEgressOutcome,
        probes: &EgressProbes,
    ) -> App {
        let sources = Arc::clone(&probes.sources);
        let polls = Arc::clone(&probes.polls);
        let chunk = Bytes::from(vec![b'a'; 0x0004_0000]);
        let router = RouterService::builder()
            .get("/", |_| async { Ok::<_, EdgeError>("ok") })
            .get("/slow", move |_| {
                let source = DropProbe(Arc::clone(&sources));
                let observed_polls = Arc::clone(&polls);
                let payload = chunk.clone();
                async move {
                    let body = poll_fn(move |_cx| {
                        let _keep_alive = &source;
                        let polled = observed_polls.fetch_add(1, Ordering::SeqCst);
                        if ready || polled == 0 {
                            Poll::Ready(Some(Ok(payload.clone())))
                        } else {
                            Poll::Pending
                        }
                    });
                    Ok::<_, EdgeError>(
                        response_builder()
                            .body(Body::from_stream(body))
                            .expect("response"),
                    )
                }
            })
            .build();
        let mut app = App::new(router);
        let frozen = MonotonicInstant::now();
        app.set_monotonic_clock(MonotonicClock::new(move || frozen));
        let budget = if expected == ResponseEgressOutcome::TransportError {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        app.set_response_egress_policy(move |_, started| ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(started.checked_add(budget).expect("deadline")),
        });
        let completions = Arc::clone(&probes.completions);
        let resources = Arc::clone(&probes.resources);
        app.set_ingress_admission_policy(move |head| {
            let completion = if head.target().path() == "/slow" {
                ResponseEgressCompletion::new({
                    let callback = Arc::clone(&completions);
                    let resource = DropProbe(Arc::clone(&resources));
                    move |report| {
                        assert_eq!(report.outcome, expected);
                        callback.fetch_add(1, Ordering::SeqCst);
                        drop(resource);
                    }
                })
            } else {
                ResponseEgressCompletion::empty()
            };
            AdmissionDecision::Admit {
                completion,
                grant: IngressGrant::empty(),
                read_deadline: head.read_deadline_after(Duration::from_secs(1)),
            }
        });
        app
    }

    #[tokio::test]
    async fn stalled_egress_termination_releases_capacity_and_recovers() {
        for (ready, disconnect) in [(false, true), (false, false), (true, false)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
            let address = listener.local_addr().expect("address");
            let probes = EgressProbes::default();
            let expected = if disconnect {
                ResponseEgressOutcome::TransportError
            } else {
                ResponseEgressOutcome::DeadlineExceeded
            };
            let app = stalled_egress_app(ready, expected, &probes);
            let task = tokio::spawn(super::serve_with_stores(
                app,
                listener,
                false,
                AxumIngressConfig::new(1, 8192, 100, Duration::from_secs(5)).expect("ingress"),
                super::Stores::default(),
            ));
            let mut client = TcpStream::connect(address).await.expect("client");
            client
                .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .expect("request");
            timeout(Duration::from_secs(2), async {
                while probes.polls.load(Ordering::SeqCst) < 2 {
                    yield_now().await;
                }
            })
            .await
            .expect("producer is live");
            if disconnect {
                drop(client);
            }
            timeout(Duration::from_secs(2), async {
                while probes.completions.load(Ordering::SeqCst) != 1 {
                    yield_now().await;
                }
            })
            .await
            .expect("terminal callback");
            assert_eq!(probes.resources.load(Ordering::SeqCst), 1);
            assert_eq!(probes.sources.load(Ordering::SeqCst), 1);
            let stopped = probes.polls.load(Ordering::SeqCst);
            timeout(Duration::from_secs(2), async {
                loop {
                    let mut recovered = TcpStream::connect(address)
                        .await
                        .expect("recovery connection");
                    if recovered
                        .write_all(
                            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .is_ok()
                    {
                        let mut wire = Vec::new();
                        if recovered.read_to_end(&mut wire).await.is_ok() && wire.ends_with(b"ok") {
                            break;
                        }
                    }
                    yield_now().await;
                }
            })
            .await
            .expect("capacity recovery");
            assert_eq!(
                probes.polls.load(Ordering::SeqCst),
                stopped,
                "terminated sources must stop"
            );
            assert_eq!(probes.completions.load(Ordering::SeqCst), 1);
            task.abort();
            let _shutdown = task.await;
        }
    }

    async fn start(connections: usize, milliseconds: u64, count: usize) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let panics = Arc::new(AtomicUsize::new(0));
        let observed_panics = Arc::clone(&panics);
        let router = RouterService::builder()
            .get("/panic", move |_| {
                let counted = Arc::clone(&observed_panics);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    panic!("isolated connection failure");
                    #[expect(
                        unreachable_code,
                        reason = "sets the handler response type after the intentional panic"
                    )]
                    Ok::<_, EdgeError>("unreachable")
                }
            })
            .get("/", move |_| {
                let counted = Arc::clone(&observed);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, EdgeError>("ok")
                }
            })
            .build();
        let server = AxumDevServer::with_config(
            router,
            AxumDevServerConfig {
                addr: address,
                enable_ctrl_c: false,
                ingress: AxumIngressConfig::new(
                    connections,
                    8192,
                    count,
                    Duration::from_millis(milliseconds),
                )
                .unwrap(),
            },
        );
        let task = tokio::spawn(server.run_with_listener(listener));
        Server {
            address,
            calls,
            panics,
            task,
        }
    }

    #[tokio::test]
    async fn parser_and_unwinding_failures_release_capacity_and_leave_server_healthy() {
        let server = start(1, 10_000, 100).await;
        for request in [
            &b"invalid\r\n\r\n"[..],
            &b"GET /panic HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
        ] {
            let mut failed = TcpStream::connect(server.address).await.unwrap();
            failed.write_all(request).await.unwrap();
            let mut response = Vec::new();
            let _read = timeout(Duration::from_secs(2), failed.read_to_end(&mut response))
                .await
                .unwrap();
            assert!(!response.starts_with(b"HTTP/1.1 200"));
            let mut recovered = TcpStream::connect(server.address).await.unwrap();
            healthy(&mut recovered).await;
            drop(recovered);
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
        assert_eq!(server.panics.load(Ordering::SeqCst), 1);
        assert!(!server.task.is_finished());
    }

    #[tokio::test]
    async fn shutdown_releases_pending_handler_stream_and_completion_resources() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let resources = Arc::new(AtomicUsize::new(0));
        let terminals = Arc::new(AtomicUsize::new(0));
        let handler_started = Arc::clone(&started);
        let handler_released = Arc::clone(&released);
        let stream_started = Arc::clone(&started);
        let stream_released = Arc::clone(&released);
        let router = RouterService::builder()
            .get("/handler", move |_| {
                let entered = Arc::clone(&handler_started);
                let guard = DropProbe(Arc::clone(&handler_released));
                async move {
                    entered.fetch_add(1, Ordering::SeqCst);
                    pending::<()>().await;
                    drop(guard);
                    Ok::<_, EdgeError>("unreachable")
                }
            })
            .get("/stream", move |_| {
                let entered = Arc::clone(&stream_started);
                let guard = DropProbe(Arc::clone(&stream_released));
                async move {
                    let body = poll_fn(move |_cx| {
                        let _keep_alive = &guard;
                        entered.store(2, Ordering::SeqCst);
                        Poll::<Option<Result<Bytes, EdgeError>>>::Pending
                    });
                    Ok::<_, EdgeError>(response_builder().body(Body::from_stream(body)).unwrap())
                }
            })
            .build();
        let mut app = App::new(router);
        let completion_resources = Arc::clone(&resources);
        let completion_terminals = Arc::clone(&terminals);
        app.set_ingress_admission_policy(move |head| AdmissionDecision::Admit {
            completion: ResponseEgressCompletion::new({
                let resource = DropProbe(Arc::clone(&completion_resources));
                let observed = Arc::clone(&completion_terminals);
                move |report| {
                    assert_eq!(report.outcome, ResponseEgressOutcome::TransportError);
                    observed.fetch_add(1, Ordering::SeqCst);
                    drop(resource);
                }
            }),
            grant: IngressGrant::empty(),
            read_deadline: head.read_deadline_after(Duration::from_secs(10)),
        });
        let task = tokio::spawn(super::serve_with_stores(
            app,
            listener,
            false,
            AxumIngressConfig::new(2, 8192, 100, Duration::from_secs(10)).unwrap(),
            super::Stores::default(),
        ));
        let mut handler = TcpStream::connect(address).await.unwrap();
        handler
            .write_all(b"GET /handler HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) != 1 {
                yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut head = Vec::new();
        timeout(Duration::from_secs(2), async {
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
                assert!(head.len() < 4096);
            }
        })
        .await
        .unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"));
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert_eq!(resources.load(Ordering::SeqCst), 0);
        task.abort();
        closed(&mut handler).await;
        closed(&mut stream).await;
        timeout(Duration::from_secs(2), async {
            while released.load(Ordering::SeqCst) != 2 || resources.load(Ordering::SeqCst) != 2 {
                yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(terminals.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn saturation_refuses_without_queueing_and_disconnect_recovers() {
        let server = start(2, 10_000, 100).await;
        let mut first = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut first).await;
        let mut second = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut second).await;
        for _ in 0_usize..64 {
            let mut refused = TcpStream::connect(server.address).await.unwrap();
            closed(&mut refused).await;
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
        drop(first);
        // Wait for disconnect processing without depending on an arbitrary sleep.
        let recover = async {
            loop {
                let mut stream = TcpStream::connect(server.address).await.unwrap();
                let written = stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await;
                let mut response = Vec::new();
                let read = stream.read_to_end(&mut response).await;
                if written.is_ok() && read.is_ok() && response.starts_with(b"HTTP/1.1 200") {
                    break;
                }
                yield_now().await;
            }
        };
        timeout(Duration::from_secs(2), recover).await.unwrap();
        healthy(&mut second).await;
        assert_eq!(server.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn silence_and_trickling_initial_and_keepalive_heads_expire() {
        let server = start(1, 150, 100).await;
        let mut silent = TcpStream::connect(server.address).await.unwrap();
        closed(&mut silent).await;
        for keepalive in [false, true] {
            let mut stream = TcpStream::connect(server.address).await.unwrap();
            if keepalive {
                healthy(&mut stream).await;
            }
            stream
                .write_all(b"GET / HTTP/1.1\r\nX-Trickle: ")
                .await
                .unwrap();
            let started = Instant::now();
            let trickle = async {
                loop {
                    sleep(Duration::from_millis(10)).await;
                    if stream.write_all(b"a").await.is_err() {
                        break;
                    }
                    let mut byte = [0];
                    if let Ok(read) =
                        timeout(Duration::from_millis(1), stream.peek(&mut byte)).await
                        && !matches!(read, Ok(1..))
                    {
                        break;
                    }
                }
            };
            timeout(Duration::from_secs(2), trickle)
                .await
                .expect("trickle must not extend deadline");
            assert!(started.elapsed() < Duration::from_secs(2));
            closed(&mut stream).await;
        }
        let mut recovered = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut recovered).await;
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn idle_keepalive_timeout_releases_capacity() {
        let server = start(1, 100, 100).await;
        let mut idle = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut idle).await;
        closed(&mut idle).await;
        let mut recovered = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut recovered).await;
    }

    #[tokio::test]
    async fn raw_count_duplicates_and_byte_boundaries_precede_dispatch() {
        let server = start(1, 10_000, 4).await;
        let prefix = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Pad: ";
        let mut exact = prefix.to_vec();
        exact.extend(vec![b'a'; 8192 - prefix.len() - 4]);
        exact.extend_from_slice(b"\r\n\r\n");
        let mut over = exact.clone();
        over.insert(prefix.len(), b'a');
        let count_exact = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Dup: a\r\nX-Dup: b\r\n\r\n".to_vec();
        let count_over = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Dup: a\r\nX-Dup: b\r\nX-Dup: c\r\n\r\n".to_vec();
        let mut long_line = b"GET /".to_vec();
        long_line.extend(vec![b'a'; 8192]);
        long_line.extend_from_slice(b" HTTP/1.1\r\nHost: localhost\r\n\r\n");
        for (request, expected) in [
            (exact, b"HTTP/1.1 200"),
            (over, b"HTTP/1.1 431"),
            (count_exact, b"HTTP/1.1 200"),
            (count_over, b"HTTP/1.1 431"),
            (long_line, b"HTTP/1.1 431"),
        ] {
            let mut stream = TcpStream::connect(server.address).await.unwrap();
            stream.write_all(&request).await.unwrap();
            let mut response = Vec::new();
            let _read = timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
                .await
                .unwrap();
            assert!(response.starts_with(expected), "unexpected response status");
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
        let mut recovered = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut recovered).await;
    }

    #[tokio::test]
    async fn cancellation_closes_idle_and_partial_connections() {
        let server = start(2, 10_000, 100).await;
        let mut idle = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut idle).await;
        let mut partial = TcpStream::connect(server.address).await.unwrap();
        healthy(&mut partial).await;
        partial
            .write_all(b"GET / HTTP/1.1\r\nX-Pending:")
            .await
            .unwrap();
        server.task.abort();
        closed(&mut idle).await;
        closed(&mut partial).await;
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use edgezero_core::action;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::extractor::Secrets;
    use edgezero_core::router::RouterService;
    use edgezero_core::secret_store::SecretHandle as CoreSecretHandle;
    use reqwest::header::ALLOW;
    use std::time::{Duration, Instant};
    use tokio::task::{JoinHandle, spawn_blocking};
    use tokio::time::sleep;

    struct TestServer {
        _temp_dir: tempfile::TempDir,
        base_url: String,
        handle: JoinHandle<()>,
    }

    struct TestServerWithStore {
        base_url: String,
        handle: JoinHandle<()>,
    }

    async fn start_test_server(router: RouterService) -> TestServer {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let config = AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
            ingress: AxumIngressConfig::default(),
        };
        // Use a unique temp directory for each test server
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let kv_path = temp_dir.path().join("kv.redb");
        let kv_handle = kv_handle_from_path(&kv_path).expect("create kv store");
        let server = AxumDevServer::with_config(router, config).with_kv_handle(kv_handle);

        let handle = tokio::spawn(async move {
            let _result = server.run_with_listener(listener).await;
        });

        TestServer {
            base_url: format!("http://{addr}"),
            handle,
            _temp_dir: temp_dir,
        }
    }

    async fn send_with_retry<F>(client: &reqwest::Client, mut make_request: F) -> reqwest::Response
    where
        F: FnMut(&reqwest::Client) -> reqwest::RequestBuilder,
    {
        let start = Instant::now();
        let timeout = Duration::from_secs(2);

        loop {
            match make_request(client).send().await {
                Ok(response) => return response,
                Err(err) => {
                    assert!(
                        start.elapsed() < timeout,
                        "server did not respond before timeout: {err}"
                    );
                }
            }

            sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_responds_to_requests() {
        async fn handler(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            Ok("hello from dev server")
        }

        let router = RouterService::builder().get("/test", handler).build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();
        let url = format!("{}/test", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "hello from dev server");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_returns_404_for_unknown_routes() {
        let router = RouterService::builder().build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();
        let url = format!("{}/nonexistent", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_returns_method_not_allowed() {
        async fn handler(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            Ok("ok")
        }

        let router = RouterService::builder().post("/submit", handler).build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();
        let url = format!("{}/submit", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers().get(ALLOW).expect("Allow header"), "POST");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_forwards_headers() {
        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let value = ctx
                .headers()
                .get("x-custom")
                .and_then(|val| val.to_str().ok())
                .unwrap_or("missing");
            Ok(value.to_owned())
        }

        let router = RouterService::builder().get("/headers", handler).build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();
        let url = format!("{}/headers", server.base_url);
        let response = send_with_retry(&client, |http_client| {
            http_client.get(url.as_str()).header("x-custom", "my-value")
        })
        .await;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "my-value");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_fails_to_bind_to_used_port() {
        // First bind to a port
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind first");
        let addr = listener.local_addr().expect("listener addr");

        // Try to start server on same port
        let router = RouterService::builder().build();
        let config = AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
            ingress: AxumIngressConfig::default(),
        };
        let server = AxumDevServer::with_config(router, config);

        // Run in blocking mode to capture the error
        let result = spawn_blocking(move || server.run()).await;

        match result {
            Ok(Err(err)) => {
                let err_str = err.to_string();
                assert!(
                    err_str.contains("bind") || err_str.contains("address"),
                    "expected bind error, got: {err_str}"
                );
            }
            _ => panic!("expected bind error"),
        }

        drop(listener);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_persists_across_requests() {
        async fn write_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let store = ctx.kv_store_default().expect("kv configured");
            store.put("counter", &42_i32).await?;
            Ok("written")
        }

        async fn read_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let store = ctx.kv_store_default().expect("kv configured");
            let val: i32 = store.get_or("counter", 0_i32).await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder()
            .post("/write", write_handler)
            .get("/read", read_handler)
            .build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();

        // Write a value
        let write_url = format!("{}/write", server.base_url);
        let write_response =
            send_with_retry(&client, |http_client| http_client.post(write_url.as_str())).await;
        assert_eq!(write_response.status(), reqwest::StatusCode::OK);
        assert_eq!(write_response.text().await.unwrap(), "written");

        // Read it back — proves shared state across requests
        let read_url = format!("{}/read", server.base_url);
        let read_response =
            send_with_retry(&client, |http_client| http_client.get(read_url.as_str())).await;
        assert_eq!(read_response.status(), reqwest::StatusCode::OK);
        assert_eq!(read_response.text().await.unwrap(), "42");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_delete_across_requests() {
        async fn write_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            kv.put("temp", &"to_delete").await?;
            Ok("written")
        }

        async fn delete_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            kv.delete("temp").await?;
            Ok("deleted")
        }

        async fn check_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            let exists = kv.exists("temp").await?;
            Ok(format!("exists={exists}"))
        }

        let router = RouterService::builder()
            .post("/write", write_handler)
            .post("/delete", delete_handler)
            .get("/check", check_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();

        // Write
        let write_url = format!("{}/write", server.base_url);
        send_with_retry(&client, |http_client| http_client.post(write_url.as_str())).await;

        // Verify exists
        let check_url = format!("{}/check", server.base_url);
        let exists_before =
            send_with_retry(&client, |http_client| http_client.get(check_url.as_str())).await;
        assert_eq!(exists_before.text().await.unwrap(), "exists=true");

        // Delete
        let delete_url = format!("{}/delete", server.base_url);
        send_with_retry(&client, |http_client| http_client.post(delete_url.as_str())).await;

        // Verify gone
        let exists_after =
            send_with_retry(&client, |http_client| http_client.get(check_url.as_str())).await;
        assert_eq!(exists_after.text().await.unwrap(), "exists=false");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_update_across_requests() {
        async fn increment_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            let val = kv
                .read_modify_write("counter", 0_i32, |n| n + 1_i32)
                .await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder()
            .post("/inc", increment_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();
        let url = format!("{}/inc", server.base_url);

        // Increment 5 times, each should return incremented value
        for expected in 1_i32..=5_i32 {
            let resp = send_with_retry(&client, |http_client| http_client.post(url.as_str())).await;
            assert_eq!(
                resp.text().await.unwrap(),
                expected.to_string(),
                "increment #{expected}"
            );
        }

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_returns_not_found_gracefully() {
        async fn read_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            let val: i32 = kv.get_or("nonexistent", -1_i32).await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder().get("/read", read_handler).build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();

        let url = format!("{}/read", server.base_url);
        let resp = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "-1");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_handles_typed_data() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct UserProfile {
            active: bool,
            age: u32,
            name: String,
        }

        async fn write_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            let profile = UserProfile {
                name: "Alice".to_owned(),
                age: 30,
                active: true,
            };
            kv.put("user:alice", &profile).await?;
            Ok("saved")
        }

        async fn read_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_store_default().expect("kv configured");
            let profile: Option<UserProfile> = kv.get("user:alice").await?;
            match profile {
                Some(found) => Ok(format!("{}:{}", found.name, found.age)),
                None => Ok("not found".to_owned()),
            }
        }

        let router = RouterService::builder()
            .post("/save", write_handler)
            .get("/load", read_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();

        // Save profile
        let save_url = format!("{}/save", server.base_url);
        let save_resp =
            send_with_retry(&client, |http_client| http_client.post(save_url.as_str())).await;
        assert_eq!(save_resp.text().await.unwrap(), "saved");

        // Load profile
        let load_url = format!("{}/load", server.base_url);
        let load_resp =
            send_with_retry(&client, |http_client| http_client.get(load_url.as_str())).await;
        assert_eq!(load_resp.text().await.unwrap(), "Alice:30");

        server.handle.abort();
    }

    // -----------------------------------------------------------------------
    // Secret store helpers
    // -----------------------------------------------------------------------

    async fn start_test_server_with_store_handle(
        router: RouterService,
        secret_handle: Option<CoreSecretHandle>,
    ) -> TestServerWithStore {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind secrets test server");
        let addr = listener.local_addr().expect("local addr");
        let config = super::AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
            ingress: AxumIngressConfig::default(),
        };
        let mut server = super::AxumDevServer::with_config(router, config);
        if let Some(handle) = secret_handle {
            server = server.with_secret_handle(handle);
        }
        let handle = tokio::spawn(async move {
            let _result = server.run_with_listener(listener).await;
        });
        TestServerWithStore {
            base_url: format!("http://{addr}"),
            handle,
        }
    }

    #[action]
    async fn secret_value_handler(secrets: Secrets) -> Result<String, EdgeError> {
        let store = secrets
            .default()
            .ok_or_else(|| EdgeError::service_unavailable("no default secret store registered"))?;
        store.require_str("API_KEY").await.map_err(EdgeError::from)
    }

    // -----------------------------------------------------------------------
    // Secret store integration tests
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn secret_present_returns_value() {
        use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
        use std::sync::Arc;

        let router = RouterService::builder()
            .get("/secret", secret_value_handler)
            .build();
        // The legacy single-handle wiring binds under `"default"` (see
        // `Secrets::from_request` fallback), so the in-memory store is
        // keyed under that prefix.
        let store = InMemorySecretStore::new([("default/API_KEY", bytes::Bytes::from("s3cr3t"))]);
        let handle = SecretHandle::new(Arc::new(store));
        let server = start_test_server_with_store_handle(router, Some(handle)).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "s3cr3t");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secret_missing_returns_500() {
        use edgezero_core::secret_store::{InMemorySecretStore, SecretHandle};
        use std::sync::Arc;

        let router = RouterService::builder()
            .get("/secret", secret_value_handler)
            .build();
        let store = InMemorySecretStore::new(iter::empty::<(&str, bytes::Bytes)>());
        let handle = SecretHandle::new(Arc::new(store));
        let server = start_test_server_with_store_handle(router, Some(handle)).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = response.text().await.unwrap();
        assert!(!body.contains("API_KEY"));
        assert!(body.contains("internal server error"));
        assert!(!body.contains("required secret is not configured"));

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_secret_store_configured_returns_500() {
        let router = RouterService::builder()
            .get("/secret", secret_value_handler)
            .build();
        let server = start_test_server_with_store_handle(router, None).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |http_client| http_client.get(url.as_str())).await;

        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = response.text().await.unwrap();
        assert!(body.contains("internal server error"));
        assert!(!body.contains(
            "no secret store configured -- check [stores.secrets] in edgezero.toml and platform bindings"
        ));

        server.handle.abort();
    }
}
