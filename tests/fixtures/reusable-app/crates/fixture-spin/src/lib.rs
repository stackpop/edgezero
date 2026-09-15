use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, OtherApp};
use spin_sdk::{
    http::{IntoResponse, Request},
    http_service,
};
use std::sync::OnceLock;
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();
#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    if std::env::var("FIXTURE_MODE").as_deref() == Ok("retained")
        && req.uri().path() == "/binding-failure"
    {
        let mut stores = FixtureApp::stores();
        stores.kv = Some(edgezero_core::app::StoreMetadata {
            default: "missing_fixture_kv",
            ids: &["missing_fixture_kv"],
        });
        let result = edgezero_adapter_spin::dispatch_app(
            APP.get_or_init(FixtureApp::build_app),
            stores,
            req,
        )
        .await;
        return match result {
            Err(_) => {
                let body = serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding"}).to_string();
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
        return edgezero_adapter_spin::dispatch_app(
            OTHER_APP.get_or_init(OtherApp::build_app),
            OtherApp::stores(),
            req,
        )
        .await;
    }
    if std::env::var("FIXTURE_MODE").as_deref() == Ok("retained") {
        edgezero_adapter_spin::dispatch_app(
            APP.get_or_init(FixtureApp::build_app),
            FixtureApp::stores(),
            req,
        )
        .await
    } else {
        edgezero_adapter_spin::run_app::<FixtureApp>(req).await
    }
}
