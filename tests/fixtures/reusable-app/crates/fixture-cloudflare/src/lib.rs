use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, OtherApp};
use std::sync::OnceLock;
use worker::{Context, Env, Request, Response, event};
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

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    if !FixtureApp::owns_logging() {
        drop(edgezero_adapter_cloudflare::init_logger());
    }
    let retained = env
        .var("FIXTURE_MODE")
        .map(|v| v.to_string() == "retained")
        .unwrap_or(false);
    if retained && req.path() == "/binding-failure" {
        let result = edgezero_adapter_cloudflare::dispatch_app::<MissingBindingApp>(
            APP.get_or_init(FixtureApp::build_app),
            req,
            env,
            ctx,
        )
        .await;
        return match result {
            Err(error) => Response::from_json(&serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding","error":error.to_string()})).map(|r|r.with_status(503)),
            Ok(response) => Ok(response),
        };
    }
    if req.path() == "/other" {
        return edgezero_adapter_cloudflare::dispatch_app::<OtherApp>(
            OTHER_APP.get_or_init(OtherApp::build_app),
            req,
            env,
            ctx,
        )
        .await;
    }
    if retained {
        edgezero_adapter_cloudflare::dispatch_app::<FixtureApp>(
            APP.get_or_init(FixtureApp::build_app),
            req,
            env,
            ctx,
        )
        .await
    } else {
        edgezero_adapter_cloudflare::run_app::<FixtureApp>(req, env, ctx).await
    }
}
