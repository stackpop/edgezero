//! Child executable used by `tests/native_aws_startup.rs`.

use std::future;
use std::sync::Arc;

use edgezero_adapter_axum::dev_server::{
    AxumRunOptions, LocalStartupFuture, PreparedStores, run_app_with_native_stores,
};
use edgezero_adapter_axum::native_stores::NativeStoreOverrides;
use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::context::RequestContext;
use edgezero_core::extractor::{AppConfig as TypedAppConfig, State};
use edgezero_core::router::RouterService;
use edgezero_core::{EdgeError, action};
use serde::Deserialize;
use validator::Validate;

struct NativeAwsApp;

#[derive(Deserialize, Validate, edgezero_core::AppConfig)]
struct FixtureConfig {
    #[validate(length(min = 4_u64))]
    greeting: String,
    #[secret]
    #[validate(length(min = 8_u64))]
    token: String,
}

#[derive(Clone)]
struct Snapshot {
    greeting: String,
    token: String,
}

#[expect(
    clippy::missing_trait_methods,
    reason = "fixture retains unused Hooks defaults"
)]
impl Hooks for NativeAwsApp {
    fn routes() -> RouterService {
        RouterService::builder()
            .get("/snapshot", snapshot)
            .get("/local-secret", local_secret)
            .build()
    }

    fn stores() -> StoresMetadata {
        StoresMetadata {
            config: Some(StoreMetadata {
                ids: &["app"],
                default: "app",
            }),
            kv: None,
            secrets: Some(StoreMetadata {
                ids: &["vault", "local"],
                default: "vault",
            }),
        }
    }
}

#[action]
async fn snapshot(State(snapshot): State<Arc<Snapshot>>) -> Result<String, EdgeError> {
    Ok(format!("{}|{}", snapshot.greeting, snapshot.token))
}

#[action]
async fn local_secret(ctx: RequestContext) -> Result<String, EdgeError> {
    let local = ctx
        .secret_store("local")
        .ok_or_else(|| EdgeError::service_unavailable("local fixture store missing"))?;
    Ok(local.require_str("LOCAL_FIXTURE_SECRET").await?)
}

fn initialize<'startup>(
    app: &'startup mut App,
    stores: &'startup PreparedStores,
) -> LocalStartupFuture<'startup> {
    Box::pin(async move {
        let binding = stores
            .config()
            .and_then(|registry| registry.named_ref("app"))
            .ok_or_else(|| EdgeError::service_unavailable("config fixture missing"))?;
        let config = TypedAppConfig::<FixtureConfig>::from_binding(
            binding,
            None,
            stores.secrets(),
            app.config_extraction_limits(),
            app.monotonic_clock(),
        )
        .await?;
        app.insert_state(Arc::new(Snapshot {
            greeting: config.greeting,
            token: config.token,
        }));
        Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    let options = AxumRunOptions::from_env()?;
    run_app_with_native_stores::<NativeAwsApp, _, _>(
        options,
        NativeStoreOverrides::default(),
        future::pending::<()>(),
        initialize,
    )?;
    Ok(())
}
