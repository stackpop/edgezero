use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config_store::SpinConfigStore;
use crate::context::SpinRequestContext;
use crate::key_value_store::{SpinKvStore, DEFAULT_MAX_LIST_KEYS};
use crate::proxy::SpinProxyClient;
use crate::response::from_core_response;
use crate::secret_store::SpinSecretStore;
use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request, Uri};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, KvRegistry, SecretRegistry, StoreRegistry,
};
use spin_sdk::http::IncomingRequest;

#[derive(Default)]
pub(crate) struct Stores {
    pub(crate) config_registry: Option<ConfigRegistry>,
    pub(crate) config_store: Option<ConfigStoreHandle>,
    pub(crate) kv: Option<KvHandle>,
    pub(crate) kv_registry: Option<KvRegistry>,
    pub(crate) secret_registry: Option<SecretRegistry>,
    pub(crate) secrets: Option<SecretHandle>,
}

/// Convert a Spin `IncomingRequest` into an EdgeZero core `Request`.
///
/// Reads the full body into a buffered `Body::Once`, inserts
/// `SpinRequestContext` and a `ProxyHandle` into extensions.
pub async fn into_core_request(req: IncomingRequest) -> Result<Request, EdgeError> {
    let method = req.method();
    let path_with_query = req.path_with_query().unwrap_or_else(|| "/".to_string());

    let uri: Uri = path_with_query
        .parse()
        .map_err(|err| EdgeError::bad_request(format!("invalid URI: {}", err)))?;

    // Extract headers before consuming the request body. The WASI `headers()`
    // handle borrows the request and must be dropped before `into_body()`.
    let headers = req.headers();
    let header_entries = headers.entries();

    let mut builder = request_builder()
        .method(into_core_method(&method)?)
        .uri(uri);

    for (name, value) in &header_entries {
        match edgezero_core::http::HeaderValue::from_bytes(value) {
            Ok(hval) => {
                builder = builder.header(name.as_str(), hval);
            }
            Err(_) => {
                log::warn!("dropping invalid request header value: {}", name);
            }
        }
    }

    let client_addr = find_header_string(&header_entries, "spin-client-addr")
        .and_then(|raw| crate::context::parse_client_addr(&raw));
    let full_url = find_header_string(&header_entries, "spin-full-url");

    // Drop the WASI resource handle before consuming the body.
    drop(headers);

    // Inbound body size is not capped at the adapter level. The Spin runtime
    // enforces its own request body limit (configurable via `spin.toml`), which
    // is consistent with how the Fastly and Cloudflare adapters delegate inbound
    // size enforcement to their respective platform runtimes.
    let body_bytes = req
        .into_body()
        .await
        .map_err(|e| EdgeError::bad_request(format!("failed to read request body: {}", e)))?;

    let mut request = builder
        .body(Body::from(body_bytes))
        .map_err(|e| EdgeError::bad_request(format!("failed to build request: {}", e)))?;

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

/// Find a header value by name from pre-extracted header entries.
fn find_header_string(entries: &[(String, Vec<u8>)], name: &str) -> Option<String> {
    entries
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
}

/// Dispatch a Spin request through the EdgeZero router using the `"default"`
/// KV store label.
///
/// This is a low-level manual path. It does not read `EDGEZERO__*` environment
/// config and therefore does not honor baked store metadata for KV, config, or
/// secret stores. Prefer [`crate::run_app`] for normal dispatch.
pub async fn dispatch(app: &App, req: IncomingRequest) -> anyhow::Result<spin_sdk::http::Response> {
    dispatch_with_kv_label(app, req, "default").await
}

/// Dispatch a Spin request through the EdgeZero router and return
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
pub async fn dispatch_with_kv_label(
    app: &App,
    req: IncomingRequest,
    kv_label: &str,
) -> anyhow::Result<spin_sdk::http::Response> {
    let stores = Stores {
        config_store: resolve_config_handle(true),
        kv: resolve_kv_handle(kv_label, false)?,
        secrets: resolve_secret_handle(true),
        ..Default::default()
    };
    dispatch_with_handles(app, req, stores).await
}

pub(crate) async fn dispatch_with_handles(
    app: &App,
    req: IncomingRequest,
    stores: Stores,
) -> anyhow::Result<spin_sdk::http::Response> {
    let mut core_request = into_core_request(req).await?;
    if let Some(registry) = stores.config_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(handle) = stores.config_store {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(registry) = stores.kv_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(handle) = stores.kv {
        core_request.extensions_mut().insert(handle);
    }
    if let Some(registry) = stores.secret_registry {
        core_request.extensions_mut().insert(registry);
    }
    if let Some(handle) = stores.secrets {
        core_request.extensions_mut().insert(handle);
    }
    let response = app.router().oneshot(core_request).await?;
    Ok(from_core_response(response).await?)
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Spin capability map (§6.6):
/// - KV: **Multi** — each declared id opens its own [`SpinKvStore`] under the
///   label resolved from `EDGEZERO__STORES__KV__<ID>__NAME`. Optional
///   `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS` overrides the paging cap.
/// - Config: **Single** — every declared id maps to the one shared
///   [`SpinConfigStore`] (flat variable namespace).
/// - Secrets: **Single** — every declared id maps to the one shared
///   [`SpinSecretStore`] (same flat namespace).
pub(crate) async fn dispatch_with_registries(
    app: &App,
    req: IncomingRequest,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> anyhow::Result<spin_sdk::http::Response> {
    let kv_registry = build_kv_registry(kv_meta, env)?;
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

fn build_kv_registry(
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
        match SpinKvStore::open_with_max_list_keys(&label, max_list_keys) {
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
    if by_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(StoreRegistry::new(by_id, meta.default.to_owned())))
}

fn build_config_registry(config_meta: Option<StoreMetadata>) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    // Spin is `Single` for config: every id resolves to the same flat variable store.
    let handle = ConfigStoreHandle::new(Arc::new(SpinConfigStore::new()));
    let mut by_id: BTreeMap<String, ConfigStoreHandle> = BTreeMap::new();
    for id in meta.ids {
        by_id.insert((*id).to_owned(), handle.clone());
    }
    Some(StoreRegistry::new(by_id, meta.default.to_owned()))
}

fn build_secret_registry(
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Spin is `Single` for secrets: every id resolves to the same flat
    // variable store. `SpinSecretStore::get_bytes` ignores `store_name`
    // (logs a debug if non-empty per §6.7), so the per-id bound name is
    // observable only via [`BoundSecretStore::store_name`].
    let handle = SecretHandle::new(Arc::new(SpinSecretStore::new()));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    Some(StoreRegistry::new(by_id, meta.default.to_owned()))
}

fn resolve_config_handle(config_enabled: bool) -> Option<ConfigStoreHandle> {
    if !config_enabled {
        return None;
    }
    Some(ConfigStoreHandle::new(Arc::new(SpinConfigStore::new())))
}

fn resolve_kv_handle(kv_label: &str, kv_required: bool) -> anyhow::Result<Option<KvHandle>> {
    match SpinKvStore::open(kv_label) {
        Ok(store) => Ok(Some(KvHandle::new(Arc::new(store)))),
        Err(e) => {
            if kv_required {
                return Err(anyhow::anyhow!(
                    "Spin KV store '{}' is explicitly configured but could not be opened: {}",
                    kv_label,
                    e
                ));
            }
            log::warn!(
                "SpinKvStore: could not open KV store (label {:?}); \
                 KV operations will be unavailable: {e}",
                kv_label
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

fn into_core_method(
    method: &spin_sdk::http::Method,
) -> Result<edgezero_core::http::Method, EdgeError> {
    match method {
        spin_sdk::http::Method::Get => Ok(edgezero_core::http::Method::GET),
        spin_sdk::http::Method::Post => Ok(edgezero_core::http::Method::POST),
        spin_sdk::http::Method::Put => Ok(edgezero_core::http::Method::PUT),
        spin_sdk::http::Method::Delete => Ok(edgezero_core::http::Method::DELETE),
        spin_sdk::http::Method::Patch => Ok(edgezero_core::http::Method::PATCH),
        spin_sdk::http::Method::Head => Ok(edgezero_core::http::Method::HEAD),
        spin_sdk::http::Method::Options => Ok(edgezero_core::http::Method::OPTIONS),
        spin_sdk::http::Method::Connect => Ok(edgezero_core::http::Method::CONNECT),
        spin_sdk::http::Method::Trace => Ok(edgezero_core::http::Method::TRACE),
        spin_sdk::http::Method::Other(s) => edgezero_core::http::Method::from_bytes(s.as_bytes())
            .map_err(|_| EdgeError::bad_request(format!("unsupported HTTP method: {s}"))),
    }
}
