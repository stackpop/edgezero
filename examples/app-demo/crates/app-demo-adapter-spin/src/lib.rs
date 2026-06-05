#![cfg_attr(
    target_arch = "wasm32",
    allow(
        unsafe_code,
        reason = "spin's #[http_service] macro generates the unsafe wasm export"
    )
)]

#[cfg(target_arch = "wasm32")]
use app_demo_core::App;
#[cfg(target_arch = "wasm32")]
use spin_sdk::http::{IntoResponse, Request};
#[cfg(target_arch = "wasm32")]
use spin_sdk::http_service;

#[cfg(target_arch = "wasm32")]
#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    // `run_app_with_seeder` adds the `POST /__edgezero/config/seed`
    // handler used by `edgezero config push --adapter spin`. Plain
    // `run_app` would route the seed request through the app
    // router (404), making `config push` unreachable.
    edgezero_adapter_spin::run_app_with_seeder::<App>(req).await
}
