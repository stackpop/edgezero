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
    async fn run_with_listener(self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        let AxumDevServer { router, config } = self;
        serve_with_listener(router, listener, config.enable_ctrl_c).await
    }
}

async fn serve_with_listener(
    router: RouterService,
    listener: tokio::net::TcpListener,
    enable_ctrl_c: bool,
) -> anyhow::Result<()> {
    let service = EdgeZeroAxumService::new(router);
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

        let handle = tokio::spawn(async move {
            let _ = server.run_with_listener(listener).await;
        });

        TestServer {
            base_url: format!("http://{}", addr),
            handle,
        }
    }

    async fn send_with_retry<F>(
        client: &reqwest::Client,
        mut make_request: F,
    ) -> reqwest::Response
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
}
