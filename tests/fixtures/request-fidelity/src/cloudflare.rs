use crate::Fixture;
use edgezero_adapter_cloudflare::request::CloudflareService;
use edgezero_core::app::Hooks;
use worker::{Context, Env, Request, Response, Result};

#[worker::event(fetch)]
async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    CloudflareService::new(&Fixture::build_app())
        .dispatch(req, env, ctx)
        .await
}
