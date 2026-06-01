use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config_store::SpinConfigStore;
use crate::context::{parse_client_addr, SpinRequestContext};
use crate::key_value_store::{SpinKvStore, DEFAULT_MAX_LIST_KEYS};
use crate::proxy::SpinProxyClient;
use crate::response::from_core_response;
use crate::secret_store::SpinSecretStore;
use crate::SpinFullResponse;
use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, KvRegistry, SecretRegistry, StoreRegistry,
};
use spin_sdk::http::body::IncomingBodyExt as _;
use spin_sdk::http::Request as SpinRequest;

/// Per-dispatch store wiring assembled before the request enters the router.
/// The struct itself is `pub(crate)` because `dispatch_with_handles` takes it
/// by value, but fields are constructed only inside this module so they stay
/// private and the field-scoped-visibility lint does not fire.
#[derive(Default)]
pub(crate) struct Stores {
    config_registry: Option<ConfigRegistry>,
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    secret_registry: Option<SecretRegistry>,
    secrets: Option<SecretHandle>,
}

/// Convert a Spin `Request` into an `EdgeZero` core `Request`.
///
/// Reads the full body into a buffered `Body::Once`, inserts
/// `SpinRequestContext` and a `ProxyHandle` into extensions.
///
/// # Errors
/// Returns [`EdgeError::bad_request`] if the request body cannot be read or
/// the core `Request` cannot be built from the resulting parts.
#[inline]
pub async fn into_core_request(req: SpinRequest) -> Result<Request, EdgeError> {
    let (parts, body) = req.into_parts();

    let client_addr = parts
        .headers
        .get("spin-client-addr")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_client_addr);
    let full_url = parts
        .headers
        .get("spin-full-url")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut builder = request_builder().method(parts.method).uri(parts.uri);
    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }

    // Inbound body size is not capped at the adapter level. The Spin runtime
    // enforces its own request body limit (configurable via `spin.toml`), which
    // is consistent with how the Fastly and Cloudflare adapters delegate inbound
    // size enforcement to their respective platform runtimes.
    let body_bytes = body
        .bytes()
        .await
        .map_err(|err| EdgeError::bad_request(format!("failed to read request body: {err}")))?;

    let mut request = builder
        .body(Body::from(body_bytes.to_vec()))
        .map_err(|err| EdgeError::bad_request(format!("failed to build request: {err}")))?;

    SpinRequestContext::insert(
        &mut request,
        SpinRequestContext {
            client_addr,
            full_url,
        },
    );
    request
        .extensions_mut()
        .insert(ProxyHandle::with_client(SpinProxyClient));

    Ok(request)
}

/// Dispatch a Spin request through the `EdgeZero` router using the `"default"`
/// KV store label.
///
/// This is a low-level manual path. It does not read `EDGEZERO__*` environment
/// config and therefore does not honor baked store metadata for KV, config, or
/// secret stores. Prefer [`crate::run_app`] for normal dispatch.
///
/// # Errors
/// Returns [`anyhow::Error`] if KV open fails for `"default"`, the request
/// cannot be converted, the router dispatch fails, or response translation
/// fails.
#[inline]
pub async fn dispatch(app: &App, req: SpinRequest) -> anyhow::Result<SpinFullResponse> {
    dispatch_with_kv_label(app, req, "default").await
}

/// Dispatch a Spin request through the `EdgeZero` router and return
/// a Spin-compatible response, opening the KV store under `kv_label`.
///
/// Injects all available stores into request extensions:
/// - `ConfigStoreHandle` backed by `SpinConfigStore` (Spin component variables)
/// - `KvHandle` backed by `SpinKvStore` opened on `kv_label` (best-effort;
///   logged and omitted if the label is not declared in `spin.toml`)
/// - `SecretHandle` backed by `SpinSecretStore` (Spin component variables)
///
/// Pass the label that matches your `spin.toml` `key_value_stores` entry —
/// the same value `EDGEZERO__STORES__KV__<ID>__NAME` resolves to at runtime.
///
/// # Errors
/// Returns [`anyhow::Error`] if KV open fails when the store is required,
/// the request cannot be converted, the router dispatch fails, or response
/// translation fails.
#[inline]
pub async fn dispatch_with_kv_label(
    app: &App,
    req: SpinRequest,
    kv_label: &str,
) -> anyhow::Result<SpinFullResponse> {
    let stores = Stores {
        config_store: resolve_config_handle(true),
        kv: resolve_kv_handle(kv_label, false).await?,
        secrets: resolve_secret_handle(true),
        ..Default::default()
    };
    dispatch_with_handles(app, req, stores).await
}

pub(crate) async fn dispatch_with_handles(
    app: &App,
    req: SpinRequest,
    stores: Stores,
) -> anyhow::Result<SpinFullResponse> {
    let mut core_request = into_core_request(req).await?;
    // Hard-cutoff: see fastly's `dispatch_core_request`
    // for the rationale. Only registries go into extensions —
    // legacy bare handles are synthesised into a one-id registry
    // at the dispatch boundary.
    let (config_registry, kv_registry, secret_registry) = synthesise_store_registries(stores);
    if let Some(registry) = config_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(registry) = kv_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(registry) = secret_registry {
        core_request.extensions_mut().insert(registry);
    }
    let response = app.router().oneshot(core_request).await?;
    Ok(from_core_response(response).await?)
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Spin capability map:
/// - KV: **Multi** — each declared id opens its own [`SpinKvStore`] under the
///   label resolved from `EDGEZERO__STORES__KV__<ID>__NAME`. Optional
///   `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS` overrides the paging cap.
/// - Config: **Single** — every declared id maps to the one shared
///   [`SpinConfigStore`] (flat variable namespace).
/// - Secrets: **Single** — every declared id maps to the one shared
///   [`SpinSecretStore`] (same flat namespace).
pub(crate) async fn dispatch_with_registries(
    app: &App,
    req: SpinRequest,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> anyhow::Result<SpinFullResponse> {
    let kv_registry = build_kv_registry(kv_meta, env).await?;
    let config_registry = build_config_registry(config_meta);
    let secret_registry = build_secret_registry(secret_meta, env);
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Default::default()
        },
    )
    .await
}

/// Pure synthesis: collapse a `Stores` (which may carry both a
/// wired multi-id registry AND a legacy bare handle) into the
/// three registries that go into request extensions. Precedence
/// is "registry wins": a wired registry is taken verbatim; only
/// in its absence is a bare handle wrapped into a one-id registry
/// keyed under `"default"`. Pulled out as a pure function so the
/// precedence contract is unit-testable without spinning up a
/// real Spin `Request` and async dispatcher.
fn synthesise_store_registries(
    stores: Stores,
) -> (
    Option<ConfigRegistry>,
    Option<KvRegistry>,
    Option<SecretRegistry>,
) {
    let config_registry = stores.config_registry.or_else(|| {
        stores
            .config_store
            .map(|handle| ConfigRegistry::single_id("default".to_owned(), handle))
    });
    let kv_registry = stores.kv_registry.or_else(|| {
        stores
            .kv
            .map(|handle| KvRegistry::single_id("default".to_owned(), handle))
    });
    let secret_registry = stores.secret_registry.or_else(|| {
        stores.secrets.map(|handle| {
            SecretRegistry::single_id(
                "default".to_owned(),
                BoundSecretStore::new(handle, "default".to_owned()),
            )
        })
    });
    (config_registry, kv_registry, secret_registry)
}

async fn build_kv_registry(
    kv_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> anyhow::Result<Option<KvRegistry>> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };
    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let label = env.store_name("kv", id);
        let max_list_keys = env
            .store_setting("kv", id, "MAX_LIST_KEYS")
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_LIST_KEYS);
        match SpinKvStore::open_with_max_list_keys(&label, max_list_keys).await {
            Ok(store) => {
                by_id.insert((*id).to_owned(), KvHandle::new(Arc::new(store)));
            }
            Err(err) => {
                // Required: `[stores.kv]` is declared, so a missing label is a
                // configuration error rather than a silent degradation.
                return Err(anyhow::anyhow!(
                    "Spin KV store '{label}' (id `{id}`) is explicitly configured but could not be opened: {err}"
                ));
            }
        }
    }
    // For Spin, KV open is required — any failure already returns Err
    // above, so the default id is guaranteed to be in `by_id` here.
    // `from_parts` keeps the API symmetric with the other adapters.
    Ok(StoreRegistry::from_parts(by_id, meta.default.to_owned()))
}

fn build_config_registry(config_meta: Option<StoreMetadata>) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    // Spin is `Single` for config: every id resolves to the same flat
    // variable store. Construction is infallible, so the default id is
    // always present in `by_id`.
    let handle = ConfigStoreHandle::new(Arc::new(SpinConfigStore::new()));
    let mut by_id: BTreeMap<String, ConfigStoreHandle> = BTreeMap::new();
    for id in meta.ids {
        by_id.insert((*id).to_owned(), handle.clone());
    }
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

fn build_secret_registry(
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Spin is `Single` for secrets: every id resolves to the same flat
    // variable store. `SpinSecretStore::get_bytes` ignores `store_name`
    // (logs a debug if non-empty), so the per-id bound name is
    // observable only via [`BoundSecretStore::store_name`]. Construction
    // is infallible.
    let handle = SecretHandle::new(Arc::new(SpinSecretStore::new()));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

fn resolve_config_handle(config_enabled: bool) -> Option<ConfigStoreHandle> {
    if !config_enabled {
        return None;
    }
    Some(ConfigStoreHandle::new(Arc::new(SpinConfigStore::new())))
}

async fn resolve_kv_handle(kv_label: &str, kv_required: bool) -> anyhow::Result<Option<KvHandle>> {
    match SpinKvStore::open(kv_label).await {
        Ok(store) => Ok(Some(KvHandle::new(Arc::new(store)))),
        Err(err) => {
            if kv_required {
                return Err(anyhow::anyhow!(
                    "Spin KV store '{kv_label}' is explicitly configured but could not be opened: {err}"
                ));
            }
            log::warn!(
                "SpinKvStore: could not open KV store (label {kv_label:?}); \
                 KV operations will be unavailable: {err}"
            );
            Ok(None)
        }
    }
}

fn resolve_secret_handle(secrets_enabled: bool) -> Option<SecretHandle> {
    if !secrets_enabled {
        return None;
    }
    Some(SecretHandle::new(Arc::new(SpinSecretStore::new())))
}

#[cfg(test)]
mod synthesis_tests {
    use super::*;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::key_value_store::{KvStore, NoopKvStore};
    use edgezero_core::secret_store::{NoopSecretStore, SecretHandle};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    struct StubConfig;
    #[async_trait::async_trait(?Send)]
    impl ConfigStore for StubConfig {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(None)
        }
    }

    fn kv_handle() -> KvHandle {
        let store: Arc<dyn KvStore> = Arc::new(NoopKvStore);
        KvHandle::new(store)
    }

    fn config_handle() -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(StubConfig))
    }

    fn secret_handle() -> SecretHandle {
        SecretHandle::new(Arc::new(NoopSecretStore))
    }

    #[test]
    fn synthesis_wraps_bare_kv_handle_under_default_when_no_registry() {
        let stores = Stores {
            kv: Some(kv_handle()),
            ..Default::default()
        };
        let (config, kv, secret) = synthesise_store_registries(stores);
        assert!(config.is_none(), "no config registry without input");
        assert!(secret.is_none(), "no secret registry without input");
        let kv_registry = kv.expect("kv registry synthesised");
        assert_eq!(
            kv_registry.default_id(),
            "default",
            "bare kv keyed under default"
        );
        assert!(
            kv_registry.named("other").is_none(),
            "no other id synthesised"
        );
    }

    #[test]
    fn synthesis_registry_wins_over_bare_handle_when_both_wired() {
        let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
        by_id.insert("sessions".to_owned(), kv_handle());
        let registry = KvRegistry::new(by_id, "sessions".to_owned());
        let stores = Stores {
            kv: Some(kv_handle()),
            kv_registry: Some(registry),
            ..Default::default()
        };
        let (_, kv, _) = synthesise_store_registries(stores);
        let kv_registry = kv.expect("registry survives");
        assert_eq!(kv_registry.default_id(), "sessions", "wired default wins");
        assert!(
            kv_registry.named("default").is_none(),
            "bare handle's `default` synth NOT merged in"
        );
    }

    #[test]
    fn synthesis_returns_none_for_each_kind_with_no_wiring() {
        let (config, kv, secret) = synthesise_store_registries(Stores::default());
        assert!(
            config.is_none() && kv.is_none() && secret.is_none(),
            "all registries empty"
        );
    }

    #[test]
    fn synthesis_handles_config_and_secret_bare_handles_symmetrically() {
        let stores = Stores {
            config_store: Some(config_handle()),
            secrets: Some(secret_handle()),
            ..Default::default()
        };
        let (config, _, secret) = synthesise_store_registries(stores);
        assert_eq!(
            config.expect("config").default_id(),
            "default",
            "config synth under default"
        );
        let secret_registry = secret.expect("secret");
        assert_eq!(
            secret_registry.default_id(),
            "default",
            "secret synth under default"
        );
        // BoundSecretStore binds the synthesised secret to platform
        // store name "default". A handler reading via
        // `ctx.secret_store_default()?.require_str(key)` resolves
        // the spin variable literally named "default"; if the
        // operator's spin.toml uses a different name, the runtime
        // require_str() surfaces a clear variable-name error
        // rather than a silent miss.
        assert_eq!(
            secret_registry.default().expect("bound").store_name(),
            "default",
            "bound name copied verbatim"
        );
    }
}
