//! Axum adapter for `EdgeZero` routers and applications.

#[cfg(feature = "axum")]
pub mod config_store;
#[cfg(feature = "axum")]
pub mod context;
#[cfg(feature = "axum")]
pub mod dev_server;
#[cfg(feature = "axum")]
pub mod key_value_store;
#[cfg(feature = "axum")]
pub mod proxy;
#[cfg(feature = "axum")]
pub mod request;
#[cfg(feature = "axum")]
pub mod response;
#[cfg(feature = "axum")]
pub mod secret_store;
#[cfg(feature = "axum")]
pub mod service;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(test)]
pub mod test_utils;
