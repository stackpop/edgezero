use anyedge_core::App;

use crate::http;

/// Handle a single Cloudflare Workers request with an AnyEdge `App`.
#[cfg(feature = "workers")]
pub async fn handle(
    app: &App,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> worker::Result<worker::Response> {
    let _ = (env, ctx); // currently unused; reserved for future features
    let areq = http::to_anyedge_request(req).await?;
    let ares = app.handle(areq);
    http::from_anyedge_response(ares)
}
