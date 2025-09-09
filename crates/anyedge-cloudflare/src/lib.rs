//! Cloudflare Workers adapter for AnyEdge.
//!
//! Usage (in your Workers project using the `worker` crate):
//!
//! ```ignore
//! use anyedge_cloudflare as aecf;
//! use worker::*;
//!
//! async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
//!     let app = build_app();
//!     aecf::handle(&app, req, env, ctx).await
//! }
//! ```

#[cfg(feature = "workers")]
pub mod app;
#[cfg(feature = "workers")]
pub mod http;
pub mod proxy;
#[cfg(not(feature = "workers"))]
mod stub;

#[cfg(feature = "workers")]
pub use app::handle;
#[cfg(not(feature = "workers"))]
pub use stub::handle;
