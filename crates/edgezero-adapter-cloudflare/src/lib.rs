//! Adapter helpers for Cloudflare Workers.

#[cfg(feature = "cli")]
pub mod cli;

// `config_store` compiles on host for its `InMemory` test backend; the
// production `Kv` backend is feature-gated internally.
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
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
use edgezero_core::app::{Hooks, StoresMetadata};
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::env_config::EnvConfig;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::{Context, Env, Error as WorkerError, Request, Response};

/// # Errors
/// Never; this is currently a no-op on Cloudflare Workers (Workers manages
/// its own logging). The signature still returns [`log::SetLoggerError`] so
/// callers and the non-wasm stub stay drop-in compatible if a real logger
/// is wired in later.
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

/// Build an [`EnvConfig`] from a Cloudflare `Env`. Workers have no
/// `std::env`, and the `Env` binding object cannot be enumerated, so the exact
/// `EDGEZERO__STORES__<KIND>__<ID>__NAME` / `__KEY` keys are derived from the
/// baked store metadata and queried individually, alongside the fixed
/// `EDGEZERO__ADAPTER__*` / `EDGEZERO__LOGGING__*` keys.
///
/// `__KEY` is included for `CONFIG` ids only -- it's how spec 5.4 routes the
/// runtime extractor at a per-environment override blob (e.g. `app_config`
/// vs `app_config_staging`). KV/SECRETS bindings don't have a per-id key
/// override.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
fn env_config_from_worker(env: &Env, stores: StoresMetadata) -> EnvConfig {
    let mut keys: Vec<String> = vec![
        "EDGEZERO__ADAPTER__HOST".to_owned(),
        "EDGEZERO__ADAPTER__PORT".to_owned(),
        "EDGEZERO__LOGGING__LEVEL".to_owned(),
    ];
    for (kind, store_meta) in [
        ("CONFIG", stores.config),
        ("KV", stores.kv),
        ("SECRETS", stores.secrets),
    ] {
        if let Some(meta) = store_meta {
            for id in meta.ids {
                let id_upper = id.to_ascii_uppercase();
                keys.push(format!("EDGEZERO__STORES__{kind}__{id_upper}__NAME"));
                if kind == "CONFIG" {
                    keys.push(format!("EDGEZERO__STORES__{kind}__{id_upper}__KEY"));
                }
            }
        }
    }
    let vars = keys
        .into_iter()
        .filter_map(|key| env.var(&key).ok().map(|value| (key, value.to_string())));
    EnvConfig::from_vars(vars)
}

/// Entry point for a Cloudflare Workers application.
///
/// Portable store config is baked into `A` by the `app!` macro; adapter-specific
/// values (platform store names) are read at runtime from `EDGEZERO__*`
/// variables on the worker `Env`. No `edgezero.toml` is required.
///
/// # Errors
/// Returns [`worker::Error`] if the inner dispatch fails or any required
/// store binding cannot be opened.
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
#[inline]
pub async fn run_app<A: Hooks>(
    req: Request,
    env: Env,
    ctx: Context,
) -> Result<Response, WorkerError> {
    // Best-effort: if a logger is already installed, ignore the error rather
    // than panicking — every Worker request re-enters this function.
    drop(init_logger());
    let stores = A::stores();
    let env_config = env_config_from_worker(&env, stores);
    let app = A::build_app();
    request::dispatch_with_registries(
        &app,
        req,
        env,
        ctx,
        request::RegistryInputs {
            config_meta: stores.config,
            kv_meta: stores.kv,
            secret_meta: stores.secrets,
            env_config: &env_config,
        },
    )
    .await
}
