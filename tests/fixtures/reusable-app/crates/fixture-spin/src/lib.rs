use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, MissingBindingApp, OtherApp};
use spin_sdk::{
    http::{IntoResponse, Request},
    http_service,
};
use std::sync::OnceLock;
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();

#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    if !FixtureApp::owns_logging() {
        drop(edgezero_adapter_spin::init_logger());
    }
    let retained = std::env::var("FIXTURE_MODE").as_deref() == Ok("retained");
    if retained && req.uri().path() == "/binding-failure" {
        // Pairs MissingBindingApp with FixtureApp's router; valid only while
        // dispatch_app reads nothing but A::stores() (see MissingBindingApp).
        let result = edgezero_adapter_spin::dispatch_app::<MissingBindingApp>(
            APP.get_or_init(FixtureApp::build_app),
            req,
        )
        .await;
        return match result {
            Err(error) => {
                let body = serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding","error":format!("{error:#}")}).to_string();
                Ok(edgezero_adapter_spin::response::from_core_response(
                    edgezero_core::http::response_builder()
                        .status(503)
                        .body(edgezero_core::body::Body::from(body))
                        .unwrap(),
                )
                .await?)
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
            OTHER_APP.get_or_init(OtherApp::build_app),
            req,
        )
        .await;
    }
    if retained {
        edgezero_adapter_spin::dispatch_app::<FixtureApp>(
            APP.get_or_init(FixtureApp::build_app),
            req,
        )
        .await
    } else {
        edgezero_adapter_spin::run_app::<FixtureApp>(req).await
    }
}
