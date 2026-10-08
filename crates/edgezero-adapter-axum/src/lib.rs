//! Axum adapter for `EdgeZero` routers and applications.

#[cfg(feature = "axum")]
mod config_snapshot;
#[cfg(feature = "axum")]
pub mod config_store;
pub mod config_store_limits;
#[cfg(feature = "axum")]
mod connection;
#[cfg(feature = "axum")]
pub mod context;
#[cfg(feature = "axum")]
pub mod dev_server;
#[cfg(feature = "axum")]
mod header_deadline;
pub mod ingress_config;
#[cfg(feature = "axum")]
pub mod key_value_store;
#[cfg(all(feature = "native-bindings", not(target_arch = "wasm32")))]
pub mod native_bindings;
#[cfg(all(feature = "native-bindings", not(target_arch = "wasm32")))]
pub mod native_build;
#[cfg(feature = "axum")]
pub mod native_stores;
#[cfg(feature = "axum")]
pub mod outbound;
#[cfg(feature = "axum")]
pub mod request;
#[cfg(feature = "axum")]
mod response;
#[cfg(feature = "axum")]
mod run_options;
#[cfg(feature = "axum")]
pub mod secret_store;
#[cfg(feature = "axum")]
mod service;

#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod cli;

#[cfg(test)]
pub mod test_utils;

/// Native Axum resource limits and accounting are operator-configured.
pub const AXUM_PLATFORM: edgezero_core::PlatformMetadata = edgezero_core::PlatformMetadata::new(
    edgezero_core::PlatformFact::unknown(edgezero_core::PlatformUnknownReason::OperatorConfigured),
    edgezero_core::PlatformFact::unknown(edgezero_core::PlatformUnknownReason::OperatorConfigured),
    edgezero_core::PlatformFact::unknown(edgezero_core::PlatformUnknownReason::OperatorConfigured),
);
