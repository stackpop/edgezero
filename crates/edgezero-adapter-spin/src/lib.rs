//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod context;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod response;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use context::SpinRequestContext;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use proxy::SpinProxyClient;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use request::{dispatch, into_core_request};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use response::from_core_response;

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
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: spin_sdk::http::IncomingRequest,
) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
    let app = A::build_app();
    dispatch(&app, req).await
}
