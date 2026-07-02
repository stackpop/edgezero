#![expect(
    clippy::pub_use,
    reason = "crate-root re-exports for external callers; adapters + CLI read \
             `edgezero_adapter::TypeName` instead of `edgezero_adapter::registry::TypeName`"
)]

pub mod env_file;

pub mod registry;

pub mod scaffold;

#[cfg(feature = "cli")]
pub mod cli_support;

// Re-exports so adapters + the CLI can write
// `edgezero_adapter::TypeName` instead of
// `edgezero_adapter::registry::TypeName`. Mirrors the surface
// adapters already touch via `registry::*` imports today.
pub use registry::{
    get_adapter, Adapter, AdapterDeployedState, ProvisionMode, ProvisionOutcome, ProvisionStores,
    ResolvedStoreId, TypedSecretEntry,
};
