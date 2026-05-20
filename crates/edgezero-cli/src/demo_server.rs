#![cfg(feature = "edgezero-adapter-axum")]

use std::net::SocketAddr;

use edgezero_adapter_axum::dev_server::{AxumDevServer, AxumDevServerConfig};
use edgezero_core::router::RouterService;

#[cfg(not(feature = "dev-example"))]
use edgezero_core::{action, extractor::Path, response::Text};

#[cfg(feature = "dev-example")]
use app_demo_core::App;
#[cfg(feature = "dev-example")]
use edgezero_core::app::Hooks as _;

#[cfg(not(feature = "dev-example"))]
#[derive(serde::Deserialize)]
struct EchoParams {
    name: String,
}

/// Run the bundled example app locally on the axum demo server.
///
/// This always runs the built-in example — it does **not** read
/// `edgezero.toml` or delegate to a project's axum adapter. To run your
/// own project's axum adapter, use `edgezero serve --adapter axum`.
///
/// Returns `Ok(())` on graceful shutdown, `Err` on startup failure.
pub fn run_demo() -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8787));
    log::info!(
        "[edgezero] demo: starting example server on http://{}:{}",
        addr.ip(),
        addr.port()
    );

    let router = build_demo_router();
    let config = AxumDevServerConfig {
        addr,
        ..AxumDevServerConfig::default()
    };

    let server = AxumDevServer::with_config(router, config);
    server
        .run()
        .map_err(|err| format!("demo server error: {err}"))
}

fn build_demo_router() -> RouterService {
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
        .get("/", demo_root)
        .get("/echo/{name}", demo_echo)
        .build()
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn demo_root() -> Text<&'static str> {
    Text::new("EdgeZero demo server")
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn demo_echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("hello {}", params.name))
}
