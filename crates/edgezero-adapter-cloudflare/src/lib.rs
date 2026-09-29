//! Adapter helpers for Cloudflare Workers.

#[cfg(all(feature = "cli", not(target_arch = "wasm32")))]
pub mod cli;

// `config_store` compiles on host for its `InMemory` test backend; the
// production `Kv` backend is feature-gated internally.
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
pub mod config_store;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod context;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod key_value_store;
#[cfg(any(
    test,
    feature = "test-utils",
    all(feature = "cloudflare", target_arch = "wasm32")
))]
pub mod outbound;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod request;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
pub mod response;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
pub mod secret_store;

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::app::StoresMetadata;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::app::{App, Hooks};
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::context::RuntimeVariables;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::env_config::EnvConfig;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::error::EdgeError;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use edgezero_core::manifest::BakedManifest;
#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
use edgezero_core::manifest::ResolvedEnvironmentBinding;
#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
use worker::{Context, Env, Error as WorkerError, Request, Response};

/// Cloudflare Workers' published per-isolate memory limit.
pub const CLOUDFLARE_PLATFORM: edgezero_core::PlatformMetadata =
    edgezero_core::PlatformMetadata::new(
        edgezero_core::PlatformFact::known(
            edgezero_core::MemoryCeiling::new(
                128_000_000,
                edgezero_core::MemoryCeilingScope::PerInstance,
                None,
            ),
            edgezero_core::PlatformResourceSource::PlatformLimit {
                provider: "Cloudflare Workers",
            },
        ),
        edgezero_core::PlatformFact::unknown(
            edgezero_core::PlatformUnknownReason::ProviderUnpublished,
        ),
        edgezero_core::PlatformFact::unknown(
            edgezero_core::PlatformUnknownReason::ProviderUnpublished,
        ),
    );

#[cfg(all(feature = "cloudflare", target_arch = "wasm32"))]
fn build_app_for_dispatch<A: Hooks>() -> Result<App, WorkerError> {
    build_target_app::<A>().map_err(|error| {
        log::error!("application configuration failed: {}", error.kind());
        WorkerError::RustError("application configuration failed".to_owned())
    })
}

#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
fn build_target_app<A: Hooks>() -> Result<App, EdgeError> {
    App::build::<A>(CLOUDFLARE_PLATFORM)
}

/// Test seam for the production application-assembly error mapping.
#[cfg(all(feature = "test-utils", feature = "cloudflare", target_arch = "wasm32"))]
#[doc(hidden)]
#[inline]
pub fn build_app_for_test<A: Hooks>() -> Result<App, WorkerError> {
    build_app_for_dispatch::<A>()
}

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

#[cfg(any(test, all(feature = "cloudflare", target_arch = "wasm32")))]
fn collect_worker_variables(
    bindings: &[ResolvedEnvironmentBinding],
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> RuntimeVariables {
    RuntimeVariables::from_vars(bindings.iter().filter_map(|binding| {
        lookup(&binding.env)
            .or_else(|| binding.value.clone())
            .map(|value| (binding.name.clone(), value))
    }))
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
    let app = build_app_for_dispatch::<A>()?;
    // Best-effort: if a logger is already installed, ignore the error rather
    // than panicking — every Worker request re-enters this function. Skipped
    // entirely when the app owns logging.
    if !A::owns_logging() {
        drop(init_logger());
    }
    let stores = A::stores();
    let env_config = env_config_from_worker(&env, stores);
    let runtime_variables = match A::manifest() {
        BakedManifest::Absent => RuntimeVariables::default(),
        BakedManifest::Present(manifest) => {
            let resolved = manifest.environment_for("cloudflare");
            collect_worker_variables(&resolved.variables, |key| {
                env.var(key).ok().map(|value| value.to_string())
            })
        }
        BakedManifest::Malformed(_) | _ => {
            return Err(WorkerError::RustError(
                "application manifest is invalid".to_owned(),
            ));
        }
    };
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
            runtime_variables,
        },
    )
    .await
}

#[cfg(test)]
mod platform_tests {
    use edgezero_core::app::{App, Hooks};
    use edgezero_core::error::EdgeError;
    use edgezero_core::manifest::ResolvedEnvironmentBinding;
    use edgezero_core::router::RouterService;

    struct PlatformAwareConfiguration;

    #[expect(
        clippy::missing_trait_methods,
        reason = "test hook exercises only target metadata propagation"
    )]
    impl Hooks for PlatformAwareConfiguration {
        fn configure(app: &mut App) -> Result<(), EdgeError> {
            if app.platform() == crate::CLOUDFLARE_PLATFORM {
                Ok(())
            } else {
                Err(EdgeError::service_unavailable("wrong application platform"))
            }
        }

        fn routes() -> RouterService {
            RouterService::builder().build()
        }
    }

    #[test]
    fn application_configuration_receives_cloudflare_platform_metadata() {
        let app = super::build_target_app::<PlatformAwareConfiguration>()
            .expect("configured application");

        assert_eq!(app.platform(), crate::CLOUDFLARE_PLATFORM);
    }

    #[test]
    fn declared_worker_variables_prefer_binding_then_manifest_default() {
        let bindings = [
            ResolvedEnvironmentBinding {
                description: None,
                env: "UPSTREAM_ORIGIN".to_owned(),
                name: "API_BASE_URL".to_owned(),
                value: Some("https://default.example".to_owned()),
            },
            ResolvedEnvironmentBinding {
                description: None,
                env: "OPTIONAL_BINDING".to_owned(),
                name: "OPTIONAL".to_owned(),
                value: None,
            },
        ];
        let runtime = super::collect_worker_variables(&bindings, |key| {
            (key == "UPSTREAM_ORIGIN").then(|| "https://worker.example".to_owned())
        });
        assert_eq!(runtime.get("API_BASE_URL"), Some("https://worker.example"));
        assert_eq!(runtime.get("OPTIONAL"), None);

        let defaulted = super::collect_worker_variables(&bindings, |_| None);
        assert_eq!(
            defaulted.get("API_BASE_URL"),
            Some("https://default.example")
        );
    }
}
