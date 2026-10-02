use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, OtherApp};
use spin_sdk::{
    http::{IntoResponse, Request},
    http_service,
};
use std::sync::OnceLock;
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();

// Deliberately change only binding metadata on the same retained router. A second
// app construction would invalidate the recovery fixture's one-build assertion.
struct MissingBindingApp;
impl Hooks for MissingBindingApp {
    fn routes() -> edgezero_core::router::RouterService {
        FixtureApp::routes()
    }
    fn stores() -> edgezero_core::app::StoresMetadata {
        let mut stores = FixtureApp::stores();
        stores.kv = Some(edgezero_core::app::StoreMetadata {
            default: "missing_fixture_kv",
            ids: &["missing_fixture_kv"],
        });
        stores
    }
}

#[http_service]
async fn handle(req: Request) -> anyhow::Result<impl IntoResponse> {
    if std::env::var("FIXTURE_MODE").as_deref() == Ok("retained")
        && req.uri().path() == "/binding-failure"
    {
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
        return edgezero_adapter_spin::dispatch_app::<OtherApp>(
            OTHER_APP.get_or_init(OtherApp::build_app),
            req,
        )
        .await;
    }
    if std::env::var("FIXTURE_MODE").as_deref() == Ok("retained") {
        edgezero_adapter_spin::dispatch_app::<FixtureApp>(
            APP.get_or_init(FixtureApp::build_app),
            req,
        )
        .await
    } else {
        edgezero_adapter_spin::run_app::<FixtureApp>(req).await
    }
}
