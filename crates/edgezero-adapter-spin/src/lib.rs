//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
mod config_store;
pub mod context;
mod decompress;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod response;

// SpinConfigStore is available without the `spin` feature flag because its
// production spin_sdk backend is feature-gated internally, allowing the
// InMemory test backend to compile on all targets. SpinKvStore and
// SpinSecretStore import spin_sdk types at the module level and therefore
// require `all(feature = "spin", target_arch = "wasm32")`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod key_value_store;
// `kv_pagination` is the pure paging logic for `SpinKvStore::list_keys_page`.
// It is host-compilable so its tests run under `cargo test`, while the wasm32
// `SpinKvStore` is the production consumer.
mod kv_pagination;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod secret_store;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use config_store::SpinConfigStore;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::env_config::EnvConfig;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use key_value_store::SpinKvStore;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use proxy::SpinProxyClient;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use request::{dispatch, dispatch_with_kv_label, into_core_request};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use response::from_core_response;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use secret_store::SpinSecretStore;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub trait AppExt {
    fn dispatch<'a>(
        &'a self,
        req: spin_sdk::http::IncomingRequest,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = anyhow::Result<spin_sdk::http::Response>> + 'a>,
    >;
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl AppExt for edgezero_core::app::App {
    fn dispatch<'a>(
        &'a self,
        req: spin_sdk::http::IncomingRequest,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = anyhow::Result<spin_sdk::http::Response>> + 'a>,
    > {
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
/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the EdgeZero router, and return the
/// response.
///
/// Portable store config is baked into `A` by the `app!` macro; the KV store
/// label is resolved at runtime from `EDGEZERO__STORES__KV__<ID>__NAME`. No
/// `edgezero.toml` is required.
///
/// Usage in a Spin component:
///
/// ```ignore
/// use spin_sdk::http_component;
/// use my_core::App;
///
/// #[http_component]
/// async fn handle(req: spin_sdk::http::IncomingRequest) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
///     edgezero_adapter_spin::run_app::<App>(req).await
/// }
/// ```
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: spin_sdk::http::IncomingRequest,
) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
    // Use `let _ =` instead of `.expect()` because Spin calls
    // `#[http_component]` per-request. Once a real logger is wired in,
    // `log::set_logger` returns Err on the second call — `.expect()`
    // would panic on every subsequent request.
    let _ = init_logger();
    let env = EnvConfig::from_env();
    let stores = A::stores();
    let app = A::build_app();
    request::dispatch_with_registries(&app, req, stores.config, stores.kv, stores.secrets, &env)
        .await
}
