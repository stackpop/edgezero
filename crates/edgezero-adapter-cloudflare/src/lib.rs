//! Adapter helpers for Cloudflare Workers.

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod context;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod key_value_store;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod proxy;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod request;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod response;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod secret_store;
mod store_handles;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use context::CloudflareRequestContext;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use proxy::CloudflareProxyClient;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use request::{
    dispatch, dispatch_with_kv, dispatch_with_kv_and_secrets, dispatch_with_secrets,
    into_core_request, DEFAULT_KV_BINDING,
};
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

/// Entry point for a Cloudflare Workers application.
///
/// **Breaking change (pre-1.0):** `manifest_src` is now a required parameter.
/// Callers previously using `run_app_with_manifest` can rename to `run_app` —
/// the signatures are identical.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let manifest = manifest_loader.manifest();
    let kv_binding = manifest.kv_store_name("cloudflare");
    let kv_required = manifest.stores.kv.is_some();
    let secrets_required = manifest.secret_store_enabled("cloudflare");
    let app = A::build_app();
    let kv_handle = crate::request::resolve_kv_handle(&env, kv_binding, kv_required)?;
    let secret_handle = crate::request::resolve_secret_handle(&env, secrets_required);
    crate::request::dispatch_with_handles(&app, req, env, ctx, kv_handle, secret_handle).await
}

/// Deprecated: use [`run_app`] which now takes `manifest_src` directly.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[deprecated(note = "use run_app instead, which now takes manifest_src")]
pub async fn run_app_with_manifest<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    run_app::<A>(manifest_src, req, env, ctx).await
}
