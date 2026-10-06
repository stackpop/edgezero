//! Test-only executable for real startup, signal, drain and storage acceptance.
#![expect(clippy::print_stdout, reason = "bounded subprocess IPC markers")]

#[path = "../../../edgezero-core/tests/fixtures/production_handlers.rs"]
mod fixture;

use std::cell::Cell;
use std::env;
use std::future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use edgezero_adapter_axum::dev_server::{
    AxumRunOptions, run_app, run_app_with_options_and_initializer, run_production_app,
    run_production_app_with_initializer,
};
use edgezero_core::app::{App, Hooks, StoreMetadata, StoresMetadata};
use edgezero_core::context::RequestContext;
use edgezero_core::extractor::{AppConfig, State};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::probe::{self, LifecyclePhase, LifecycleReader};
use edgezero_core::router::RouterService;
use edgezero_core::{Body, EdgeError, MonotonicClock, MonotonicInstant, action};
use futures::stream;
use tokio::sync::oneshot;
use tokio::time::sleep;

struct FixtureApp;

#[expect(
    clippy::missing_trait_methods,
    reason = "fixture retains unused Hooks defaults"
)]
impl Hooks for FixtureApp {
    fn configure(app: &mut App) -> Result<(), EdgeError> {
        if mode() == "hook-error" {
            return Err(EdgeError::internal(anyhow::anyhow!("HOOK_SENTINEL")));
        }
        if env::var_os("FIXTURE_FREEZE_CLOCK").is_some() {
            let frozen = MonotonicInstant::now();
            app.set_monotonic_clock(MonotonicClock::new(move || frozen));
        }
        Ok(())
    }

    fn owns_logging() -> bool {
        true
    }

    fn routes() -> RouterService {
        RouterService::builder()
            .get("/ready", probe::readiness)
            .get("/live", probe::liveness)
            .get("/state", fixture::state)
            .get("/typed", fixture::typed)
            .get("/finite", finite)
            .get("/pending", pending)
            .get("/stream", endless_stream)
            .get("/flood", flood)
            .get("/after", after)
            .get("/write", write)
            .get("/read", read)
            .get("/alias", alias)
            .get("/distinct", distinct)
            .get("/snapshot", snapshot)
            .get("/stop", stop)
            .build()
    }

    fn stores() -> StoresMetadata {
        if matches!(
            mode().as_str(),
            "bare"
                | "bare-dev"
                | "hook-error"
                | "initializer-error"
                | "starting"
                | "embedding-bare"
        ) {
            return StoresMetadata::default();
        }
        StoresMetadata {
            config: Some(StoreMetadata {
                ids: &["settings"],
                default: "settings",
            }),
            kv: Some(StoreMetadata {
                ids: &["first", "second", "third"],
                default: "first",
            }),
            secrets: Some(StoreMetadata {
                ids: &["vault"],
                default: "vault",
            }),
        }
    }
}

struct WorkGuard(&'static str);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        println!("{}", self.0);
    }
}

struct StopState(Mutex<Option<oneshot::Sender<()>>>);

fn mode() -> String {
    env::var("FIXTURE_MODE").unwrap_or_else(|_| "initialized".to_owned())
}

fn reader(ctx: &RequestContext) -> Result<LifecycleReader, EdgeError> {
    ctx.extensions()
        .get::<LifecycleReader>()
        .cloned()
        .ok_or_else(|| EdgeError::service_unavailable("fixture reader missing"))
}

#[action]
async fn after() -> Result<&'static str, EdgeError> {
    println!("FORBIDDEN_DISPATCH");
    Ok("forbidden dispatch")
}

#[action]
async fn finite(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    let observed = reader(&ctx)?;
    let _guard = WorkGuard("FINITE_DROPPED");
    println!("DISPATCH");
    while observed.phase() == Some(LifecyclePhase::Ready) {
        sleep(Duration::from_millis(5)).await;
    }
    println!("DRAINING");
    sleep(Duration::from_millis(80)).await;
    Ok("finite-complete")
}

#[action]
async fn pending(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    let observed = reader(&ctx)?;
    let _guard = WorkGuard("WORK_DROPPED");
    println!("DISPATCH");
    let mut reported = false;
    #[expect(
        clippy::infinite_loop,
        reason = "fixture proves process shutdown cancels a never-ending handler"
    )]
    loop {
        if !reported && observed.phase() == Some(LifecyclePhase::Draining) {
            println!("DRAINING");
            reported = true;
        }
        sleep(Duration::from_millis(5)).await;
    }
}

#[action]
async fn endless_stream(ctx: RequestContext) -> Result<Body, EdgeError> {
    let observed = reader(&ctx)?;
    let guard = Rc::new(WorkGuard("STREAM_DROPPED"));
    println!("STREAM_READY");
    Ok(Body::from_stream(async_stream::stream! {
        let _retained = guard;
        yield Ok(Bytes::from_static(b"stream-prefix"));
        let mut reported = false;
        loop {
            if !reported && observed.phase() == Some(LifecyclePhase::Draining) {
                println!("DRAINING");
                reported = true;
            }
            sleep(Duration::from_millis(5)).await;
        }
    }))
}

#[action]
async fn flood() -> Result<Body, EdgeError> {
    static BLOCK: [u8; 0x0001_0000] = [b'x'; 0x0001_0000];
    let guard = Rc::new(WorkGuard("STREAM_DROPPED"));
    println!("STREAM_READY");
    Ok(Body::from_stream(stream::repeat_with(move || {
        let _retained = &guard;
        Ok(Bytes::from_static(&BLOCK))
    })))
}

fn kv(ctx: &RequestContext, id: &str) -> Result<KvHandle, EdgeError> {
    ctx.kv_store(id)
        .ok_or_else(|| EdgeError::service_unavailable("fixture KV missing"))
}

#[action]
async fn write(ctx: RequestContext) -> Result<&'static str, EdgeError> {
    kv(&ctx, "first")?
        .put("persisted", &"durable-value")
        .await?;
    Ok("written")
}

#[action]
async fn read(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "first")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn alias(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "second")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn distinct(ctx: RequestContext) -> Result<String, EdgeError> {
    Ok(kv(&ctx, "third")?
        .get::<String>("persisted")
        .await?
        .unwrap_or_default())
}

#[action]
async fn snapshot(ctx: RequestContext) -> Result<String, EdgeError> {
    let binding = ctx
        .config_store_default_binding()
        .ok_or_else(|| EdgeError::service_unavailable("fixture config missing"))?;
    Ok(binding
        .handle
        .get(&binding.default_key)
        .await?
        .unwrap_or_default())
}

#[action]
async fn stop(State(state): State<Arc<StopState>>) -> Result<&'static str, EdgeError> {
    if let Some(sender) = state
        .0
        .lock()
        .map_err(|_error| EdgeError::internal(anyhow::anyhow!("fixture stop lock poisoned")))?
        .take()
    {
        let _sent = sender.send(());
    }
    Ok("stopping")
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "test-only executable asserts initializer ownership and call counts"
)]
fn main() -> anyhow::Result<()> {
    let selected = mode();
    if matches!(selected.as_str(), "development" | "bare-dev") {
        return run_app::<FixtureApp>();
    }
    if matches!(selected.as_str(), "framework" | "bare" | "hook-error") {
        return run_production_app::<FixtureApp>();
    }
    if selected == "starting" {
        return run_production_app_with_initializer::<FixtureApp, _>(|_app, _stores| {
            Box::pin(async {
                let _guard = WorkGuard("STARTUP_DROPPED");
                println!("INITIALIZING");
                future::pending::<Result<(), EdgeError>>().await
            })
        });
    }
    if selected == "initializer-error" {
        return run_production_app_with_initializer::<FixtureApp, _>(|_app, _stores| {
            Box::pin(async { Err(EdgeError::internal(anyhow::anyhow!("CALLBACK_SENTINEL"))) })
        });
    }
    let calls = Rc::new(Cell::new(0_u32));
    let initialized = Rc::clone(&calls);
    if selected.starts_with("embedding") {
        let address: SocketAddr = env::var("FIXTURE_BIND")?.parse()?;
        let mut options = AxumRunOptions::new(address)?;
        if selected == "embedding" {
            options = options
                .with_config_dir(env::var("FIXTURE_CONFIG_ROOT")?)?
                .with_data_dir(env::var("FIXTURE_DATA_ROOT")?)?;
        }
        let (sender, receiver) = oneshot::channel();
        let stop_capture = Rc::clone(&calls);
        return run_app_with_options_and_initializer::<FixtureApp, _, _>(
            options,
            async move {
                let _closed = receiver.await;
                assert_eq!(
                    stop_capture.get(),
                    1,
                    "initializer ran once before caller stop"
                );
            },
            move |app, stores| {
                Box::pin(async move {
                    if let Some(binding) =
                        stores.config().and_then(|registry| registry.default_ref())
                    {
                        let config = AppConfig::<fixture::FixtureConfig>::from_binding(
                            binding,
                            None,
                            stores.secrets(),
                            app.config_extraction_limits(),
                            app.monotonic_clock(),
                        )
                        .await?;
                        app.insert_state(Arc::new(fixture::HostState {
                            greeting: config.greeting,
                        }));
                    }
                    app.insert_state(Arc::new(StopState(Mutex::new(Some(sender)))));
                    initialized.set(initialized.get().saturating_add(1));
                    println!("INITIALIZED");
                    Ok(())
                })
            },
        );
    }
    run_production_app_with_initializer::<FixtureApp, _>(move |app, stores| {
        Box::pin(async move {
            let binding = stores
                .config()
                .and_then(|registry| registry.default_ref())
                .ok_or_else(|| {
                    EdgeError::service_unavailable("required fixture binding missing")
                })?;
            let config = AppConfig::<fixture::FixtureConfig>::from_binding(
                binding,
                None,
                stores.secrets(),
                app.config_extraction_limits(),
                app.monotonic_clock(),
            )
            .await?;
            app.insert_state(Arc::new(fixture::HostState {
                greeting: config.greeting,
            }));
            initialized.set(initialized.get().saturating_add(1));
            assert_eq!(initialized.get(), 1, "initializer runs exactly once");
            println!("INITIALIZED");
            Ok(())
        })
    })
}
