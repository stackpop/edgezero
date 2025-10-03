#![cfg(feature = "anyedge-adapter-axum")]

use std::net::SocketAddr;

use anyedge_adapter_axum::{AxumDevServer, AxumDevServerConfig};
use anyedge_core::router::RouterService;

#[cfg(not(feature = "dev-example"))]
use anyedge_core::{action, extractor::Path, responder::Text};

#[cfg(feature = "dev-example")]
use anyedge_core::app::Hooks;
#[cfg(feature = "dev-example")]
use app_demo_core::App;

pub fn run_dev() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8787));
    println!(
        "[anyedge] dev: starting local server on http://{}:{}",
        addr.ip(),
        addr.port()
    );

    let router = build_dev_router();
    let config = AxumDevServerConfig {
        addr,
        ..AxumDevServerConfig::default()
    };

    let server = AxumDevServer::with_config(router, config);
    if let Err(err) = server.run() {
        eprintln!("[anyedge] dev server error: {err}");
    }
}

fn build_dev_router() -> RouterService {
    #[cfg(feature = "dev-example")]
    {
        let demo_app = App::build_app();
        demo_app.router().clone()
    }

    #[cfg(not(feature = "dev-example"))]
    {
        default_router()
    }
}

#[cfg(not(feature = "dev-example"))]
fn default_router() -> RouterService {
    RouterService::builder()
        .get("/", dev_root)
        .get("/echo/{name}", dev_echo)
        .build()
}

#[cfg(not(feature = "dev-example"))]
#[derive(serde::Deserialize)]
struct EchoParams {
    name: String,
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn dev_root() -> Text<&'static str> {
    Text::new("AnyEdge dev server")
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn dev_echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("hello {}", params.name))
}
