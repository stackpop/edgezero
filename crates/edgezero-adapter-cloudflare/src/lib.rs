//! Adapter helpers for Cloudflare Workers.

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod context;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod kv;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod proxy;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod request;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod response;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use context::CloudflareRequestContext;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use proxy::CloudflareProxyClient;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use request::{dispatch, dispatch_with_kv, into_core_request, DEFAULT_KV_BINDING};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use response::from_core_response;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

#[cfg(not(all(feature = "cloudflare", target_arch = "wasm32")))]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub trait AppExt {
    fn dispatch<'a>(
        &'a self,
        req: worker::Request,
        env: worker::Env,
        ctx: worker::Context,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = Result<worker::Response, worker::Error>> + 'a>,
    >;
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl AppExt for edgezero_core::app::App {
    fn dispatch<'a>(
        &'a self,
        req: worker::Request,
        env: worker::Env,
        ctx: worker::Context,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = Result<worker::Response, worker::Error>> + 'a>,
    > {
        Box::pin(crate::request::dispatch(self, req, env, ctx))
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let app = A::build_app();
    dispatch(&app, req, env, ctx).await
}

/// Like [`run_app`], but reads the KV binding name from the manifest.
///
/// Users who are happy with the default `EDGEZERO_KV` binding can keep
/// using [`run_app`]. Call this variant when you have a `[stores.kv]`
/// section in `edgezero.toml`:
///
/// ```rust,ignore
/// run_app_with_manifest::<App>(include_str!("../../edgezero.toml"), req, env, ctx).await
/// ```
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app_with_manifest<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let kv_binding = manifest_loader.manifest().kv_store_name("cloudflare");
    let app = A::build_app();
    dispatch_with_kv(&app, req, env, ctx, kv_binding).await
}
