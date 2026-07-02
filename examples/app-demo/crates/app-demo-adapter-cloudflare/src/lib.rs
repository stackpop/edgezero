#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
use app_demo_core::App;
#[cfg(target_arch = "wasm32")]
use worker::{event, Context, Env, Request, Response, Result};

/// Entrypoint invoked by Cloudflare Workers.
///
/// # Errors
/// Returns an error if `EdgeZero` or the Cloudflare adapter cannot build a
/// response.
#[cfg(target_arch = "wasm32")]
#[event(fetch)]
#[inline]
pub async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    edgezero_adapter_cloudflare::run_app::<App>(req, env, ctx).await
}
