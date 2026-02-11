#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

#[cfg(target_arch = "wasm32")]
use app_demo_core::App;
#[cfg(target_arch = "wasm32")]
use fastly::{Error, Request, Response};
#[cfg(target_arch = "wasm32")]
#[fastly::main]
pub fn main(req: Request) -> Result<Response, Error> {
    edgezero_adapter_fastly::run_app::<App>(include_str!("../../../edgezero.toml"), req)
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("app-demo-adapter-fastly: target wasm32-wasip1 to run on Fastly.");
}
