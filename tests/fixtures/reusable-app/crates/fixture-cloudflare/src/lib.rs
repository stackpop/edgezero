use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, OtherApp};
use std::sync::OnceLock;
use worker::{Context, Env, Request, Response, event};
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();
#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let retained = env
        .var("FIXTURE_MODE")
        .map(|v| v.to_string() == "retained")
        .unwrap_or(false);
    if retained && req.path() == "/binding-failure" {
        let mut stores = FixtureApp::stores();
        stores.kv = Some(edgezero_core::app::StoreMetadata {
            default: "missing_fixture_kv",
            ids: &["missing_fixture_kv"],
        });
        let result = edgezero_adapter_cloudflare::dispatch_app(
            APP.get_or_init(FixtureApp::build_app),
            stores,
            req,
            env,
            ctx,
        )
        .await;
        return match result {
            Err(_) => Response::from_json(&serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding"})).map(|r|r.with_status(503)),
            Ok(response) => Ok(response),
        };
    }
    if req.path() == "/other" {
        return edgezero_adapter_cloudflare::dispatch_app(
            OTHER_APP.get_or_init(OtherApp::build_app),
            OtherApp::stores(),
            req,
            env,
            ctx,
        )
        .await;
    }
    if retained {
        edgezero_adapter_cloudflare::dispatch_app(
            APP.get_or_init(FixtureApp::build_app),
            FixtureApp::stores(),
            req,
            env,
            ctx,
        )
        .await
    } else {
        edgezero_adapter_cloudflare::run_app::<FixtureApp>(req, env, ctx).await
    }
}
