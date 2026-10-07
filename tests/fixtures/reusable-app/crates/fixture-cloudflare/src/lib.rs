#![cfg(target_arch = "wasm32")]

use edgezero_core::app::{App, Hooks};
use fixture_core::{FixtureApp, MissingBindingApp, OtherApp, retained_app};
use std::sync::OnceLock;
use worker::{Context, Env, Request, Response, event};
static APP: OnceLock<App> = OnceLock::new();
static OTHER_APP: OnceLock<App> = OnceLock::new();

fn retained_app_for_dispatch<A: Hooks>(cell: &OnceLock<App>) -> worker::Result<&App> {
    retained_app::<A>(cell, edgezero_adapter_cloudflare::CLOUDFLARE_PLATFORM)
        .map_err(|_error| worker::Error::RustError("application configuration failed".to_owned()))
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
        let app = App::build::<MissingBindingApp>(edgezero_adapter_cloudflare::CLOUDFLARE_PLATFORM)
            .map_err(|_error| {
                worker::Error::RustError("application configuration failed".to_owned())
            })?;
        let result =
            edgezero_adapter_cloudflare::dispatch_app::<MissingBindingApp>(&app, req, env, ctx)
                .await;
        return match result {
            Err(error) => Response::from_json(&serde_json::json!({"instance":fixture_core::instance_id(),"source":"injected_required_binding","error":error.to_string()})).map(|r|r.with_status(503)),
            Ok(response) => Ok(response),
        };
    }
    if req.path() == "/other" {
        if !retained {
            return edgezero_adapter_cloudflare::run_app::<OtherApp>(req, env, ctx).await;
        }
        return edgezero_adapter_cloudflare::dispatch_app::<OtherApp>(
            retained_app_for_dispatch::<OtherApp>(&OTHER_APP)?,
            req,
            env,
            ctx,
        )
        .await;
    }
    if retained {
        edgezero_adapter_cloudflare::dispatch_app::<FixtureApp>(
            retained_app_for_dispatch::<FixtureApp>(&APP)?,
            req,
            env,
            ctx,
        )
        .await
    } else {
        edgezero_adapter_cloudflare::run_app::<FixtureApp>(req, env, ctx).await
    }
}
