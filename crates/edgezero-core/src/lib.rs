//! Core primitives for building portable edge workloads across edge adapters.

pub mod addr;
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

pub use config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
pub use edgezero_macros::{action, app};
#[cfg(any(test, feature = "test-utils"))]
pub use key_value_store::NoopKvStore;
pub use key_value_store::{KvError, KvHandle, KvPage, KvStore};
#[cfg(any(test, feature = "test-utils"))]
pub use secret_store::{InMemorySecretStore, NoopSecretStore};
pub use secret_store::{SecretError, SecretHandle, SecretStore};
