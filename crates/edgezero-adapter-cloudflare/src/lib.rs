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

/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
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

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub trait AppExt {
    #[deprecated(
        note = "AppExt::dispatch() is the low-level manual path and does not inject config-store metadata; prefer run_app(), dispatch_with_config(), or dispatch_with_config_handle()"
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

/// Build an [`EnvConfig`](edgezero_core::env_config::EnvConfig) from a
/// Cloudflare `Env`. Workers have no `std::env`, and the `Env` binding object
/// cannot be enumerated, so the exact `EDGEZERO__STORES__<KIND>__<ID>__NAME`
/// keys are derived from the baked store metadata and queried individually,
/// alongside the fixed `EDGEZERO__ADAPTER__*` / `EDGEZERO__LOGGING__*` keys.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
fn env_config_from_worker(
    env: &worker::Env,
    stores: edgezero_core::app::StoresMetadata,
) -> edgezero_core::env_config::EnvConfig {
    let mut keys: Vec<String> = vec![
        "EDGEZERO__ADAPTER__HOST".to_owned(),
        "EDGEZERO__ADAPTER__PORT".to_owned(),
        "EDGEZERO__LOGGING__LEVEL".to_owned(),
    ];
    for (kind, meta) in [
        ("CONFIG", stores.config),
        ("KV", stores.kv),
        ("SECRETS", stores.secrets),
    ] {
        if let Some(meta) = meta {
            for id in meta.ids {
                keys.push(format!(
                    "EDGEZERO__STORES__{kind}__{}__NAME",
                    id.to_ascii_uppercase()
                ));
            }
        }
    }
    let vars = keys
        .into_iter()
        .filter_map(|key| env.var(&key).ok().map(|value| (key, value.to_string())));
    edgezero_core::env_config::EnvConfig::from_vars(vars)
}

/// Entry point for a Cloudflare Workers application.
///
/// Portable store config is baked into `A` by the `app!` macro; adapter-specific
/// values (platform store names) are read at runtime from `EDGEZERO__*`
/// variables on the worker `Env`. No `edgezero.toml` is required.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> Result<worker::Response, worker::Error> {
    init_logger().expect("init cloudflare logger");
    let stores = A::stores();
    let env_config = env_config_from_worker(&env, stores);
    let kv_binding = stores.kv.map_or_else(
        || crate::request::DEFAULT_KV_BINDING.to_owned(),
        |meta| env_config.store_name("kv", meta.default),
    );
    let config_binding = stores
        .config
        .map(|meta| env_config.store_name("config", meta.default));
    let app = A::build_app();
    crate::request::dispatch_with_bindings(
        &app,
        req,
        env,
        ctx,
        config_binding.as_deref(),
        &kv_binding,
        stores.kv.is_some(),
        stores.secrets.is_some(),
    )
    .await
}
