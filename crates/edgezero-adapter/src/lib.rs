mod registry;

pub use registry::{get_adapter, register_adapter, registered_adapters, Adapter, AdapterAction};

pub mod scaffold;

#[cfg(feature = "cli")]
pub mod cli_support;
