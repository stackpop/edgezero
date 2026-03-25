//! Adapter helpers for Cloudflare Workers.

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod config_store;
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
pub use config_store::CloudflareConfigStore;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use context::CloudflareRequestContext;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use proxy::CloudflareProxyClient;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[allow(deprecated)]
pub use request::{
    dispatch, dispatch_with_config, dispatch_with_config_handle, dispatch_with_kv,
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
    #[deprecated(
        note = "AppExt::dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_handle()"
    )]
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
    #[allow(deprecated)]
    fn dispatch<'a>(
        &'a self,
        req: worker::Request,
        env: worker::Env,
        ctx: worker::Context,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = Result<worker::Response, worker::Error>> + 'a>,
    > {
        Box::pin(crate::request::dispatch_raw(self, req, env, ctx))
    }
}

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
    let kv_binding = manifest.kv_store_name(edgezero_core::app::CLOUDFLARE_ADAPTER);
    let kv_required = manifest.stores.kv.is_some();
    // Two-path resolution: `A::config_store()` is set at compile time by the
    // `#[app]` macro and is the common case. The manifest fallback handles
    // callers that implement `Hooks` manually without the macro — in that case
    // `A::config_store()` returns `None` while `[stores.config]` in
    // `edgezero.toml` may still be present.
    let config_binding = A::config_store()
        .map(|cfg| cfg.name_for_adapter(edgezero_core::app::CLOUDFLARE_ADAPTER))
        .or_else(|| {
            manifest
                .stores
                .config
                .as_ref()
                .map(|cfg| cfg.config_store_name(edgezero_core::app::CLOUDFLARE_ADAPTER))
        });
    let app = A::build_app();
    crate::request::dispatch_with_bindings(
        &app,
        req,
        env,
        ctx,
        config_binding,
        kv_binding,
        kv_required,
    )
    .await
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
