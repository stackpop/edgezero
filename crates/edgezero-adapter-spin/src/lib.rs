//! Adapter helpers for Spin (Fermyon).

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
pub mod config_store;
pub mod context;
mod decompress;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod proxy;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod request;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod response;

// SpinKvStore and SpinSecretStore import spin_sdk types at the module level
// and therefore require `all(feature = "spin", target_arch = "wasm32")`.
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod key_value_store;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub mod secret_store;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::future::Future;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use core::pin::Pin;
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::app::SPIN_ADAPTER;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::app::{App, Hooks};
#[cfg(any(test, all(feature = "spin", target_arch = "wasm32")))]
use edgezero_core::manifest::Manifest;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use edgezero_core::manifest::ManifestLoader;
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
use spin_sdk::http::{IncomingRequest, IntoResponse, Response as SpinResponse};

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub trait AppExt {
    fn dispatch<'app>(
        &'app self,
        req: IncomingRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinResponse>> + 'app>>;
}

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
impl AppExt for App {
    #[inline]
    fn dispatch<'app>(
        &'app self,
        req: IncomingRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpinResponse>> + 'app>> {
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
/// Never; this is currently a no-op because Spin manages logging
/// internally. The signature still returns [`log::SetLoggerError`] so
/// the future "wire in a real logger" branch stays drop-in compatible.
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
/// incoming Spin request through the `EdgeZero` router, and return the
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
///
/// # Errors
/// Returns [`anyhow::Error`] when the manifest cannot be parsed or the
/// inner dispatch fails (transport, router, store binding, or response
/// translation errors propagate here).
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[inline]
pub async fn run_app<A: Hooks>(
    manifest_src: &str,
    req: IncomingRequest,
) -> anyhow::Result<impl IntoResponse> {
    // Best-effort: every Spin `#[http_component]` re-enters this function, so
    // a second `log::set_logger` call returns Err — drop the result instead
    // of `.expect()` to avoid panicking on every subsequent request.
    drop(init_logger());
    let manifest_loader = ManifestLoader::load_from_str(manifest_src);
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

        assert_eq!(settings.kv_label, DEFAULT_KV_STORE_NAME, "default kv label");
        assert!(!settings.kv_required, "kv not required by default");
        assert!(!settings.config_enabled, "config disabled by default");
        assert!(!settings.secrets_enabled, "secrets disabled by default");
    }

    #[test]
    fn store_settings_resolve_spin_manifest_overrides() {
        let settings = resolve_settings(
            r#"
[stores.kv]
name = "GLOBAL_KV"

[stores.kv.adapters.spin]
name = "SPIN_KV"

[stores.config]

[stores.secrets]
enabled = false

[stores.secrets.adapters.spin]
enabled = true
"#,
            false,
        );

        assert_eq!(settings.kv_label, "SPIN_KV", "spin override applied");
        assert!(settings.kv_required, "kv required by manifest");
        assert!(settings.config_enabled, "config enabled by manifest");
        assert!(
            settings.secrets_enabled,
            "secrets enabled via spin per-adapter override"
        );
    }

    #[test]
    fn store_settings_honor_hook_config_metadata_without_manifest_config_section() {
        let settings = resolve_settings("", true);

        assert!(
            settings.config_enabled,
            "config enabled because hook provided metadata"
        );
    }
}
