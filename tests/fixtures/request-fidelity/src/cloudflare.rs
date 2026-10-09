use crate::Fixture;
use edgezero_adapter_cloudflare::request::CloudflareService;
use edgezero_core::action;
use edgezero_core::app::{App, Hooks};
use edgezero_core::context::RequestContext;
use edgezero_core::request::{CapturedTarget, Preservation, RequestIngress, TargetSource};
use edgezero_core::router::RouterService;
use serde_json::json;
use worker::{Context, Env, Request, Response, Result};

#[worker::event(fetch)]
async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let app = if matches!(
        req.headers().get("x-fixture")?.as_deref(),
        Some("route-unicode" | "route-backslash")
    ) {
        App::new(
            RouterService::builder()
                .get("/caf%C3%A9", normalized_route)
                .get("/a/b", normalized_route)
                .build(),
        )
    } else {
        Fixture::build_app()
    };
    CloudflareService::new(&app).dispatch(req, env, ctx).await
}

#[action]
async fn normalized_route(ctx: RequestContext) -> String {
    let (raw_target, normalized_uri) = match ctx
        .request()
        .headers()
        .get("x-fixture")
        .and_then(|value| value.to_str().ok())
    {
        Some("route-unicode") => ("http://example.com/café", "http://example.com/caf%C3%A9"),
        Some("route-backslash") => ("http://example.com/a\\b", "http://example.com/a/b"),
        _ => ("", ""),
    };
    let target = ctx
        .request()
        .extensions()
        .get::<RequestIngress>()
        .map(RequestIngress::target);
    json!({
        "ordinary": true,
        "primary_uri_normalized": *ctx.request().uri() == normalized_uri,
        "raw_target_preserved": matches!(target, Some(CapturedTarget::Complete(target))
            if target.value() == raw_target
                && target.source() == TargetSource::RuntimeUrl
                && target.fidelity() == Preservation::Unknown),
    })
    .to_string()
}
