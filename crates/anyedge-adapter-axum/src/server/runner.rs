use std::net::{SocketAddr, TcpListener as StdTcpListener};

use anyhow::Context;
use axum::Router;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;
use tower::{service_fn, Service};

use anyedge_core::app::Hooks;
use anyedge_core::manifest::ManifestLoader;
use anyedge_core::router::RouterService;
use log::LevelFilter;
use simple_logger::SimpleLogger;

use super::service::AnyEdgeAxumService;

/// Configuration used when running the dev server embedding AnyEdge into Axum.
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

/// Blocking dev server runner used by the AnyEdge CLI.
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

        let service = AnyEdgeAxumService::new(router);
        let router = Router::new().fallback_service(service_fn(move |req| {
            let mut svc = service.clone();
            async move { svc.call(req).await }
        }));

        let shutdown = if config.enable_ctrl_c {
            Some(async {
                let _ = signal::ctrl_c().await;
            })
        } else {
            None
        };

        let server = axum::serve(listener, router.into_make_service());
        if let Some(shutdown) = shutdown {
            let server = server.with_graceful_shutdown(shutdown);
            server.await.context("axum server error")?;
        } else {
            server.await.context("axum server error")?;
        }

        Ok(())
    }
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
