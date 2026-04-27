//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

pub mod context;
mod decompress;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod response;

/// Initialize the logger for Spin.
///
/// Currently a no-op — Spin manages its own logging internally.
/// When a real logger is needed for one target, split this into
/// `#[cfg(all(feature = "spin", target_arch = "wasm32"))]` /
/// `#[cfg(not(...))]` branches following the Fastly/Cloudflare pattern.
// TODO: wire in real Spin logger when available
/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

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

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the EdgeZero router, and return the
/// response.
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
///
/// **Note:** Config store, KV store, and secret store support are not yet
/// implemented for the Spin adapter. The `[stores.config]`, `[stores.kv]`,
/// and `[stores.secrets]` manifest sections are intentionally rejected for
/// the `spin` adapter. See the manifest validation error for details.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: spin_sdk::http::IncomingRequest,
) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
    // Use `let _ =` instead of `.expect()` because Spin calls
    // `#[http_component]` per-request. Once a real logger is wired in,
    // `log::set_logger` returns Err on the second call — `.expect()`
    // would panic on every subsequent request.
    let _ = init_logger();
    let app = A::build_app();
    request::dispatch(&app, req).await
}
