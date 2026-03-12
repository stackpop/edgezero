//! Adapter helpers for Cloudflare Workers.

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use std::collections::{HashMap, VecDeque};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod config_store;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod context;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod proxy;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod request;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
mod response;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use config_store::CloudflareConfigStore;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use context::CloudflareRequestContext;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use proxy::CloudflareProxyClient;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[allow(deprecated)]
pub use request::{dispatch, dispatch_with_config, dispatch_with_config_store, into_core_request};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub use response::from_core_response;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

#[cfg(not(all(feature = "cloudflare", target_arch = "wasm32")))]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub trait AppExt {
    #[deprecated(
        note = "AppExt::dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_store()"
    )]
    fn dispatch<'a>(
        &'a self,
        req: worker::Request,
        env: worker::Env,
        ctx: worker::Context,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = Result<worker::Response, worker::Error>> + 'a>,
    >;
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl AppExt for edgezero_core::app::App {
    #[allow(deprecated)]
    fn dispatch<'a>(
        &'a self,
        req: worker::Request,
        env: worker::Env,
        ctx: worker::Context,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = Result<worker::Response, worker::Error>> + 'a>,
    > {
        Box::pin(crate::request::dispatch_raw(self, req, env, ctx))
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let app = A::build_app();
    dispatch_app(
        &app,
        req,
        env,
        ctx,
        A::config_store().map(|cfg| cfg.name_for_adapter(edgezero_core::app::CLOUDFLARE_ADAPTER)),
    )
    .await
}

/// Run the app resolving the config store binding name from `manifest_src`.
///
/// Prefers hook metadata from [`edgezero_core::app::Hooks::config_store`]
/// and falls back to resolving `[stores.config]` from `manifest_src`.
///
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app_with_manifest<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let app = A::build_app();
    let binding_name = A::config_store()
        .map(|cfg| {
            cfg.name_for_adapter(edgezero_core::app::CLOUDFLARE_ADAPTER)
                .to_string()
        })
        .or_else(|| resolve_manifest_config_store_name(manifest_src));
    dispatch_app(&app, req, env, ctx, binding_name.as_deref()).await
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
async fn dispatch_app(
    app: &edgezero_core::app::App,
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
    config_store_name: Option<&str>,
) -> Result<worker::Response, worker::Error> {
    if let Some(binding_name) = config_store_name {
        dispatch_with_config(app, req, env, ctx, binding_name).await
    } else {
        crate::request::dispatch_raw(app, req, env, ctx).await
    }
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
fn resolve_manifest_config_store_name(manifest_src: &str) -> Option<String> {
    const MANIFEST_NAME_CACHE_LIMIT: usize = 8;

    if let Some(binding_name) = manifest_name_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(manifest_src)
    {
        return binding_name;
    }

    let manifest = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let binding_name = manifest.manifest().stores.config.as_ref().map(|cfg| {
        cfg.config_store_name(edgezero_core::app::CLOUDFLARE_ADAPTER)
            .to_string()
    });

    manifest_name_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            manifest_src,
            binding_name.clone(),
            MANIFEST_NAME_CACHE_LIMIT,
        )
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
fn manifest_name_cache() -> &'static Mutex<ManifestNameCache> {
    static CACHE: OnceLock<Mutex<ManifestNameCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ManifestNameCache::default()))
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[derive(Default)]
struct ManifestNameCache {
    entries: HashMap<String, Option<String>>,
    order: VecDeque<String>,
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl ManifestNameCache {
    fn get(&self, manifest_src: &str) -> Option<Option<String>> {
        self.entries.get(manifest_src).cloned()
    }

    fn insert(
        &mut self,
        manifest_src: &str,
        binding_name: Option<String>,
        limit: usize,
    ) -> Option<String> {
        if let Some(existing) = self.entries.get(manifest_src) {
            return existing.clone();
        }

        if limit > 0 && self.order.len() >= limit {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }

        let manifest_src = manifest_src.to_string();
        self.order.push_back(manifest_src.clone());
        self.entries.insert(manifest_src, binding_name.clone());
        binding_name
    }
}
