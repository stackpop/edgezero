// Note: even when targeting wasm32-wasip1, `target_os` remains `wasi`.
#[cfg(target_arch = "wasm32")]
use app_demo_core::DemoApp;
#[cfg(target_arch = "wasm32")]
use fastly::{Error, Request, Response};
#[cfg(target_arch = "wasm32")]
use log::LevelFilter;

#[cfg(target_arch = "wasm32")]
#[fastly::main]
pub fn main(req: Request) -> Result<Response, Error> {
    let app = DemoApp::build_app();
    anyedge_adapter_fastly::init_logger("stdout", LevelFilter::Info, true)
        .expect("init fastly logger");
    anyedge_adapter_fastly::dispatch(&app, req)
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("app-demo-adapter-fastly: target wasm32-wasip1 to run on Fastly.");
}
