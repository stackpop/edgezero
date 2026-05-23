//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
mod config_store;
pub mod context;
mod decompress;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod response;

// SpinConfigStore is available without the `spin` feature flag because its
// production spin_sdk backend is feature-gated internally, allowing the
// InMemory test backend to compile on all targets. SpinKvStore and
// SpinSecretStore import spin_sdk types at the module level and therefore
// require `all(feature = "spin", target_arch = "wasm32")`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod key_value_store;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
mod secret_store;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use config_store::SpinConfigStore;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::app::StoresMetadata;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::env_config::EnvConfig;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::manifest::DEFAULT_KV_STORE_NAME;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use key_value_store::SpinKvStore;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use proxy::SpinProxyClient;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use request::{dispatch, dispatch_with_kv_label, into_core_request};
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use response::from_core_response;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use secret_store::SpinSecretStore;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub trait AppExt {
    fn dispatch<'a>(
        &'a self,
        req: spin_sdk::http::IncomingRequest,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = anyhow::Result<spin_sdk::http::Response>> + 'a>,
    >;
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl AppExt for edgezero_core::app::App {
    fn dispatch<'a>(
        &'a self,
        req: spin_sdk::http::IncomingRequest,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = anyhow::Result<spin_sdk::http::Response>> + 'a>,
    > {
        Box::pin(request::dispatch(self, req))
    }
}

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpinStoreSettings {
    pub config_enabled: bool,
    pub kv_label: String,
    pub kv_required: bool,
    pub secrets_enabled: bool,
}

/// Initialize the logger for Spin.
///
/// Currently a no-op — Spin manages its own logging internally.
/// When a real logger is needed for one target, split this into
/// `#[cfg(all(feature = "spin", target_arch = "wasm32"))]` /
/// `#[cfg(not(...))]` branches following the Fastly/Cloudflare pattern.
// TODO: wire in real Spin logger when available
/// # Errors
/// Returns [`log::SetLoggerError`] if a global logger is already installed.
#[inline]
pub fn init_logger() -> Result<(), log::SetLoggerError> {
    Ok(())
}

/// Resolve Spin store settings from baked store metadata plus `EDGEZERO__*`
/// environment config. The KV label resolves to the platform name for the
/// declared default logical id, or [`DEFAULT_KV_STORE_NAME`] when no
/// `[stores.kv]` was declared.
///
/// [`DEFAULT_KV_STORE_NAME`]: edgezero_core::manifest::DEFAULT_KV_STORE_NAME
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub(crate) fn resolve_store_settings(stores: StoresMetadata, env: &EnvConfig) -> SpinStoreSettings {
    let kv_label = stores.kv.map_or_else(
        || DEFAULT_KV_STORE_NAME.to_owned(),
        |meta| env.store_name("kv", meta.default),
    );
    SpinStoreSettings {
        config_enabled: stores.config.is_some(),
        kv_label,
        kv_required: stores.kv.is_some(),
        secrets_enabled: stores.secrets.is_some(),
    }
}

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the EdgeZero router, and return the
/// response.
///
/// Portable store config is baked into `A` by the `app!` macro; the KV store
/// label is resolved at runtime from `EDGEZERO__STORES__KV__<ID>__NAME`. No
/// `edgezero.toml` is required.
///
/// Usage in a Spin component:
///
/// ```ignore
/// use spin_sdk::http_component;
/// use my_core::App;
///
/// #[http_component]
/// async fn handle(req: spin_sdk::http::IncomingRequest) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
///     edgezero_adapter_spin::run_app::<App>(req).await
/// }
/// ```
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    req: spin_sdk::http::IncomingRequest,
) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
    // Use `let _ =` instead of `.expect()` because Spin calls
    // `#[http_component]` per-request. Once a real logger is wired in,
    // `log::set_logger` returns Err on the second call — `.expect()`
    // would panic on every subsequent request.
    let _ = init_logger();
    let settings = resolve_store_settings(A::stores(), &EnvConfig::from_env());
    let app = A::build_app();
    request::dispatch_with_store_settings(&app, req, &settings).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::StoreMetadata;

    #[test]
    fn store_settings_default_to_optional_kv_without_config_or_secrets() {
        let empty: [(&str, &str); 0] = [];
        let settings =
            resolve_store_settings(StoresMetadata::default(), &EnvConfig::from_vars(empty));

        assert_eq!(settings.kv_label, DEFAULT_KV_STORE_NAME);
        assert!(!settings.kv_required);
        assert!(!settings.config_enabled);
        assert!(!settings.secrets_enabled);
    }

    #[test]
    fn store_settings_resolve_baked_metadata() {
        let stores = StoresMetadata {
            config: Some(StoreMetadata {
                default: "app_config",
                ids: &["app_config"],
            }),
            kv: Some(StoreMetadata {
                default: "sessions",
                ids: &["sessions", "cache"],
            }),
            secrets: Some(StoreMetadata {
                default: "default",
                ids: &["default"],
            }),
        };
        let empty: [(&str, &str); 0] = [];
        let settings = resolve_store_settings(stores, &EnvConfig::from_vars(empty));

        // No env override: the KV label resolves to the default logical id.
        assert_eq!(settings.kv_label, "sessions");
        assert!(settings.kv_required);
        assert!(settings.config_enabled);
        assert!(settings.secrets_enabled);
    }

    #[test]
    fn store_settings_kv_label_from_env() {
        let stores = StoresMetadata {
            config: None,
            kv: Some(StoreMetadata {
                default: "sessions",
                ids: &["sessions"],
            }),
            secrets: None,
        };
        let env = EnvConfig::from_vars([("EDGEZERO__STORES__KV__SESSIONS__NAME", "prod-label")]);
        let settings = resolve_store_settings(stores, &env);

        assert_eq!(settings.kv_label, "prod-label");
    }
}
