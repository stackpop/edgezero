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
use edgezero_core::app::SPIN_ADAPTER;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::manifest::Manifest;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use key_value_store::SpinKvStore;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use proxy::SpinProxyClient;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub use request::{dispatch, dispatch_with_kv_label, dispatch_with_manifest, into_core_request};
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

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub(crate) fn resolve_store_settings(
    manifest: &Manifest,
    hook_has_config_store: bool,
) -> SpinStoreSettings {
    SpinStoreSettings {
        config_enabled: hook_has_config_store || manifest.stores.config.is_some(),
        kv_label: manifest.kv_store_name(SPIN_ADAPTER).to_owned(),
        kv_required: manifest.stores.kv.is_some(),
        secrets_enabled: manifest.secret_store_enabled(SPIN_ADAPTER),
    }
}

/// Convenience entry point: build the app from `Hooks`, dispatch the
/// incoming Spin request through the EdgeZero router, and return the
/// response.
///
/// `manifest_src` must be the contents of `edgezero.toml`. `run_app` uses it
/// to resolve KV, config-store, and secret-store manifest gating before
/// dispatching.
///
/// Usage in a Spin component:
///
/// ```ignore
/// use spin_sdk::http_component;
/// use my_core::App;
///
/// #[http_component]
/// async fn handle(req: spin_sdk::http::IncomingRequest) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
///     edgezero_adapter_spin::run_app::<App>(include_str!("../../../edgezero.toml"), req).await
/// }
/// ```
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub async fn run_app<A: edgezero_core::app::Hooks>(
    manifest_src: &str,
    req: spin_sdk::http::IncomingRequest,
) -> anyhow::Result<impl spin_sdk::http::IntoResponse> {
    // Use `let _ =` instead of `.expect()` because Spin calls
    // `#[http_component]` per-request. Once a real logger is wired in,
    // `log::set_logger` returns Err on the second call — `.expect()`
    // would panic on every subsequent request.
    let _ = init_logger();
    let manifest_loader = edgezero_core::manifest::ManifestLoader::load_from_str(manifest_src);
    let settings = resolve_store_settings(manifest_loader.manifest(), A::config_store().is_some());
    let app = A::build_app();
    request::dispatch_with_store_settings(&app, req, &settings).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::manifest::{ManifestLoader, DEFAULT_KV_STORE_NAME};

    fn resolve_settings(src: &str, hook_has_config_store: bool) -> SpinStoreSettings {
        let manifest = ManifestLoader::load_from_str(src);
        resolve_store_settings(manifest.manifest(), hook_has_config_store)
    }

    #[test]
    fn store_settings_default_to_optional_kv_without_config_or_secrets() {
        let settings = resolve_settings("", false);

        assert_eq!(settings.kv_label, DEFAULT_KV_STORE_NAME);
        assert!(!settings.kv_required);
        assert!(!settings.config_enabled);
        assert!(!settings.secrets_enabled);
    }

    #[test]
    fn store_settings_resolve_spin_manifest_declarations() {
        let settings = resolve_settings(
            r#"
[stores.kv]
ids = ["SPIN_KV", "cache"]
default = "SPIN_KV"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#,
            false,
        );

        assert_eq!(settings.kv_label, "SPIN_KV");
        assert!(settings.kv_required);
        assert!(settings.config_enabled);
        assert!(settings.secrets_enabled);
    }

    #[test]
    fn store_settings_honor_hook_config_metadata_without_manifest_config_section() {
        let settings = resolve_settings("", true);

        assert!(settings.config_enabled);
    }
}
