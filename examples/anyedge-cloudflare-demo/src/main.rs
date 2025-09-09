use anyedge_app_lib::build_app;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let app = build_app();
    anyedge_cloudflare::handle(&app, req, env, ctx).await
}
