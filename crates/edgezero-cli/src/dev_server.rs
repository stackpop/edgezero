#![cfg(feature = "edgezero-adapter-axum")]

use std::net::SocketAddr;
use std::path::PathBuf;

use edgezero_adapter_axum::{AxumDevServer, AxumDevServerConfig};
use edgezero_core::manifest::ManifestLoader;
use edgezero_core::router::RouterService;

use crate::adapter;
use crate::adapter::Action;

#[cfg(not(feature = "dev-example"))]
use edgezero_core::{action, extractor::Path, response::Text};

#[cfg(feature = "dev-example")]
use app_demo_core::App;
#[cfg(feature = "dev-example")]
use edgezero_core::app::Hooks;

pub fn run_dev() {
    match try_run_manifest_axum() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => eprintln!("[edgezero] dev manifest error: {err}"),
    }

    let addr = resolve_dev_addr();
    println!(
        "[edgezero] dev: starting local server on http://{}:{}",
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
        eprintln!("[edgezero] dev server error: {err}");
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
    Text::new("EdgeZero dev server")
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn dev_echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("hello {}", params.name))
}

/// Resolve the dev server bind address from `EDGEZERO_HOST` / `EDGEZERO_PORT`
/// environment variables, falling back to `127.0.0.1:8787`.
fn resolve_dev_addr() -> SocketAddr {
    let env_host = std::env::var("EDGEZERO_HOST").ok();
    let env_port = std::env::var("EDGEZERO_PORT").ok();
    edgezero_core::addr::resolve_bind_addr(env_host.as_deref(), env_port.as_deref(), None, None)
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
    let path = std::env::var("EDGEZERO_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("edgezero.toml"));

    match ManifestLoader::from_path(&path) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to load {}: {err}", path.display())),
    }
}
