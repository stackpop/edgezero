use std::net::{SocketAddr, TcpListener as StdTcpListener};

use anyhow::Context;
use axum::Router;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
use tower::{service_fn, Service};

use edgezero_core::app::Hooks;
use edgezero_core::manifest::ManifestLoader;
use edgezero_core::router::RouterService;
use log::LevelFilter;
use simple_logger::SimpleLogger;

use crate::service::EdgeZeroAxumService;

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

/// Blocking dev server runner used by the EdgeZero CLI.
pub struct AxumDevServer {
    router: RouterService,
    config: AxumDevServerConfig,
}

impl AxumDevServer {
    pub fn new(router: RouterService) -> Self {
        Self {
            router,
            config: AxumDevServerConfig::default(),
        }
    }

    pub fn with_config(router: RouterService, config: AxumDevServerConfig) -> Self {
        Self { router, config }
    }

    pub fn run(self) -> anyhow::Result<()> {
        let runtime = RuntimeBuilder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime")?;

        runtime.block_on(async move { self.run_async().await })
    }

    async fn run_async(self) -> anyhow::Result<()> {
        let AxumDevServer { router, config } = self;

        // Allow binding to already-open listener if caller created one to surface errors early.
        let listener = StdTcpListener::bind(config.addr)
            .with_context(|| format!("failed to bind dev server to {}", config.addr))?;
        listener
            .set_nonblocking(true)
            .context("failed to set listener to non-blocking")?;

        let listener = tokio::net::TcpListener::from_std(listener)
            .context("failed to adopt std listener into tokio")?;

        serve_with_listener(router, listener, config.enable_ctrl_c).await
    }

    #[cfg(test)]
    async fn run_with_listener(self, listener: tokio::net::TcpListener, kv_path: &str) -> anyhow::Result<()> {
        let AxumDevServer { router, config } = self;
        serve_with_listener_and_kv_path(router, listener, config.enable_ctrl_c, kv_path).await
    }
}

async fn serve_with_listener(
    router: RouterService,
    listener: tokio::net::TcpListener,
    enable_ctrl_c: bool,
) -> anyhow::Result<()> {
    serve_with_listener_and_kv_path(router, listener, enable_ctrl_c, ".edgezero/kv.redb").await
}

async fn serve_with_listener_and_kv_path(
    router: RouterService,
    listener: tokio::net::TcpListener,
    enable_ctrl_c: bool,
    kv_path: &str,
) -> anyhow::Result<()> {
    // Create a persistent KV store
    if let Some(parent) = std::path::Path::new(kv_path).parent() {
        std::fs::create_dir_all(parent)
            .context("failed to create KV store directory")?;
    }
    let kv_store = std::sync::Arc::new(
        crate::kv::PersistentKvStore::new(kv_path)
            .context("failed to create KV store")?,
    );
    let kv_handle = edgezero_core::kv::KvHandle::new(kv_store);

    let service = EdgeZeroAxumService::new(router).with_kv_handle(kv_handle);
    let router = Router::new().fallback_service(service_fn(move |req| {
        let mut svc = service.clone();
        async move { svc.call(req).await }
    }));
    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();

    let shutdown = if enable_ctrl_c {
        Some(async {
            let _ = signal::ctrl_c().await;
        })
    } else {
        None
    };

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
    let logging = manifest.manifest().logging_or_default("axum");

    let level: LevelFilter = logging.level.into();
    let level = if logging.echo_stdout.unwrap_or(true) {
        level
    } else {
        LevelFilter::Off
    };

    SimpleLogger::new().with_level(level).init().ok();

    let app = A::build_app();
    let router = app.router().clone();

    AxumDevServer::new(router).run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn default_config_uses_expected_address() {
        let config = AxumDevServerConfig::default();
        assert_eq!(config.addr.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
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
        assert_eq!(config.addr.ip(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
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
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use edgezero_core::context::RequestContext;
    use edgezero_core::error::EdgeError;
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
        let server = AxumDevServer::with_config(router, config);

        // Use a unique temp directory for each test server
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let kv_path = temp_dir.path().join("kv.redb");
        let kv_path_str = kv_path.to_str().expect("valid path").to_string();

        let handle = tokio::spawn(async move {
            let _ = server.run_with_listener(listener, &kv_path_str).await;
        });

        TestServer {
            base_url: format!("http://{}", addr),
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
                    if start.elapsed() >= timeout {
                        panic!("server did not respond before timeout: {}", err);
                    }
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
                    "expected bind error, got: {}",
                    err_str
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
            store.put("counter", &42i32).await?;
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
            let val = kv.update("counter", 0i32, |n| n + 1).await?;
            Ok(val.to_string())
        }

        let router = RouterService::builder()
            .post("/inc", increment_handler)
            .build();
        let server = start_test_server(router).await;
        let client = reqwest::Client::new();
        let url = format!("{}/inc", server.base_url);

        // Increment 5 times, each should return incremented value
        for expected in 1..=5i32 {
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
}
