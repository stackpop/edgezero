#![cfg(target_arch = "wasm32")]
#![allow(
    unsafe_code,
    reason = "spin's #[http_service] macro generates the unsafe wasm export"
)]

use anyhow::Context as _;
use edgezero_core::app::{App, Hooks};
#[cfg(not(feature = "qualification"))]
use fixture_core::FixtureApp;
#[cfg(feature = "qualification")]
use fixture_core::qualification::QualificationApp as FixtureApp;
use fixture_core::{MissingBindingApp, OtherApp, retained_app};
use spin_sdk::{
    http::{IntoResponse, Request},
    http_service,
};
use std::sync::OnceLock;
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();

fn retained_app_for_dispatch<A: Hooks>(cell: &OnceLock<App>) -> anyhow::Result<&App> {
    retained_app::<A>(cell, edgezero_adapter_spin::SPIN_PLATFORM)
        .context("application configuration failed")
}

#[http_service]
async fn handle(req: Request) -> anyhow::Result<edgezero_adapter_spin::SpinResponse> {
    if !FixtureApp::owns_logging() {
        drop(edgezero_adapter_spin::init_logger());
    }
    let retained = std::env::var("FIXTURE_MODE").as_deref() == Ok("retained");
    if retained && req.uri().path() == "/binding-failure" {
        let app = App::build::<MissingBindingApp>(edgezero_adapter_spin::SPIN_PLATFORM)
            .context("application configuration failed")?;
        let result = edgezero_adapter_spin::dispatch_app::<MissingBindingApp>(&app, req).await;
        return match result {
            Err(error) => {
                let body = serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding","error":format!("{error:#}")}).to_string();
                edgezero_core::http::response_builder()
                    .status(503)
                    .header("content-type", "application/json")
                    .body(body)?
                    .into_response()
                    .map_err(|error| {
                        anyhow::anyhow!("fixture response conversion failed: {error:?}")
                    })
            }
            Ok(response) => Ok(response),
        };
    }
    if req.uri().path() == "/bindings" {
        let store = spin_sdk::key_value::Store::open("fixture_config").await?;
        store.set("marker", b"local-fixture").await?;
    }
    if req.uri().path() == "/other" {
        if !retained {
            return edgezero_adapter_spin::run_app::<OtherApp>(req).await;
        }
        return edgezero_adapter_spin::dispatch_app::<OtherApp>(
            retained_app_for_dispatch::<OtherApp>(&OTHER_APP)?,
            req,
        )
        .await;
    }
    if retained {
        edgezero_adapter_spin::dispatch_app::<FixtureApp>(
            retained_app_for_dispatch::<FixtureApp>(&APP)?,
            req,
        )
        .await
    } else {
        edgezero_adapter_spin::run_app::<FixtureApp>(req).await
    }
}
