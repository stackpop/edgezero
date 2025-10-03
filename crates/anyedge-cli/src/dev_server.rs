#![cfg(feature = "anyedge-adapter-axum")]

use std::net::SocketAddr;
use std::path::PathBuf;

use anyedge_adapter_axum::{AxumDevServer, AxumDevServerConfig};
use anyedge_core::manifest::ManifestLoader;
use anyedge_core::router::RouterService;

use crate::adapter;
use crate::adapter::Action;

#[cfg(not(feature = "dev-example"))]
use anyedge_core::{action, extractor::Path, response::Text};

#[cfg(feature = "dev-example")]
use anyedge_core::app::Hooks;
#[cfg(feature = "dev-example")]
use app_demo_core::App;

pub fn run_dev() {
    match try_run_manifest_axum() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => eprintln!("[anyedge] dev manifest error: {err}"),
    }

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

fn try_run_manifest_axum() -> Result<bool, String> {
    let manifest = match load_manifest_optional()? {
        Some(manifest) => manifest,
        None => return Ok(false),
    };

    if manifest.manifest().adapters.contains_key("axum") {
        adapter::execute("axum", Action::Serve, Some(&manifest), &[])
            .map_err(|err| format!("serve command failed: {err}"))?;
        return Ok(true);
    }

    Ok(false)
}

fn load_manifest_optional() -> Result<Option<ManifestLoader>, String> {
    let path = std::env::var("ANYEDGE_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("anyedge.toml"));

    match ManifestLoader::from_path(&path) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to load {}: {err}", path.display())),
    }
}
