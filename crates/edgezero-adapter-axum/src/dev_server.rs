#![expect(
    clippy::pub_use,
    reason = "validated options retain the existing dev_server API location"
)]
#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "compatibility entrypoints, normalization, native ownership and startup policy remain grouped by responsibility"
)]

use std::env::vars_os;
use std::ffi::OsString;
use std::fs;
use std::future;
use std::future::Future;
#[cfg(test)]
use std::iter;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr as _;
use std::sync::Arc;

use crate::diagnostics::{NativeDiagnosticsHandle, NativeSession};
use crate::proxy::TrustedProxyPolicy;
pub use crate::run_options::AxumRunOptions;
use crate::run_options::{JsonBodyLimit, failure, native_settings, shutdown_grace};
use anyhow::Context as _;
use edgezero_core::probe::LifecyclePhase;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
#[cfg(test)]
use tokio::sync::oneshot::{Receiver as ShutdownReceiver, Sender as ShutdownSender, channel};
use tokio::sync::watch;
#[cfg(test)]
use tokio::task::spawn_blocking;
use tokio::task::{JoinSet, LocalSet};

use edgezero_core::addr;
use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry, StoreRegistry,
};
use log::{Level, LevelFilter};
use simple_logger::SimpleLogger;

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::process;
use tokio::task::yield_now;
use tokio::time::{Instant as NativeInstant, sleep_until};

use crate::config_store::AxumConfigStore;
use crate::connection::serve_http1_with_shutdown;
use crate::key_value_store::PersistentKvStore;
use crate::outbound::AxumOutboundClient;
use crate::response::EgressConnection;
use crate::secret_store::EnvSecretStore;
use crate::service::AxumServiceState;

/// Local application-owned startup work, borrowing the unshared App and prepared stores.
pub type LocalStartupFuture<'startup> =
    Pin<Box<dyn Future<Output = Result<(), edgezero_core::EdgeError>> + 'startup>>;

/// The exact registries prepared for requests. Handles may be cloned by the application;
/// retained KV clones must be released by their owners before reopening or backing up.
#[derive(Default)]
pub struct PreparedStores {
    config: Option<ConfigRegistry>,
    kv: Option<KvRegistry>,
    secrets: Option<SecretRegistry>,
}

impl PreparedStores {
    #[must_use]
    #[inline]
    pub fn config(&self) -> Option<&ConfigRegistry> {
        self.config.as_ref()
    }

    #[must_use]
    #[inline]
    pub fn kv(&self) -> Option<&KvRegistry> {
        self.kv.as_ref()
    }

    #[must_use]
    #[inline]
    pub fn secrets(&self) -> Option<&SecretRegistry> {
        self.secrets.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KvInitRequirement {
    Optional,
    Required,
}

/// Configuration used when running the dev server embedding `EdgeZero` into Axum.
#[derive(Clone)]
pub struct AxumDevServerConfig {
    pub addr: SocketAddr,
    pub enable_ctrl_c: bool,
}

impl Default for AxumDevServerConfig {
    #[inline]
    fn default() -> Self {
        Self {
            addr: SocketAddr::from((addr::DEFAULT_HOST, addr::DEFAULT_PORT)),
            enable_ctrl_c: true,
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
    json_body_limit: JsonBodyLimit,
    native: NativeHosting,
    router: RouterService,
    stores: Stores,
}

impl AxumDevServer {
    #[must_use]
    #[inline]
    pub fn new(router: RouterService) -> Self {
        Self {
            config: AxumDevServerConfig::default(),
            json_body_limit: JsonBodyLimit::DEFAULT,
            native: NativeHosting::default(),
            router,
            stores: Stores::default(),
        }
    }

    /// # Errors
    /// Returns an error if the dev server fails to bind, the Tokio runtime fails to start, or the underlying request loop returns an error.
    #[inline]
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            router,
            config,
            stores,
            json_body_limit,
            native,
        } = self;
        run_local(
            ListenerSource::Bind(config.addr),
            config.enable_ctrl_c,
            Duration::from_secs(10),
            future::pending::<()>(),
            async move { prepare_transport(App::new(router), stores.into(), json_body_limit, native) },
        )
    }

    /// Uses an explicit incoming-proxy policy, ignoring ambient proxy settings.
    #[must_use]
    #[inline]
    pub fn with_trusted_proxy_policy(mut self, policy: TrustedProxyPolicy) -> Self {
        self.native.policy = policy;
        self
    }

    /// Controls only default request records, not observers or counters.
    #[must_use]
    #[inline]
    pub fn with_request_records(mut self, enabled: bool) -> Self {
        self.native.request_records = enabled;
        self
    }

    /// Shares managed counters and the optional native observer with the runner.
    #[must_use]
    #[inline]
    pub fn with_diagnostics(mut self, diagnostics: NativeDiagnosticsHandle) -> Self {
        self.native.diagnostics = diagnostics;
        self
    }

    #[cfg(test)]
    async fn run_with_listener(self, listener: TokioTcpListener) -> anyhow::Result<()> {
        let AxumDevServer {
            router,
            config,
            stores,
            json_body_limit,
            native,
        } = self;
        serve_with_stores(
            App::new(router),
            listener,
            config.enable_ctrl_c,
            stores,
            json_body_limit,
            native,
        )
        .await
    }

    #[must_use]
    #[inline]
    pub fn with_config(router: RouterService, config: AxumDevServerConfig) -> Self {
        Self {
            config,
            json_body_limit: JsonBodyLimit::DEFAULT,
            native: NativeHosting::default(),
            router,
            stores: Stores::default(),
        }
    }

    /// Selects the ceiling for framework-managed JSON buffering, in bytes.
    ///
    /// # Errors
    /// Rejects zero and values above the collector's representable capacity.
    #[inline]
    pub fn with_json_body_limit_bytes(mut self, limit: usize) -> anyhow::Result<Self> {
        self.json_body_limit = JsonBodyLimit::new(limit)?;
        Ok(self)
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

fn kv_store_file_name(store_name: &str) -> String {
    format!(
        "kv-{}-{:016x}.redb",
        store_name_slug(store_name),
        stable_store_name_hash(store_name)
    )
}

fn kv_store_path(store_name: &str) -> PathBuf {
    PathBuf::from(".edgezero").join(kv_store_file_name(store_name))
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

impl From<Stores> for PreparedStores {
    #[inline]
    fn from(stores: Stores) -> Self {
        Self {
            config: stores.config_registry.or_else(|| {
                stores.config_store.map(|handle| {
                    ConfigRegistry::single_id(
                        "default".to_owned(),
                        ConfigStoreBinding {
                            handle,
                            default_key: "default".to_owned(),
                        },
                    )
                })
            }),
            kv: stores.kv_registry.or_else(|| {
                stores
                    .kv
                    .map(|handle| KvRegistry::single_id("default".to_owned(), handle))
            }),
            secrets: stores.secret_registry.or_else(|| {
                stores.secrets.map(|handle| {
                    SecretRegistry::single_id(
                        "default".to_owned(),
                        BoundSecretStore::new(handle, "default".to_owned()),
                    )
                })
            }),
        }
    }
}

#[cfg(test)]
async fn serve_with_stores(
    app: App,
    listener: TokioTcpListener,
    enable_ctrl_c: bool,
    stores: Stores,
    json_body_limit: JsonBodyLimit,
    native: NativeHosting,
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
            stores,
            json_body_limit,
            native,
            shutdown_receiver,
        )
    });
    let result = worker.await.context("axum server thread failed")?;
    drop(shutdown_guard);
    result
}

#[cfg(test)]
fn serve_local(
    app: App,
    std_listener: StdTcpListener,
    enable_ctrl_c: bool,
    stores: Stores,
    json_body_limit: JsonBodyLimit,
    native: NativeHosting,
    shutdown_receiver: ShutdownReceiver<()>,
) -> anyhow::Result<()> {
    run_local(
        ListenerSource::Prebound(std_listener),
        enable_ctrl_c,
        Duration::from_secs(10),
        async move {
            let _closed = shutdown_receiver.await;
        },
        async move { prepare_transport(app, stores.into(), json_body_limit, native) },
    )
}

#[derive(Clone, Debug)]
struct NativeHosting {
    diagnostics: NativeDiagnosticsHandle,
    policy: TrustedProxyPolicy,
    request_records: bool,
}

impl Default for NativeHosting {
    fn default() -> Self {
        Self {
            diagnostics: NativeDiagnosticsHandle::new(),
            policy: TrustedProxyPolicy::no_trust(),
            request_records: true,
        }
    }
}

type PreparedApp = (
    App,
    PreparedStores,
    Arc<reqwest::Client>,
    JsonBodyLimit,
    NativeHosting,
);

enum ListenerSource {
    Bind(SocketAddr),
    #[cfg(test)]
    Prebound(StdTcpListener),
}

#[cfg_attr(
    not(test),
    expect(
        clippy::needless_pass_by_value,
        reason = "test embedding consumes a prebound listener through the same source"
    )
)]
fn adopt_listener(source: ListenerSource) -> anyhow::Result<TokioTcpListener> {
    let listener = match source {
        ListenerSource::Bind(addr) => StdTcpListener::bind(addr)
            .map_err(|_error| failure("bind", "listener", "address unavailable"))?,
        #[cfg(test)]
        ListenerSource::Prebound(listener) => listener,
    };
    listener
        .set_nonblocking(true)
        .map_err(|_error| failure("bind", "listener", "nonblocking setup failed"))?;
    TokioTcpListener::from_std(listener)
        .map_err(|_error| failure("bind", "listener", "runtime adoption failed"))
}

fn prepare_transport(
    app: App,
    stores: PreparedStores,
    json_body_limit: JsonBodyLimit,
    native: NativeHosting,
) -> anyhow::Result<PreparedApp> {
    let transport = AxumOutboundClient::try_transport()
        .map_err(|_error| failure("transport", "HTTP client", "initialization failed"))?;
    Ok((app, stores, transport, json_body_limit, native))
}

struct Signals {
    #[cfg(unix)]
    interrupt: signal::unix::Signal,
    #[cfg(unix)]
    terminate: signal::unix::Signal,
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "registration precedes observation in this lifecycle owner"
)]
impl Signals {
    fn register(enabled: bool) -> anyhow::Result<Option<Self>> {
        if !enabled {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            let interrupt = signal::unix::signal(signal::unix::SignalKind::interrupt())
                .map_err(|_error| failure("signals", "SIGINT", "registration failed"))?;
            let terminate = signal::unix::signal(signal::unix::SignalKind::terminate())
                .map_err(|_error| failure("signals", "SIGTERM", "registration failed"))?;
            Ok(Some(Self {
                interrupt,
                terminate,
            }))
        }
        #[cfg(not(unix))]
        {
            Ok(Some(Self {}))
        }
    }

    #[expect(
        clippy::integer_division_remainder_used,
        reason = "tokio select branch arithmetic"
    )]
    async fn next(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let received = tokio::select! {
                received = self.interrupt.recv() => received,
                received = self.terminate.recv() => received,
            };
            received.ok_or_else(|| failure("signals", "termination", "observation closed"))
        }
        #[cfg(not(unix))]
        {
            signal::ctrl_c()
                .await
                .map_err(|_error| failure("signals", "SIGINT", "registration failed"))
        }
    }
}

async fn next_signal(signals: &mut Option<Signals>) -> anyhow::Result<()> {
    match signals {
        Some(source) => source.next().await,
        None => future::pending().await,
    }
}

fn run_local<Prepare, Stop>(
    listener: ListenerSource,
    enable_signals: bool,
    grace: Duration,
    stop: Stop,
    prepare: Prepare,
) -> anyhow::Result<()>
where
    Prepare: Future<Output = anyhow::Result<PreparedApp>>,
    Stop: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_error| {
            log_native_lifecycle(
                LifecyclePhase::Starting,
                "runtime_creation_failed",
                grace,
                true,
            );
            failure("runtime", "local runner", "creation failed")
        })?;
    let local = LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let mut signals = Signals::register(enable_signals).inspect_err(|_error| {
            log_native_lifecycle(
                LifecyclePhase::Starting,
                "signal_registration_failed",
                grace,
                true,
            );
        })?;
        let (phase, reader) = watch::channel(LifecyclePhase::Starting);
        log_native_lifecycle(LifecyclePhase::Starting, "starting", grace, false);
        let result = run_owned(listener, grace, stop, prepare, &mut signals, &phase, reader).await;
        phase.send_replace(LifecyclePhase::Stopped);
        log_native_lifecycle(
            LifecyclePhase::Stopped,
            if result.is_ok() {
                "stopped"
            } else {
                "runner_failed"
            },
            grace,
            result.is_err(),
        );
        result
    }))
}

#[expect(
    clippy::integer_division_remainder_used,
    reason = "tokio select branch arithmetic"
)]
async fn run_owned<Prepare, Stop>(
    listener_source: ListenerSource,
    grace: Duration,
    stop: Stop,
    prepare: Prepare,
    signals: &mut Option<Signals>,
    phase: &watch::Sender<LifecyclePhase>,
    reader: watch::Receiver<LifecyclePhase>,
) -> anyhow::Result<()>
where
    Prepare: Future<Output = anyhow::Result<PreparedApp>>,
    Stop: Future<Output = ()>,
{
    tokio::pin!(stop);
    tokio::pin!(prepare);
    let prepared = tokio::select! {
        biased;
        signal = next_signal(signals) => {
            signal?;
            phase.send_replace(LifecyclePhase::Draining);
            log_native_lifecycle(LifecyclePhase::Draining, "signal_stop", grace, false);
            return Ok(());
        }
        () = &mut stop => {
            phase.send_replace(LifecyclePhase::Draining);
            log_native_lifecycle(LifecyclePhase::Draining, "stop_requested", grace, false);
            return Ok(());
        }
        prepared = &mut prepare => prepared.inspect_err(|_error| log_native_lifecycle(LifecyclePhase::Starting, "startup_failed", grace, true))?,
    };
    let (app, stores, transport, json_body_limit, native) = prepared;
    let session = NativeSession::new(native.diagnostics, native.request_records)?;
    let mut service =
        AxumServiceState::with_transport(app, transport, json_body_limit, session, native.policy)
            .with_phase(reader.clone());
    if let Some(registry) = stores.config {
        service = service.with_config_registry(registry);
    }
    if let Some(registry) = stores.kv {
        service = service.with_kv_registry(registry);
    }
    if let Some(registry) = stores.secrets {
        service = service.with_secret_registry(registry);
    }
    // Give stop observation another poll before bind/adoption and readiness.
    let listener = tokio::select! {
        biased;
        signal = next_signal(signals) => { signal?; phase.send_replace(LifecyclePhase::Draining); log_native_lifecycle(LifecyclePhase::Draining, "signal_stop", grace, false); return Ok(()); }
        () = &mut stop => { phase.send_replace(LifecyclePhase::Draining); log_native_lifecycle(LifecyclePhase::Draining, "stop_requested", grace, false); return Ok(()); }
        result = async { yield_now().await; adopt_listener(listener_source) } => result?,
    };
    phase.send_replace(LifecyclePhase::Ready);
    log_native_lifecycle(LifecyclePhase::Ready, "ready", grace, false);
    let mut tasks = JoinSet::new();
    let mut outcome = Ok(());
    loop {
        tokio::select! {
            biased;
            signal = next_signal(signals) => { outcome = signal; break; }
            () = &mut stop => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_some_and(|joined| joined.is_err()) {
                    outcome = Err(failure("runner", "connection task", "unexpected task failure"));
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, remote_addr)) => {
                        let responses = EgressConnection::default();
                        // Own the guard before submission, including unpolled cancellation.
                        let (activity, connection_guard) = service.connection_activity();
                        let connection_service = service.for_connection(remote_addr, responses.clone()).with_activity(activity);
                        let connection_phase = reader.clone();
                        tasks.spawn_local(async move {
                            let _connection_guard = connection_guard;
                            if serve_http1_with_shutdown(stream, connection_service, responses, Some(connection_phase)).await.is_err() {
                                log::debug!("axum HTTP/1 connection closed");
                            }
                        });
                    }
                    Err(_error) => {
                        outcome = Err(failure("runner", "listener", "accept failed"));
                        break;
                    }
                }
            }
        }
    }
    phase.send_replace(LifecyclePhase::Draining);
    log_native_lifecycle(
        LifecyclePhase::Draining,
        if outcome.is_ok() {
            "drain_requested"
        } else {
            "runner_failed"
        },
        grace,
        outcome.is_err(),
    );
    drop(listener);
    let result = drain_tasks(&mut tasks, grace, signals, outcome).await;
    // Service/registry clones are dropped only after every owned connection settles.
    drop(service);
    result
}

#[expect(
    clippy::integer_division_remainder_used,
    reason = "tokio select branch arithmetic"
)]
async fn drain_tasks(
    tasks: &mut JoinSet<()>,
    grace: Duration,
    signals: &mut Option<Signals>,
    mut outcome: anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(deadline) = NativeInstant::now().checked_add(grace) else {
        log_native_lifecycle(
            LifecyclePhase::Draining,
            "unusable_grace_deadline",
            grace,
            true,
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        return Err(failure("shutdown", "budget", "unusable deadline"));
    };
    while !tasks.is_empty() {
        tokio::select! {
            biased;
            signal = next_signal(signals) => {
                log_native_lifecycle(LifecyclePhase::Draining, "forced_stop", grace, true);
                outcome = Err(signal.err().unwrap_or_else(|| failure("shutdown", "signal", "forced stop")));
                break;
            }
            () = sleep_until(deadline) => {
                log_native_lifecycle(LifecyclePhase::Draining, "grace_exhausted", grace, true);
                outcome = Err(failure("shutdown", "budget", "grace exhausted"));
                break;
            }
            result = tasks.join_next() => {
                if result.is_some_and(|joined| joined.is_err()) {
                    outcome = Err(failure("runner", "connection task", "unexpected task failure"));
                }
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    outcome
}

fn log_native_lifecycle(phase: LifecyclePhase, reason: &'static str, grace: Duration, fault: bool) {
    // Callers are private owner branches and pass fixed categories, never returned errors.
    let phase_name = match phase {
        LifecyclePhase::Starting => "starting",
        LifecyclePhase::Ready => "ready",
        LifecyclePhase::Draining => "draining",
        LifecyclePhase::Stopped => "stopped",
    };
    let level = if fault { Level::Error } else { Level::Info };
    let _logged = catch_unwind(
        || log::log!(target: "edgezero::native", level, "event=lifecycle phase={phase_name} reason={reason} grace_ms={}", grace.as_millis()),
    );
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
    App::build::<A>(crate::AXUM_PLATFORM)
        .map_err(|_error| anyhow::anyhow!("application configuration failed"))
}

/// Runs an application with the Axum development server.
///
/// # Errors
/// Returns an error if application configuration, runtime setup, store initialization, listener
/// binding, or connection serving fails.
#[inline]
pub fn run_app<A: Hooks>() -> anyhow::Result<()> {
    let (env, json_body_limit, native) = capture_dev_environment(vars_os())?;
    let grace = shutdown_grace(&env)?;
    let resolution = resolve_addr(&env);
    run_local(
        ListenerSource::Bind(resolution.addr),
        true,
        grace,
        future::pending::<()>(),
        async move {
            let app = build_app_for_dispatch::<A>()?;
            let metadata = A::stores();
            let level = env
                .logging_level()
                .and_then(|raw| LevelFilter::from_str(raw).ok())
                .unwrap_or(LevelFilter::Info);
            init_logging::<A>(level);
            for _warning in &resolution.warnings {
                log::warn!("invalid supplied development bind setting; using fallback");
            }
            let stores = PreparedStores {
                kv: build_kv_registry(metadata.kv, &env, kv_init_requirement(metadata))?,
                config: build_config_registry(metadata.config, &env),
                secrets: build_secret_registry(metadata.secrets, &env),
            };
            prepare_transport(app, stores, json_body_limit, native)
        },
    )
}

fn capture_dev_environment<I>(vars: I) -> anyhow::Result<(EnvConfig, JsonBodyLimit, NativeHosting)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut pairs = Vec::new();
    for (os_key, os_value) in vars {
        let Some(key) = os_key.to_str() else {
            continue;
        };
        let recognized = key.strip_prefix("EDGEZERO__").and_then(|path| {
            [
                ("ADAPTER__JSON_BODY_LIMIT_BYTES", "JSON_BODY_LIMIT_BYTES"),
                ("ADAPTER__TRUSTED_PROXY_CIDRS", "TRUSTED_PROXY_CIDRS"),
                (
                    "ADAPTER__FORWARDING_HEADER_FAMILY",
                    "FORWARDING_HEADER_FAMILY",
                ),
                ("LOGGING__REQUEST_RECORDS", "LOGGING__REQUEST_RECORDS"),
            ]
            .into_iter()
            .find(|(candidate, _)| path.eq_ignore_ascii_case(candidate))
            .map(|(_, setting)| setting)
        });
        match (os_value.to_str(), recognized) {
            (Some(value), _) => pairs.push((key.to_owned(), value.to_owned())),
            (None, Some(setting)) => return Err(failure("settings", setting, "non-Unicode value")),
            (None, None) => {}
        }
    }
    let env = EnvConfig::from_vars(pairs);
    let limit = JsonBodyLimit::from_config(&env)?;
    let (policy, request_records) = native_settings(&env)?;
    Ok((
        env,
        limit,
        NativeHosting {
            policy,
            request_records,
            diagnostics: NativeDiagnosticsHandle::new(),
        },
    ))
}

fn init_logging<A: Hooks>(level: LevelFilter) {
    if !A::owns_logging() {
        let _logger_init = SimpleLogger::new().with_level(level).init();
    }
}

fn no_initialize<'startup>(
    _app: &'startup mut App,
    _stores: &'startup PreparedStores,
) -> LocalStartupFuture<'startup> {
    Box::pin(future::ready(Ok(())))
}

/// Runs strict framework startup with SIGINT and Unix SIGTERM. No callback is
/// required. Ready does not claim application-specific schema/bootstrap checks.
///
/// # Errors
/// Returns classified startup/runner errors or non-success for forced shutdown.
#[inline]
pub fn run_production_app<A: Hooks>() -> anyhow::Result<()> {
    run_production_selected::<A, _, _>(
        AxumRunOptions::from_env()?,
        true,
        future::pending::<()>(),
        no_initialize,
    )
}

/// Runs strict startup plus exactly one application-owned local initializer.
/// Pass an inline `|app, stores| Box::pin(async move { ... })` or a boxed function
/// so Rust can infer the higher-ranked borrow lifetime. Futures need not be Send.
///
/// # Errors
/// Returns safe classified errors; callback/provider source chains are discarded.
#[inline]
pub fn run_production_app_with_initializer<A, Initialize>(
    initialize: Initialize,
) -> anyhow::Result<()>
where
    A: Hooks,
    Initialize: for<'startup> FnOnce(
        &'startup mut App,
        &'startup PreparedStores,
    ) -> LocalStartupFuture<'startup>,
{
    run_production_selected::<A, _, _>(
        AxumRunOptions::from_env()?,
        true,
        future::pending::<()>(),
        initialize,
    )
}

/// Runs production with explicit options and caller-owned stop, installing no
/// process-wide signal handlers and merging no ambient hosting/store settings.
///
/// # Errors
/// Returns classified startup/runner errors or grace exhaustion.
#[inline]
pub fn run_app_with_options<A, Stop>(options: AxumRunOptions, stop: Stop) -> anyhow::Result<()>
where
    A: Hooks,
    Stop: Future<Output = ()>,
{
    run_app_with_options_and_initializer::<A, _, _>(options, stop, no_initialize)
}

/// Runs production embedding with app-selected startup checks before acceptance.
/// Retained cloned KV handles remain the caller's responsibility after return.
///
/// # Errors
/// Returns classified startup/runner errors or grace exhaustion.
#[inline]
pub fn run_app_with_options_and_initializer<A, Initialize, Stop>(
    options: AxumRunOptions,
    stop: Stop,
    initialize: Initialize,
) -> anyhow::Result<()>
where
    A: Hooks,
    Stop: Future<Output = ()>,
    Initialize: for<'startup> FnOnce(
        &'startup mut App,
        &'startup PreparedStores,
    ) -> LocalStartupFuture<'startup>,
{
    run_production_selected::<A, _, _>(options, false, stop, initialize)
}

fn run_production_selected<A, Initialize, Stop>(
    options: AxumRunOptions,
    signals: bool,
    stop: Stop,
    initialize: Initialize,
) -> anyhow::Result<()>
where
    A: Hooks,
    Stop: Future<Output = ()>,
    Initialize: for<'startup> FnOnce(
        &'startup mut App,
        &'startup PreparedStores,
    ) -> LocalStartupFuture<'startup>,
{
    run_local(
        ListenerSource::Bind(options.addr()),
        signals,
        options.shutdown_grace(),
        stop,
        async move {
            let metadata = A::stores();
            options.validate_stores(metadata)?;
            let mut app = build_app_for_dispatch::<A>()?;
            init_logging::<A>(options.logging_level());
            let stores = build_production_stores(metadata, &options)?;
            let transport = AxumOutboundClient::try_transport()
                .map_err(|_error| failure("transport", "HTTP client", "initialization failed"))?;
            initialize(&mut app, &stores).await.map_err(|error| {
                let category = error.store_extraction_reason().map_or_else(
                    || "application checks failed".to_owned(),
                    |reason| format!("{reason:?}"),
                );
                failure("initializer", "application", &category)
            })?;
            Ok((
                app,
                stores,
                transport,
                options.json_body_limit(),
                NativeHosting {
                    diagnostics: options.diagnostics(),
                    policy: options.trusted_proxy_policy(),
                    request_records: options.request_records(),
                },
            ))
        },
    )
}

fn check_data_write_access(root: &Path) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _attempt in 0..8_u8 {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            ".edgezero-write-check-{}-{sequence}",
            process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                fs::remove_file(path).map_err(|_error| {
                    failure("stores", "DATA_DIR", "write-probe cleanup failed")
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(_error) => return Err(failure("stores", "DATA_DIR", "directory is not writable")),
        }
    }
    Err(failure("stores", "DATA_DIR", "write-probe unavailable"))
}

fn build_production_stores(
    metadata: StoresMetadata,
    options: &AxumRunOptions,
) -> anyhow::Result<PreparedStores> {
    let mut stores = PreparedStores::default();
    if let Some(meta) = metadata.config {
        let root = options
            .config_dir()
            .ok_or_else(|| failure("settings", "CONFIG_DIR", "required"))?;
        let mut by_id = BTreeMap::new();
        for id in meta.ids {
            let store =
                AxumConfigStore::from_required_path(&root.join(format!("local-config-{id}.json")))
                    .map_err(|error| failure("config", id, &error.to_string()))?;
            by_id.insert(
                (*id).to_owned(),
                ConfigStoreBinding {
                    handle: ConfigStoreHandle::new(Arc::new(store)),
                    default_key: options.env_config().store_key("config", id),
                },
            );
        }
        stores.config = Some(
            ConfigRegistry::from_parts(by_id, meta.default.to_owned()).ok_or_else(|| {
                failure("config", meta.default, "invalid declared default binding")
            })?,
        );
    }
    if let Some(meta) = metadata.kv {
        let root = options
            .data_dir()
            .ok_or_else(|| failure("settings", "DATA_DIR", "required"))?;
        check_data_write_access(root)?;
        let mut by_id = BTreeMap::new();
        let mut handles: BTreeMap<String, KvHandle> = BTreeMap::new();
        for id in meta.ids {
            let name = options.env_config().store_name("kv", id);
            let handle = if let Some(handle) = handles.get(&name) {
                handle.clone()
            } else {
                let store = PersistentKvStore::new(root.join(kv_store_file_name(&name)))
                    .map_err(|_error| failure("KV", id, "database unavailable or locked"))?;
                let handle = KvHandle::new(Arc::new(store));
                handles.insert(name, handle.clone());
                handle
            };
            by_id.insert((*id).to_owned(), handle);
        }
        stores.kv = Some(
            KvRegistry::from_parts(by_id, meta.default.to_owned())
                .ok_or_else(|| failure("KV", meta.default, "invalid declared default binding"))?,
        );
    }
    stores.secrets = build_secret_registry(metadata.secrets, options.env_config());
    Ok(stores)
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
    let mut handles: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("kv", id);
        if let Some(handle) = handles.get(&store_name) {
            by_id.insert((*id).to_owned(), handle.clone());
            continue;
        }
        let kv_path = kv_store_path(&store_name);
        let handle = match kv_handle_from_path(&kv_path) {
            Ok(handle) => handle,
            Err(_error) => match init {
                KvInitRequirement::Optional => {
                    log::warn!("KV store id `{id}` unavailable; omitting optional binding");
                    continue;
                }
                KvInitRequirement::Required => {
                    return Err(failure("KV", id, "declared database unavailable or locked"));
                }
            },
        };
        handles.insert(store_name, handle.clone());
        by_id.insert((*id).to_owned(), handle);
    }

    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
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
/// warning and the id is dropped from the registry rather than failing
/// startup, matching the cloudflare config-binding behaviour.
fn build_config_registry(
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    let mut by_id: BTreeMap<String, ConfigStoreBinding> = BTreeMap::new();
    for id in meta.ids {
        let store = match AxumConfigStore::from_local_file(id) {
            Ok(store) => store,
            Err(_error) => {
                log::warn!(
                    "config store id `{id}` could not be loaded; dropping this id from the registry"
                );
                continue;
            }
        };
        by_id.insert(
            (*id).to_owned(),
            ConfigStoreBinding {
                handle: ConfigStoreHandle::new(Arc::new(store)),
                default_key: env.store_key("config", id),
            },
        );
    }
    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            "config registry default id `{default_id}` failed to load; dropping the config registry — \
             handlers will see no config store"
        );
    }
    StoreRegistry::from_parts(by_id, default_id)
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
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingConfiguration;

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
    fn initializer_borrows_inputs_across_await_and_captures_rc() {
        use futures::executor::block_on;
        use std::cell::Cell;
        use std::rc::Rc;

        async fn initialize<Initialize>(
            app: &mut App,
            stores: &PreparedStores,
            callback: Initialize,
        ) -> Result<(), EdgeError>
        where
            Initialize: for<'startup> FnOnce(
                &'startup mut App,
                &'startup PreparedStores,
            ) -> LocalStartupFuture<'startup>,
        {
            callback(app, stores).await
        }
        let calls = Rc::new(Cell::new(0_u32));
        let recorded = Rc::clone(&calls);
        let mut owned_app = App::new(RouterService::builder().build());
        block_on(initialize(
            &mut owned_app,
            &PreparedStores::default(),
            move |app, stores| {
                Box::pin(async move {
                    future::ready(()).await;
                    assert!(stores.config().is_none());
                    app.insert_state(Arc::<str>::from("initialized"));
                    recorded.set(recorded.get() + 1);
                    Ok(())
                })
            },
        ))
        .expect("initialize");
        assert_eq!(calls.get(), 1);
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
        };
        assert_eq!(config.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(config.addr.port(), 3000);
        assert!(!config.enable_ctrl_c);
    }

    #[test]
    fn json_limit_development_capture_and_explicit_router_builder() {
        let (env, selected, _native) = capture_dev_environment([
            (
                OsString::from("EDGEZERO__ADAPTER__HOST"),
                OsString::from("invalid-host"),
            ),
            (
                OsString::from("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES"),
                OsString::from("64"),
            ),
        ])
        .expect("permissive bind and strict JSON option");
        assert_eq!(selected.get().get(), 64);
        assert_eq!(resolve_addr(&env).addr.ip(), addr::DEFAULT_HOST);
        let router = RouterService::builder().build();
        let default = AxumDevServer::new(router.clone());
        assert_eq!(default.json_body_limit.get().get(), 0x0020_0000);
        let explicit = AxumDevServer::with_config(router, AxumDevServerConfig::default())
            .with_json_body_limit_bytes(64)
            .expect("builder");
        let prepared = prepare_transport(
            App::new(explicit.router),
            explicit.stores.into(),
            explicit.json_body_limit,
            explicit.native,
        )
        .expect("prepared transport");
        assert_eq!(prepared.3.get().get(), 64);
        assert!(
            AxumDevServer::new(RouterService::builder().build())
                .with_json_body_limit_bytes(0)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn development_capture_skips_unrelated_binary_settings_but_rejects_binary_json_limit() {
        use std::os::unix::ffi::OsStringExt as _;
        let binary = OsString::from_vec(vec![0xff]);
        let (env, selected, _native) = capture_dev_environment([
            (binary.clone(), OsString::from("ignored")),
            (OsString::from("UNRELATED"), binary.clone()),
            (OsString::from("EDGEZERO__ADAPTER__HOST"), binary.clone()),
            (
                OsString::from("EDGEZERO__STORES__KV__DEFAULT__NAME"),
                binary.clone(),
            ),
        ])
        .expect("unrelated omissions");
        assert_eq!(selected.get().get(), 0x0020_0000);
        assert!(env.adapter_host().is_none());
        assert_eq!(env.store_name("kv", "default"), "default");
        let error = capture_dev_environment([(
            OsString::from("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES"),
            binary,
        )])
        .expect_err("recognized binary setting");
        assert!(error.to_string().contains("JSON_BODY_LIMIT_BYTES"));
        assert_eq!(error.chain().count(), 1);
    }

    #[test]
    fn dev_server_new_uses_default_config() {
        use edgezero_core::router::RouterService;

        let router = RouterService::builder().build();
        let server = AxumDevServer::new(router);
        assert_eq!(server.config.addr.port(), 8787);
        assert!(server.config.enable_ctrl_c);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn router_only_default_json_boundary_for_both_constructors() {
        use edgezero_core::action;
        use edgezero_core::extractor::Json;
        use edgezero_core::http::StatusCode;
        #[action]
        async fn count_json(Json(value): Json<String>) -> String {
            value.len().to_string()
        }
        let limit = 0x0020_0000_usize;
        for configured in [false, true] {
            let listener = TokioTcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let addr = listener.local_addr().expect("address");
            let router = RouterService::builder().post("/json", count_json).build();
            let server = if configured {
                AxumDevServer::with_config(
                    router,
                    AxumDevServerConfig {
                        addr,
                        enable_ctrl_c: false,
                    },
                )
            } else {
                AxumDevServer::new(router)
            };
            let serving = tokio::spawn(async move { server.run_with_listener(listener).await });
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client");
            for (bytes, status) in [
                (limit, StatusCode::OK),
                (limit.saturating_add(1), StatusCode::PAYLOAD_TOO_LARGE),
            ] {
                let body = format!(
                    "\"{}\"",
                    "x".repeat(bytes.checked_sub(2).expect("delimiters"))
                );
                let response = client
                    .post(format!("http://{addr}/json"))
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .expect("prebound listener request");
                assert_eq!(response.status(), status);
                let text = response.text().await.expect("usable response");
                if status == StatusCode::OK {
                    assert_eq!(text, limit.checked_sub(2).expect("length").to_string());
                } else {
                    assert!(text.contains("payload_too_large"));
                }
            }
            serving.abort();
            assert!(serving.await.expect_err("cancel server").is_cancelled());
        }
    }

    #[test]
    fn dev_server_with_config_uses_custom_config() {
        use edgezero_core::router::RouterService;

        let router = RouterService::builder().build();
        let config = AxumDevServerConfig {
            addr: SocketAddr::from(([127, 0, 0, 1], 9000)),
            enable_ctrl_c: false,
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
        let registry = build_config_registry(Some(meta), &env).expect("registry built");
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
        let registry = build_config_registry(Some(meta), &env).expect("registry built");
        let binding = registry.named("app_config").expect("binding registered");
        assert_eq!(binding.default_key, "app_config");
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
