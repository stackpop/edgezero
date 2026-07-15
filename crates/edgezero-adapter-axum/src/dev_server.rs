use std::fs;
#[cfg(test)]
use std::iter;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
use tower::{Service as _, service_fn};

use edgezero_core::addr;
use edgezero_core::app::{Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry, StoreRegistry,
};
use log::LevelFilter;
use simple_logger::SimpleLogger;
use std::collections::BTreeMap;

use crate::config_store::AxumConfigStore;
use crate::key_value_store::PersistentKvStore;
use crate::secret_store::EnvSecretStore;
use crate::service::EdgeZeroAxumService;

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

        serve_with_stores(router, listener, config.enable_ctrl_c, stores).await
    }

    #[cfg(test)]
    async fn run_with_listener(self, listener: TokioTcpListener) -> anyhow::Result<()> {
        let AxumDevServer {
            router,
            config,
            stores,
        } = self;
        serve_with_stores(router, listener, config.enable_ctrl_c, stores).await
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
    router: RouterService,
    listener: TokioTcpListener,
    enable_ctrl_c: bool,
    stores: Stores,
) -> anyhow::Result<()> {
    let service = {
        let mut service = EdgeZeroAxumService::new(router);
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
        service
    };
    let axum_router = Router::new().fallback_service(service_fn(move |req| {
        let mut svc = service.clone();
        async move { svc.call(req).await }
    }));
    let make_service = axum_router.into_make_service_with_connect_info::<SocketAddr>();

    let shutdown = enable_ctrl_c.then_some(async {
        let _ctrl_c = signal::ctrl_c().await;
    });

    let server = axum::serve(listener, make_service);
    if let Some(shutdown_signal) = shutdown {
        let graceful_server = server.with_graceful_shutdown(shutdown_signal);
        graceful_server.await.context("axum server error")?;
    } else {
        server.await.context("axum server error")?;
    }

    Ok(())
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
#[inline]
pub fn run_app<A: Hooks>() -> anyhow::Result<()> {
    let env = EnvConfig::from_env();
    let stores = A::stores();
    let kv_init_requirement = kv_init_requirement(stores);

    let level = env
        .logging_level()
        .and_then(|raw| LevelFilter::from_str(raw).ok())
        .unwrap_or(LevelFilter::Info);

    if !A::owns_logging() {
        let _logger_init = SimpleLogger::new().with_level(level).init();
    }

    let resolution = resolve_addr(&env);
    for warning in &resolution.warnings {
        log::warn!("{warning}");
    }
    let addr = resolution.addr;
    let app = A::build_app();
    let router = app.router().clone();

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

        let kv_registry = build_kv_registry(stores.kv, &env, kv_init_requirement)?;
        let config_registry = build_config_registry(stores.config, &env);
        let secret_registry = build_secret_registry(stores.secrets, &env);

        let request_stores = Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Stores::default()
        };
        serve_with_stores(router, listener, true, request_stores).await
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
            Err(err) => {
                log::warn!(
                    "config store for id `{}` could not be loaded from {}: {}; \
                     dropping this id from the registry",
                    id,
                    AxumConfigStore::local_path(id).display(),
                    err
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
    use std::net::{IpAddr, Ipv4Addr};

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

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_forwards_headers() {
        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let value = ctx
                .request()
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
        assert!(body.contains("required secret is not configured"));

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
        assert!(body.contains(
            "no secret store configured -- check [stores.secrets] in edgezero.toml and platform bindings"
        ));

        server.handle.abort();
    }
}
