//! Native AWS store preparation, independent of the HTTP adapter.
//!
//! Settings remain available without either service feature. Service failures
//! use closed categories; no SDK, HTTP, settings or payload source chain survives.

#![cfg_attr(
    all(
        not(target_arch = "wasm32"),
        any(feature = "appconfig-agent", feature = "secrets-manager")
    ),
    expect(
        clippy::pub_use,
        reason = "keep the provider's supported preparation API at the crate root"
    )
)]

pub mod settings;

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "appconfig-agent")]
mod appconfig_tests;

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "appconfig-agent", feature = "secrets-manager")
))]
mod preparation;
#[cfg(all(not(target_arch = "wasm32"), feature = "secrets-manager"))]
mod secrets_manager;
#[cfg(test)]
#[cfg(all(not(target_arch = "wasm32"), feature = "secrets-manager"))]
mod snapshot_tests;
#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "appconfig-agent", feature = "secrets-manager")
))]
pub use preparation::{AwsPreparation, PreparationStats};

/// Sanitized preparation failures. Never attach provider response details.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PreparationError {
    #[error("access denied")]
    AccessDenied,
    #[error("credential failure")]
    Credentials,
    #[error("preparation deadline")]
    Deadline,
    #[error("decryption failure")]
    Decryption,
    #[error("invalid binding")]
    InvalidBinding,
    #[error("invalid agent endpoint")]
    InvalidEndpoint,
    #[error("invalid preparation limits")]
    InvalidLimits,
    #[error("invalid resource reference")]
    InvalidResource,
    #[error("invalid version selector")]
    InvalidSelector,
    #[error("malformed value")]
    Malformed,
    #[error("missing resource or version")]
    Missing,
    #[error("size limit")]
    SizeLimit,
    #[error("throttled")]
    Throttled,
    #[error("provider unavailable")]
    Unavailable,
    #[error("provider not compiled")]
    UnsupportedProvider,
}
