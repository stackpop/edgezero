use crate::Fixture;
use edgezero_adapter_spin::request::into_core_request;
use edgezero_adapter_spin::response::from_core_response;
use edgezero_core::app::Hooks;
use spin_sdk::http::{IntoResponse, Request};
use spin_sdk::http_service;

#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    let request = into_core_request(req).await?;
    let response = Fixture::build_app().router().oneshot(request).await?;
    Ok(from_core_response(response).await?)
}
