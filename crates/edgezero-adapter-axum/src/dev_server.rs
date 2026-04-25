use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::Router;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
use tower::{service_fn, Service};

use edgezero_core::app::Hooks;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::manifest::ManifestLoader;
use edgezero_core::router::RouterService;
use edgezero_core::secret_store::SecretHandle;
use log::LevelFilter;
use simple_logger::SimpleLogger;

use crate::config_store::AxumConfigStore;
use crate::service::EdgeZeroAxumService;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KvInitRequirement {
    Optional,
    Required,
}

/// Configuration used when running the dev server embedding EdgeZero into Axum.
#[derive(Clone)]
pub struct AxumDevServerConfig {
    pub addr: SocketAddr,
    pub enable_ctrl_c: bool,
}

impl Default for AxumDevServerConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 8787)),
            enable_ctrl_c: true,
        }
    }
}

/// Optional store handles attached to every request processed by the dev server.
///
/// Build with struct init and `..Default::default()` for the fields you do not need:
///
/// ```rust,ignore
/// let stores = Stores { kv: Some(kv_handle), ..Default::default() };
/// ```
#[derive(Default)]
struct Stores {
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    secrets: Option<SecretHandle>,
}

/// Blocking dev server runner used by the EdgeZero CLI.
pub struct AxumDevServer {
    router: RouterService,
    config: AxumDevServerConfig,
    stores: Stores,
}

impl AxumDevServer {
    pub fn new(router: RouterService) -> Self {
        Self {
            router,
            config: AxumDevServerConfig::default(),
            stores: Stores::default(),
        }
    }

    pub fn with_config(router: RouterService, config: AxumDevServerConfig) -> Self {
        Self {
            router,
            config,
            stores: Stores::default(),
        }
    }

    #[must_use]
    pub fn with_config_store(mut self, handle: ConfigStoreHandle) -> Self {
        self.stores.config_store = Some(handle);
        self
    }

    /// Attach a KV store to the dev server.
    ///
    /// The handle is shared across all requests, making the `Kv` extractor
    /// available in handlers.
    #[must_use]
    pub fn with_kv_handle(mut self, handle: KvHandle) -> Self {
        self.stores.kv = Some(handle);
        self
    }

    /// Attach a secret store to the dev server.
    ///
    /// The handle is shared across all requests, making the `Secrets` extractor
    /// available in handlers.
    #[must_use]
    pub fn with_secret_handle(mut self, handle: SecretHandle) -> Self {
        self.stores.secrets = Some(handle);
        self
    }

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
        let listener = StdTcpListener::bind(config.addr)
            .with_context(|| format!("failed to bind dev server to {}", config.addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set listener to non-blocking")?;

        let listener = tokio::net::TcpListener::from_std(listener)
            .context("failed to adopt std listener into tokio")?;

        serve_with_stores(router, listener, config.enable_ctrl_c, stores).await
    }

    #[cfg(test)]
    async fn run_with_listener(self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        let AxumDevServer {
            router,
            config,
            stores,
        } = self;
        serve_with_stores(router, listener, config.enable_ctrl_c, stores).await
    }
}

fn kv_init_requirement(manifest: &edgezero_core::manifest::Manifest) -> KvInitRequirement {
    if manifest.stores.kv.is_some() {
        KvInitRequirement::Required
    } else {
        KvInitRequirement::Optional
    }
}

fn kv_store_path(store_name: &str) -> PathBuf {
    if store_name == edgezero_core::manifest::DEFAULT_KV_STORE_NAME {
        return PathBuf::from(".edgezero/kv.redb");
    }

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
            Some(ch) => {
                if slug.len() == MAX_SLUG_LEN {
                    break;
                }
                slug.push(ch);
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
        "store".to_string()
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

fn kv_handle_from_path(kv_path: &Path) -> anyhow::Result<edgezero_core::key_value_store::KvHandle> {
    if let Some(parent) = kv_path.parent() {
        std::fs::create_dir_all(parent).context("failed to create KV store directory")?;
    }
    let kv_store = std::sync::Arc::new(
        crate::key_value_store::PersistentKvStore::new(kv_path)
            .context("failed to create KV store")?,
    );
    log::info!("KV store: {}", kv_path.display());
    Ok(edgezero_core::key_value_store::KvHandle::new(kv_store))
}

async fn serve_with_stores(
    router: RouterService,
    listener: tokio::net::TcpListener,
    enable_ctrl_c: bool,
    stores: Stores,
) -> anyhow::Result<()> {
    let service = {
        let mut service = EdgeZeroAxumService::new(router);
        if let Some(handle) = stores.config_store {
            service = service.with_config_store_handle(handle);
        }
        if let Some(handle) = stores.kv {
            service = service.with_kv_handle(handle);
        }
        if let Some(handle) = stores.secrets {
            service = service.with_secret_handle(handle);
        }
        service
    };
    let router = Router::new().fallback_service(service_fn(move |req| {
        let mut svc = service.clone();
        async move { svc.call(req).await }
    }));
    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();

    let shutdown = enable_ctrl_c.then_some(async {
        let _ctrl_c = signal::ctrl_c().await;
    });

    let server = axum::serve(listener, make_service);
    if let Some(shutdown) = shutdown {
        let server = server.with_graceful_shutdown(shutdown);
        server.await.context("axum server error")?;
    } else {
        server.await.context("axum server error")?;
    }

    Ok(())
}

pub fn run_app<A: Hooks>(manifest_src: &str) -> anyhow::Result<()> {
    let manifest = ManifestLoader::load_from_str(manifest_src);
    let m = manifest.manifest();
    let logging = m.logging_or_default(edgezero_core::app::AXUM_ADAPTER);
    let kv_init_requirement = kv_init_requirement(m);
    let kv_store_name = m
        .kv_store_name(edgezero_core::app::AXUM_ADAPTER)
        .to_string();
    let kv_path = kv_store_path(&kv_store_name);
    let has_secret_store = m.secret_store_enabled("axum");

    let level: LevelFilter = logging.level.into();
    let level = if logging.echo_stdout.unwrap_or(true) {
        level
    } else {
        LevelFilter::Off
    };

    let _logger_init = SimpleLogger::new().with_level(level).init();

    let app = A::build_app();
    let router = app.router().clone();
    let runtime = RuntimeBuilder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move {
        let config = AxumDevServerConfig::default();
        let listener = StdTcpListener::bind(config.addr)
            .with_context(|| format!("failed to bind dev server to {}", config.addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set listener to non-blocking")?;
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("failed to adopt std listener into tokio")?;

        let kv_handle = match kv_handle_from_path(&kv_path) {
            Ok(handle) => Some(handle),
            Err(err) => {
                match kv_init_requirement {
                    KvInitRequirement::Optional => {
                        log::warn!(
                            "KV store '{}' could not be initialized at {}: {}",
                            kv_store_name,
                            kv_path.display(),
                            err
                        );
                        None
                    }
                    KvInitRequirement::Required => {
                        return Err(err.context(format!(
                            "KV store '{}' is explicitly configured for axum but could not be initialized at {}",
                            kv_store_name,
                            kv_path.display()
                        )));
                    }
                }
            }
        };
        // Axum always resolves the config store from the manifest only.
        // Unlike Fastly and Cloudflare, it does not check A::config_store() first.
        // If a user implements Hooks::config_store() without a [stores.config] section
        // in edgezero.toml, the override is silently ignored on Axum.
        if A::config_store().is_some() && m.stores.config.is_none() {
            log::warn!("A::config_store() is set but [stores.config] is missing in the manifest. This override is ignored on Axum.");
        }
        let config_store_handle = m.stores.config.as_ref().map(|cfg| {
            let defaults = cfg.config_store_defaults().clone();
            let store = AxumConfigStore::from_env(defaults);
            ConfigStoreHandle::new(std::sync::Arc::new(store))
        });
        let secret = has_secret_store.then(||  { log::info!("Secret store: reading from environment variables"); SecretHandle::new(std::sync::Arc::new(
                crate::secret_store::EnvSecretStore::new(),
            )) });
        let stores = Stores {
            config_store: config_store_handle,
            kv: kv_handle,
            secrets: secret,
        };
        serve_with_stores(router, listener, config.enable_ctrl_c, stores).await
    })
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
    fn default_store_name_uses_legacy_kv_path() {
        assert_eq!(
            kv_store_path(edgezero_core::manifest::DEFAULT_KV_STORE_NAME),
            PathBuf::from(".edgezero/kv.redb")
        );
    }

    #[test]
    fn implicit_default_kv_is_optional() {
        let manifest = ManifestLoader::load_from_str("");
        assert_eq!(
            kv_init_requirement(manifest.manifest()),
            KvInitRequirement::Optional
        );
    }

    #[test]
    fn explicit_kv_config_is_required() {
        let manifest = ManifestLoader::load_from_str(
            r#"
[stores.kv]
name = "EDGEZERO_KV"
"#,
        );
        assert_eq!(
            kv_init_requirement(manifest.manifest()),
            KvInitRequirement::Required
        );
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
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use edgezero_core::action;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
    use edgezero_core::extractor::Secrets;
    use edgezero_core::router::RouterService;
    use std::time::{Duration, Instant};

    struct TestServer {
        base_url: String,
        handle: tokio::task::JoinHandle<()>,
        _temp_dir: tempfile::TempDir,
    }

    async fn start_test_server(router: RouterService) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
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

            tokio::time::sleep(Duration::from_millis(10)).await;
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
        let response = send_with_retry(&client, |client| client.get(url.as_str())).await;

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
        let response = send_with_retry(&client, |client| client.get(url.as_str())).await;

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
        let response = send_with_retry(&client, |client| client.get(url.as_str())).await;

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
                .and_then(|v| v.to_str().ok())
                .unwrap_or("missing");
            Ok(value.to_string())
        }

        let router = RouterService::builder().get("/headers", handler).build();
        let server = start_test_server(router).await;

        let client = reqwest::Client::new();
        let url = format!("{}/headers", server.base_url);
        let response = send_with_retry(&client, |client| {
            client.get(url.as_str()).header("x-custom", "my-value")
        })
        .await;

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "my-value");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_fails_to_bind_to_used_port() {
        // First bind to a port
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind first");
        let addr = listener.local_addr().expect("listener addr");

        // Try to start server on same port
        let router = RouterService::builder().build();
        let config = AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
        };
        let server = AxumDevServer::with_config(router, config);

        // Run in blocking mode to capture the error
        let result = tokio::task::spawn_blocking(move || server.run()).await;

        match result {
            Ok(Err(e)) => {
                let err_str = e.to_string();
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
            let store = ctx.kv_handle().expect("kv configured");
            store.put("counter", &42_i32).await?;
            Ok("written")
        }

        async fn read_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let store = ctx.kv_handle().expect("kv configured");
            let val: i32 = store.get_or("counter", 0).await?;
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
        let response = send_with_retry(&client, |client| client.post(write_url.as_str())).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "written");

        // Read it back — proves shared state across requests
        let read_url = format!("{}/read", server.base_url);
        let response = send_with_retry(&client, |client| client.get(read_url.as_str())).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "42");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_delete_across_requests() {
        async fn write_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
            kv.put("temp", &"to_delete").await?;
            Ok("written")
        }

        async fn delete_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
            kv.delete("temp").await?;
            Ok("deleted")
        }

        async fn check_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
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
        let url = format!("{}/write", server.base_url);
        send_with_retry(&client, |c| c.post(url.as_str())).await;

        // Verify exists
        let url = format!("{}/check", server.base_url);
        let resp = send_with_retry(&client, |c| c.get(url.as_str())).await;
        assert_eq!(resp.text().await.unwrap(), "exists=true");

        // Delete
        let url = format!("{}/delete", server.base_url);
        send_with_retry(&client, |c| c.post(url.as_str())).await;

        // Verify gone
        let url = format!("{}/check", server.base_url);
        let resp = send_with_retry(&client, |c| c.get(url.as_str())).await;
        assert_eq!(resp.text().await.unwrap(), "exists=false");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_update_across_requests() {
        async fn increment_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
            let val = kv.read_modify_write("counter", 0_i32, |n| n + 1).await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder()
            .post("/inc", increment_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();
        let url = format!("{}/inc", server.base_url);

        // Increment 5 times, each should return incremented value
        for expected in 1..=5_i32 {
            let resp = send_with_retry(&client, |c| c.post(url.as_str())).await;
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
            let kv = ctx.kv_handle().expect("kv configured");
            let val: i32 = kv.get_or("nonexistent", -1).await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder().get("/read", read_handler).build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();

        let url = format!("{}/read", server.base_url);
        let resp = send_with_retry(&client, |c| c.get(url.as_str())).await;
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "-1");

        server.handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_store_handles_typed_data() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct UserProfile {
            name: String,
            age: u32,
            active: bool,
        }

        async fn write_handler(ctx: RequestContext) -> Result<&'static str, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
            let profile = UserProfile {
                name: "Alice".to_string(),
                age: 30,
                active: true,
            };
            kv.put("user:alice", &profile).await?;
            Ok("saved")
        }

        async fn read_handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let kv = ctx.kv_handle().expect("kv configured");
            let profile: Option<UserProfile> = kv.get("user:alice").await?;
            match profile {
                Some(p) => Ok(format!("{}:{}", p.name, p.age)),
                None => Ok("not found".to_string()),
            }
        }

        let router = RouterService::builder()
            .post("/save", write_handler)
            .get("/load", read_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();

        // Save profile
        let url = format!("{}/save", server.base_url);
        let resp = send_with_retry(&client, |c| c.post(url.as_str())).await;
        assert_eq!(resp.text().await.unwrap(), "saved");

        // Load profile
        let url = format!("{}/load", server.base_url);
        let resp = send_with_retry(&client, |c| c.get(url.as_str())).await;
        assert_eq!(resp.text().await.unwrap(), "Alice:30");

        server.handle.abort();
    }

    // -----------------------------------------------------------------------
    // Secret store helpers
    // -----------------------------------------------------------------------

    struct TestServerSecrets {
        base_url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    async fn start_test_server_with_secret_handle(
        router: RouterService,
        secret_handle: Option<edgezero_core::secret_store::SecretHandle>,
    ) -> TestServerSecrets {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind secrets test server");
        let addr = listener.local_addr().expect("local addr");
        let config = super::AxumDevServerConfig {
            addr,
            enable_ctrl_c: false,
        };
        let mut server = super::AxumDevServer::with_config(router, config);
        if let Some(h) = secret_handle {
            server = server.with_secret_handle(h);
        }
        let handle = tokio::spawn(async move {
            let _result = server.run_with_listener(listener).await;
        });
        TestServerSecrets {
            base_url: format!("http://{addr}"),
            handle,
        }
    }

    #[action]
    async fn secret_value_handler(Secrets(store): Secrets) -> Result<String, EdgeError> {
        store
            .require_str("test-store", "API_KEY")
            .await
            .map_err(EdgeError::from)
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
        let store =
            InMemorySecretStore::new([("test-store/API_KEY", bytes::Bytes::from("s3cr3t"))]);
        let handle = SecretHandle::new(Arc::new(store));
        let server = start_test_server_with_secret_handle(router, Some(handle)).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |c| c.get(url.as_str())).await;

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
        let store = InMemorySecretStore::new(std::iter::empty::<(&str, bytes::Bytes)>());
        let handle = SecretHandle::new(Arc::new(store));
        let server = start_test_server_with_secret_handle(router, Some(handle)).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |c| c.get(url.as_str())).await;

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
        let server = start_test_server_with_secret_handle(router, None).await;

        let client = reqwest::Client::new();
        let url = format!("{}/secret", server.base_url);
        let response = send_with_retry(&client, |c| c.get(url.as_str())).await;

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
