//! Adapter helpers for Cloudflare Workers.

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod config_store;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod context;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod key_value_store;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod response;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod secret_store;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use core::future::Future;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use core::pin::Pin;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::app::{App, Hooks, CLOUDFLARE_ADAPTER};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::manifest::ManifestLoader;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::{Context, Env, Error as WorkerError, Request, Response};

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub trait AppExt {
    #[deprecated(
        note = "AppExt::dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_handle()"
    )]
    fn dispatch<'app>(
        &'app self,
        req: Request,
        env: Env,
        ctx: Context,
    ) -> Pin<Box<dyn Future<Output = Result<Response, WorkerError>> + 'app>>;
}

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
impl AppExt for App {
    #[inline]
    fn dispatch<'app>(
        &'app self,
        req: Request,
        env: Env,
        ctx: Context,
    ) -> Pin<Box<dyn Future<Output = Result<Response, WorkerError>> + 'app>> {
        Box::pin(request::dispatch_raw(self, req, env, ctx))
    }
}

/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// # Errors
/// Never; this is a no-op stub on non-wasm targets.
#[cfg(not(all(feature = "cloudflare", target_arch = "wasm32")))]
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Entry point for a Cloudflare Workers application.
///
/// **Breaking change (pre-1.0):** `manifest_src` is now a required parameter.
/// Callers previously using `run_app_with_manifest` can rename to `run_app` —
/// the signatures are identical.
///
/// # Errors
/// Returns [`worker::Error`] if the manifest cannot be parsed, the
/// inner dispatch fails, or any required store binding cannot be
/// resolved.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[inline]
pub async fn run_app<A: Hooks>(
    manifest_src: &str,
    req: Request,
    env: Env,
    ctx: Context,
) -> Result<Response, WorkerError> {
    // Best-effort: if a logger is already installed, ignore the error rather
    // than panicking — every Worker request re-enters this function.
    drop(init_logger());
    let manifest_loader = ManifestLoader::try_load_from_str(manifest_src)
        .map_err(|err| WorkerError::RustError(err.to_string()))?;
    let manifest = manifest_loader.manifest();
    let kv_binding = manifest.kv_store_name(CLOUDFLARE_ADAPTER);
    let kv_required = manifest.stores.kv.is_some();
    // Two-path resolution: `A::config_store()` is set at compile time by the
    // `#[app]` macro and is the common case. The manifest fallback handles
    // callers that implement `Hooks` manually without the macro — in that case
    // `A::config_store()` returns `None` while `[stores.config]` in
    // `edgezero.toml` may still be present.
    let config_binding = A::config_store()
        .map(|cfg| cfg.name_for_adapter(CLOUDFLARE_ADAPTER))
        .or_else(|| {
            manifest
                .stores
                .config
                .as_ref()
                .map(|cfg| cfg.config_store_name(CLOUDFLARE_ADAPTER))
        });
    let secrets_required = manifest.secret_store_enabled("cloudflare");
    let app = A::build_app();
    request::dispatch_with_bindings(
        &app,
        req,
        env,
        ctx,
        request::RuntimeBindings {
            config: config_binding,
            kv: kv_binding,
            kv_required,
            secrets_required,
        },
    )
    .await
}

/// Deprecated: use [`run_app`] which now takes `manifest_src` directly.
///
/// # Errors
/// Same conditions as [`run_app`].
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[deprecated(note = "use run_app instead, which now takes manifest_src")]
#[inline]
pub async fn run_app_with_manifest<A: Hooks>(
    manifest_src: &str,
    req: Request,
    env: Env,
    ctx: Context,
) -> Result<Response, WorkerError> {
    run_app::<A>(manifest_src, req, env, ctx).await
}
