//! Adapter helpers for Cloudflare Workers.

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod context;
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
pub use request::{dispatch, into_core_request};
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
impl AppExt for anyedge_core::App {
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
