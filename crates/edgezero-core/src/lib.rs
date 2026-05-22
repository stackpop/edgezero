//! Core primitives for building portable edge workloads across edge adapters.

// Targets a single line — the proc-macro re-export at the bottom of this
// file. The `pub_use` lint is module-scoped (cannot be `#[expect]`-ed
// per-item), and proc-macros must be re-exported here so downstream users
// depend only on `edgezero-core` (not `edgezero-macros`).
#![expect(
    clippy::pub_use,
    reason = "proc-macros must be re-exported through the parent crate"
)]

pub mod addr;
pub mod app;
pub mod body;
pub mod compression;
pub mod config_store;
pub mod context;
pub mod env_config;
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

pub use edgezero_macros::{action, app};
