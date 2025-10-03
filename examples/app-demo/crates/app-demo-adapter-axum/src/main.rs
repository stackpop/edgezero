use anyedge_adapter_axum::{AxumDevServer, AxumDevServerConfig};
use anyedge_core::app::Hooks;
use anyhow::Context;
use app_demo_core::App;
use log::LevelFilter;

fn main() {
    if let Err(err) = run() {
        eprintln!("app-demo-adapter-axum failed: {err}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    simple_logger::SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()
        .ok();

    let app = App::build_app();
    let router = app.router().clone();

    let server = AxumDevServer::with_config(router, AxumDevServerConfig::default());
    server.run().context("dev server")
}
