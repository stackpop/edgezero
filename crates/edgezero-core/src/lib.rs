//! Core primitives for building portable edge workloads across edge adapters.

pub mod app;
pub mod body;
pub mod compression;
pub mod context;
pub mod error;
pub mod extractor;
pub mod handler;
pub mod http;
pub mod manifest;
pub mod middleware;
pub mod params;
pub mod proxy;
pub mod responder;
pub mod response;
pub mod router;

pub use edgezero_macros::{action, app};
