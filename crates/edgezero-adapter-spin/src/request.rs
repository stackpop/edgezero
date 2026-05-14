use std::sync::Arc;

use crate::config_store::SpinConfigStore;
use crate::context::SpinRequestContext;
use crate::key_value_store::SpinKvStore;
use crate::proxy::SpinProxyClient;
use crate::response::from_core_response;
use crate::secret_store::SpinSecretStore;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request, Uri};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::secret_store::SecretHandle;
use spin_sdk::http::IncomingRequest;

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
/// This is a convenience wrapper around [`dispatch_with_kv_label`]. Use that
/// function directly when your `spin.toml` declares a KV store under a label
/// other than `"default"` (e.g. because `[stores.kv.adapters.spin].name` in
/// `edgezero.toml` is set to a custom value).
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
/// Pass the label that matches your `spin.toml` `key_value_stores` entry.
/// If `[stores.kv.adapters.spin].name` in `edgezero.toml` is `"my-store"`,
/// that same string must appear in `spin.toml` and must be passed here.
pub async fn dispatch_with_kv_label(
    app: &App,
    req: IncomingRequest,
    kv_label: &str,
) -> anyhow::Result<spin_sdk::http::Response> {
    let mut core_request = into_core_request(req).await?;

    core_request
        .extensions_mut()
        .insert(ConfigStoreHandle::new(Arc::new(SpinConfigStore::new())));

    match SpinKvStore::open(kv_label) {
        Ok(store) => {
            core_request
                .extensions_mut()
                .insert(KvHandle::new(Arc::new(store)));
        }
        Err(e) => {
            log::warn!(
                "SpinKvStore: could not open KV store (label {:?}); \
                 KV operations will be unavailable: {e}",
                kv_label
            );
        }
    }

    core_request
        .extensions_mut()
        .insert(SecretHandle::new(Arc::new(SpinSecretStore::new())));

    let response = app.router().oneshot(core_request).await;
    Ok(from_core_response(response).await?)
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
