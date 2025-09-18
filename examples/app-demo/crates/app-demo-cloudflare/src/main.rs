use anyedge_controller::Hooks;
use app_demo_core::DemoApp;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let app = DemoApp::build_app();
    anyedge_cloudflare::handle(&app, req, env, ctx).await
}

#[cfg(not(all(target_arch = "wasm32")))]
fn main() {
    eprintln!("Run `wrangler dev` or target wasm32-unknown-unknown to execute this example.");
}
