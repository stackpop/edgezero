//! Axum adapter for EdgeZero routers and applications.

#[cfg(feature = "axum")]
pub mod config_store;
#[cfg(feature = "axum")]
mod context;
#[cfg(feature = "axum")]
mod dev_server;
#[cfg(feature = "axum")]
pub mod key_value_store;
#[cfg(feature = "axum")]
mod proxy;
#[cfg(feature = "axum")]
mod request;
#[cfg(feature = "axum")]
mod response;
#[cfg(feature = "axum")]
pub mod secret_store;
#[cfg(feature = "axum")]
mod service;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(test)]
pub mod test_utils;

#[cfg(feature = "axum")]
pub use config_store::AxumConfigStore;
#[cfg(feature = "axum")]
pub use context::AxumRequestContext;
#[cfg(feature = "axum")]
pub use dev_server::{run_app, AxumDevServer, AxumDevServerConfig};
#[cfg(feature = "axum")]
pub use key_value_store::PersistentKvStore;
#[cfg(feature = "axum")]
pub use proxy::AxumProxyClient;
#[cfg(feature = "axum")]
pub use request::into_core_request;
#[cfg(feature = "axum")]
pub use response::into_axum_response;
#[cfg(feature = "axum")]
pub use secret_store::EnvSecretStore;
#[cfg(feature = "axum")]
pub use service::EdgeZeroAxumService;
