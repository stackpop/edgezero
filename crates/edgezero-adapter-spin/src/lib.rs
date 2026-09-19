//! Adapter helpers for Spin (Fermyon).

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use anyhow::Context as _;

#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod cli;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub mod config_store;
pub mod context;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod key_value_store;
// `kv_pagination` is the pure paging logic for `SpinKvStore::list_keys_page`.
// It is host-compilable so its tests run under `cargo test`, while the wasm32
// `SpinKvStore` is the production consumer.
mod kv_pagination;
#[cfg(any(
    test,
    feature = "test-utils",
    all(feature = "spin", target_arch = "wasm32")
))]
pub mod outbound;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", any(test, target_arch = "wasm32")))]
pub mod response;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod secret_store;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::future::Future;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::pin::Pin;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::app::{App, Hooks};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::env_config::EnvConfig;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::http::Request as SpinRequest;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::wasip3::http::types::Response as WasiResponse;

/// Raw `WASIp3` response whose body and transmission lifetime remain owned by `EdgeZero`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub type SpinResponse = WasiResponse;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
fn build_app_for_dispatch<A: Hooks>() -> anyhow::Result<App> {
    A::build_app().context("application configuration failed")
}

/// Test seam for the production application-assembly error mapping.
#[cfg(all(feature = "test-utils", feature = "spin", target_arch = "wasm32"))]
#[doc(hidden)]
pub fn build_app_for_test<A: Hooks>() -> anyhow::Result<App> {
    build_app_for_dispatch::<A>()
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub trait AppExt {
    /// Dispatch a Spin request and return the raw response backed by `EdgeZero`'s owned writer.
    fn dispatch<'app>(
        &'app self,
        req: SpinRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinResponse>> + 'app>>;
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl AppExt for App {
    #[inline]
    fn dispatch<'app>(
        &'app self,
        req: SpinRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinResponse>> + 'app>> {
        Box::pin(request::dispatch(self, req))
    }
}

/// Initialize the logger for Spin.
///
/// Currently a no-op — Spin manages its own logging internally.
/// When a real logger is needed for one target, split this into
/// `#[cfg(all(feature = "spin", target_arch = "wasm32"))]` /
/// `#[cfg(not(...))]` branches following the Fastly/Cloudflare pattern.
// TODO: wire in real Spin logger when available
///
/// # Errors
/// Never; this is currently a no-op because Spin manages logging
/// internally. The signature still returns [`log::SetLoggerError`] so
/// the future "wire in a real logger" branch stays drop-in compatible.
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the `EdgeZero` router, and return the
/// response.
///
/// Portable store config is baked into `A` by the `app!` macro; the KV store
/// label is resolved at runtime from `EDGEZERO__STORES__KV__<ID>__NAME`. No
/// `edgezero.toml` is required.
///
/// Usage in a Spin component:
///
/// ```ignore
/// use spin_sdk::http_service;
/// use my_core::App;
///
/// #[http_service]
/// async fn handle(
///     req: spin_sdk::http::Request,
/// ) -> anyhow::Result<edgezero_adapter_spin::SpinResponse> {
///     edgezero_adapter_spin::run_app::<App>(req).await
/// }
/// ```
///
/// Returns the concrete raw [`SpinResponse`]. Its body writer and transmission-result observer
/// remain owned by the adapter's spawned response-egress coordinator.
///
/// # Errors
/// Returns [`anyhow::Error`] when the inner dispatch fails — transport,
/// router, store binding, or response translation errors propagate here.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub async fn run_app<A: Hooks>(req: SpinRequest) -> anyhow::Result<SpinResponse> {
    let app = build_app_for_dispatch::<A>()?;
    // Best-effort: every Spin `#[http_service]` re-enters this function, so a
    // second `log::set_logger` call returns Err — drop the result instead of
    // `.expect()` to avoid panicking on every subsequent request. Skipped
    // entirely when the app owns logging.
    if !A::owns_logging() {
        drop(init_logger());
    }
    let env = EnvConfig::from_env();
    let stores = A::stores();
    request::dispatch_with_registries(&app, req, stores.config, stores.kv, stores.secrets, &env)
        .await
}
