// Note: even when targeting wasm32-wasip1, `target_os` remains `wasi`.
#[cfg(target_arch = "wasm32")]
use anyedge_controller::Hooks;
#[cfg(target_arch = "wasm32")]
use app_demo_core::DemoApp;
#[cfg(target_arch = "wasm32")]
use fastly::{Error, Request, Response};

#[cfg(target_arch = "wasm32")]
#[fastly::main]
pub fn main(req: Request) -> Result<Response, Error> {
    let app = DemoApp::build_app();
    anyedge_fastly::init_logger("demo", log::LevelFilter::Info, true).expect("init fastly logger");
    Ok(anyedge_fastly::handle(&app, req))
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("app-demo-fastly: target wasm32-wasip1 to run on Fastly.");
}
