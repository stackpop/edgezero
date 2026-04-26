//! Core primitives for building portable edge workloads across edge adapters.

pub mod app;
pub mod body;
pub mod compression;
pub mod config_store;
pub mod context;
pub mod error;
pub mod extractor;
pub mod handler;
pub mod http;
pub mod key_value_store;
pub mod manifest;
pub mod middleware;
pub mod params;
pub mod proxy;
pub mod responder;
pub mod response;
pub mod router;
pub mod secret_store;

// Proc macros must be re-exported through the parent crate so downstream
// users depend only on `edgezero-core` rather than on `edgezero-macros`
// directly. This is the canonical proc-macro distribution pattern.
pub use edgezero_macros::{action, app};
