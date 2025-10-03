//! Axum adapter for AnyEdge routers and applications.

#[cfg(feature = "axum")]
mod server;

#[cfg(feature = "axum")]
pub use server::{AnyEdgeAxumService, AxumDevServer, AxumDevServerConfig};

#[cfg(feature = "cli")]
pub mod cli;
