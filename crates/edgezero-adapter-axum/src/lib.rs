//! Axum adapter for EdgeZero routers and applications.

#[cfg(feature = "axum")]
mod context;
#[cfg(feature = "axum")]
mod dev_server;
#[cfg(feature = "axum")]
mod proxy;
#[cfg(feature = "axum")]
mod request;
#[cfg(feature = "axum")]
mod response;
#[cfg(feature = "axum")]
mod service;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(feature = "axum")]
pub use context::AxumRequestContext;
#[cfg(feature = "axum")]
pub use dev_server::{run_app, AxumDevServer, AxumDevServerConfig};
#[cfg(feature = "axum")]
pub use proxy::AxumProxyClient;
#[cfg(feature = "axum")]
pub use request::into_core_request;
#[cfg(feature = "axum")]
pub use response::into_axum_response;
#[cfg(feature = "axum")]
pub use service::EdgeZeroAxumService;
