use std::collections::BTreeMap;
use std::sync::Arc;
#[cfg(feature = "test-utils")]
use std::{
    io,
    sync::atomic::{AtomicUsize, Ordering},
    task::Poll,
};

use anyhow::Context as _;
#[cfg(feature = "test-utils")]
use bytes::Bytes;

use crate::SpinFullResponse;
use crate::config_store::SpinConfigStore;
use crate::context::{SpinRequestContext, parse_client_addr};
use crate::key_value_store::{DEFAULT_MAX_LIST_KEYS, SpinKvStore};
use crate::response::from_egress_response;
use crate::secret_store::SpinSecretStore;
use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
#[cfg(feature = "test-utils")]
use edgezero_core::http::Uri;
#[cfg(any(test, feature = "test-utils"))]
use edgezero_core::http::{Method, request_builder};
use edgezero_core::http::{Request, RequestParts};
use edgezero_core::ingress::{
    IngressBeginOutcome, IngressFraming, IngressHeadAccounting, IngressHeadParts, PreparedIngress,
};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::outbound::HttpClient;
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry, StoreRegistry,
};
use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
#[cfg(any(test, feature = "test-utils"))]
use futures::executor::block_on;
#[cfg(feature = "test-utils")]
use futures_util::stream::poll_fn;
use futures_util::stream::unfold;
use futures_util::{StreamExt as _, TryStreamExt as _};
use spin_sdk::http::Request as SpinRequest;
use spin_sdk::http::body::IncomingBodyExt as _;

use crate::outbound::SpinOutboundClient;

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

#[cfg(feature = "test-utils")]
struct DropSignal(Arc<AtomicUsize>);

#[cfg(feature = "test-utils")]
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Convert a Spin `Request` into an `EdgeZero` core `Request`.
///
/// Preserves the body as a lazy stream, inserts `SpinRequestContext`, and adds
/// an outbound [`HttpClient`] to extensions.
///
/// # Errors
/// Returns [`EdgeError::bad_request`] if the request body cannot be read or
/// the core `Request` cannot be built from the resulting parts.
#[inline]
#[expect(
    clippy::unused_async,
    reason = "the public converter retains its established async API while request bodies remain lazy"
)]
pub async fn into_core_request(req: SpinRequest) -> Result<Request, EdgeError> {
    let (parts, body) = req.into_parts();
    let mut request = into_core_request_head(parts, MonotonicClock::default());
    *request.body_mut() = Body::from_external_stream(body.stream());
    Ok(request)
}

fn into_core_request_head(parts: RequestParts, outbound_clock: MonotonicClock) -> Request {
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

    let mut request = Request::from_parts(parts, Body::empty());

    SpinRequestContext::insert(
        &mut request,
        SpinRequestContext {
            client_addr,
            full_url,
        },
    );
    request
        .extensions_mut()
        .insert(outbound_client(outbound_clock));

    request
}

fn into_core_request_head_for_app(parts: RequestParts, app: &App) -> Request {
    into_core_request_head(parts, app.monotonic_clock())
}

fn outbound_client(clock: MonotonicClock) -> HttpClient {
    HttpClient::with_client(SpinOutboundClient::with_clock(clock))
}

fn spin_deadline_body<Source, SourceError>(
    source: Source,
    deadline: Deadline,
    monotonic_clock: MonotonicClock,
) -> Body
where
    Source: futures_util::Stream<Item = Result<bytes::Bytes, SourceError>> + 'static,
    SourceError: Into<anyhow::Error> + 'static,
{
    let boxed_stream = source.map_err(Into::into).boxed_local();
    let stream = unfold(Some(boxed_stream), move |stream_state| {
        let clock = monotonic_clock.clone();
        async move {
            let mut body_stream = stream_state?;
            if deadline.is_expired_at(clock.now()) {
                return Some((
                    Err(EdgeError::request_timeout(
                        "inbound body read deadline exceeded",
                    )),
                    None,
                ));
            }
            let item = body_stream.next().await;
            if deadline.is_expired_at(clock.now()) {
                return Some((
                    Err(EdgeError::request_timeout(
                        "inbound body read deadline exceeded",
                    )),
                    None,
                ));
            }
            match item {
                Some(Ok(bytes)) => Some((Ok(bytes), Some(body_stream))),
                Some(Err(error)) => Some((Err(EdgeError::internal(error)), None)),
                None => None,
            }
        }
    });
    Body::from_stream(stream)
}

/// Runs the terminal source-release probe used by the WASM contract suite.
#[cfg(feature = "test-utils")]
#[must_use]
#[inline]
pub fn deadline_body_releases_source_for_test() -> bool {
    let dropped = Arc::new(AtomicUsize::new(0));
    let signal = DropSignal(Arc::clone(&dropped));
    let source = poll_fn(move |_cx| {
        let _keep_alive = &signal;
        Poll::<Option<Result<Bytes, io::Error>>>::Pending
    });
    let start = MonotonicInstant::now();
    let clock = MonotonicClock::new(move || start);
    let body = spin_deadline_body(source, Deadline::at_instant(start), clock);
    let Some(mut body_stream) = body.into_stream() else {
        return false;
    };
    let Some(Err(error)) = block_on(body_stream.next()) else {
        return false;
    };
    matches!(error, EdgeError::RequestTimeout { .. }) && dropped.load(Ordering::SeqCst) == 1
}

/// Dispatches an observable source through the production ingress body wrapper.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
#[inline]
pub async fn dispatch_ingress_stream_for_test<Source, SourceError>(
    app: &App,
    method: Method,
    uri: Uri,
    source: Source,
) -> anyhow::Result<SpinFullResponse>
where
    Source: futures_util::Stream<Item = Result<Bytes, SourceError>> + 'static,
    SourceError: Into<anyhow::Error> + 'static,
{
    let request_start = app.monotonic_now();
    let core_request = request_builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .map_err(EdgeError::internal)?;
    dispatch_ingress_stream(
        app,
        core_request,
        Stores::default(),
        request_start,
        move || source,
    )
    .await
}

/// Dispatches a native Spin request through the production outer request seam.
///
/// # Errors
/// Returns the same request-conversion, routing, or response-conversion error as standard dispatch.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
#[inline]
pub async fn dispatch_request_for_test(
    app: &App,
    req: SpinRequest,
) -> anyhow::Result<SpinFullResponse> {
    dispatch_with_handles(app, req, Stores::default(), app.monotonic_now()).await
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
    let request_start = app.monotonic_now();
    dispatch_with_kv_label_at(app, req, "default", request_start).await
}

/// Dispatch a Spin request through the `EdgeZero` router and return
/// a Spin-compatible response, opening the KV store under `kv_label`.
///
/// Injects all available stores into request extensions:
/// - `ConfigStoreHandle` backed by `SpinConfigStore` opened on `kv_label`
///   (KV-backed since 2026-Q3; was variables-backed before that). The
///   same label opens the config-store backend as the KV store on this
///   low-level path — registry-aware callers should use [`run_app`]
///   instead, which resolves per-id labels from `EDGEZERO__STORES__CONFIG__<ID>__NAME`.
/// - `KvHandle` backed by `SpinKvStore` opened on `kv_label` (best-effort;
///   logged and omitted if the label is not declared in `spin.toml`)
/// - `SecretHandle` backed by `SpinSecretStore` (Spin component variables)
///
/// Pass the label that matches your `spin.toml` `key_value_stores` entry —
/// the same value `EDGEZERO__STORES__KV__<ID>__NAME` resolves to at runtime.
///
/// # Errors
/// Returns [`anyhow::Error`] if KV open fails when the store is required,
/// if the config-store open fails, if the request cannot be converted, if
/// the router dispatch fails, or if response translation fails.
#[inline]
pub async fn dispatch_with_kv_label(
    app: &App,
    req: SpinRequest,
    kv_label: &str,
) -> anyhow::Result<SpinFullResponse> {
    let request_start = app.monotonic_now();
    dispatch_with_kv_label_at(app, req, kv_label, request_start).await
}

async fn dispatch_with_kv_label_at(
    app: &App,
    req: SpinRequest,
    kv_label: &str,
    request_start: MonotonicInstant,
) -> anyhow::Result<SpinFullResponse> {
    let stores = Stores {
        config_store: resolve_config_handle(kv_label).await?,
        kv: resolve_kv_handle(kv_label, false).await?,
        secrets: resolve_secret_handle(true),
        ..Default::default()
    };
    dispatch_with_handles(app, req, stores, request_start).await
}

pub(crate) async fn dispatch_with_handles(
    app: &App,
    req: SpinRequest,
    stores: Stores,
    request_start: MonotonicInstant,
) -> anyhow::Result<SpinFullResponse> {
    let (parts, native_body) = req.into_parts();
    let core_request = into_core_request_head_for_app(parts, app);
    dispatch_ingress_stream(app, core_request, stores, request_start, move || {
        native_body.stream()
    })
    .await
}

async fn dispatch_ingress_stream<Source, SourceError, MakeSource>(
    app: &App,
    mut head_request: Request,
    stores: Stores,
    request_start: MonotonicInstant,
    make_source: MakeSource,
) -> anyhow::Result<SpinFullResponse>
where
    Source: futures_util::Stream<Item = Result<bytes::Bytes, SourceError>> + 'static,
    SourceError: Into<anyhow::Error> + 'static,
    MakeSource: FnOnce() -> Source,
{
    head_request
        .extensions_mut()
        .insert(outbound_client(app.monotonic_clock()));
    let head_parts = IngressHeadParts::from_request(
        &head_request,
        IngressHeadAccounting::HostManaged,
        IngressFraming::HostManaged,
    );
    head_parts.validate_normalized(app.ingress_head_limits())?;
    let prepared = match app.begin_ingress(head_parts, request_start)? {
        IngressBeginOutcome::Admitted(prepared) => prepared,
        IngressBeginOutcome::Refused(response) => {
            return Ok(from_egress_response(response).await?);
        }
        _ => return Err(anyhow::anyhow!("unsupported ingress admission outcome")),
    };
    *head_request.body_mut() = spin_deadline_body(
        make_source(),
        prepared.read_deadline(),
        prepared.monotonic_clock(),
    );
    dispatch_core_request(app, head_request, stores, prepared).await
}

async fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    stores: Stores,
    prepared: PreparedIngress,
) -> anyhow::Result<SpinFullResponse> {
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
    let response = app.dispatch_admitted(prepared, core_request).await?;
    Ok(from_egress_response(response).await?)
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Spin capability map:
/// - KV: **Multi** — each declared id opens its own [`SpinKvStore`] under the
///   label resolved from `EDGEZERO__STORES__KV__<ID>__NAME`. Optional
///   `EDGEZERO__STORES__KV__<ID>__MAX_LIST_KEYS` overrides the paging cap.
/// - Config: **Multi** — each declared id opens its own [`SpinConfigStore`]
///   under the label resolved from `EDGEZERO__STORES__CONFIG__<ID>__NAME`.
///   KV-backed under the hood (was variables-backed up through 2026-Q2).
/// - Secrets: **Single** — every declared id maps to the one shared
///   [`SpinSecretStore`] (flat variable namespace).
pub(crate) async fn dispatch_with_registries(
    app: &App,
    req: SpinRequest,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> anyhow::Result<SpinFullResponse> {
    let request_start = app.monotonic_now();
    let kv_registry = build_kv_registry(kv_meta, env).await?;
    let config_registry = build_config_registry(config_meta, env).await?;
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
        request_start,
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
        stores.config_store.map(|handle| {
            ConfigRegistry::single_id(
                "default".to_owned(),
                ConfigStoreBinding {
                    handle,
                    default_key: "default".to_owned(),
                },
            )
        })
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

async fn build_config_registry(
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> anyhow::Result<Option<ConfigRegistry>> {
    let Some(meta) = config_meta else {
        return Ok(None);
    };
    // Spin is `Multi` for config (KV-backed): each declared id opens its
    // own `key_value::Store` under the label resolved from
    // `EDGEZERO__STORES__CONFIG__<ID>__NAME`. Mirrors `build_kv_registry`
    // so missing `key_value_stores = [...]` declarations surface at
    // dispatch setup, not on first config read.
    // Preserve the `ConfigStoreError` as the anyhow source so the
    // caller can downcast to distinguish `Internal` (structural —
    // label not declared / registered) from `Unavailable` (transient
    // hostcall failure). `with_context` chains rather than
    // stringifying.
    let mut by_id: BTreeMap<String, ConfigStoreBinding> = BTreeMap::new();
    for id in meta.ids {
        let label = env.store_name("config", id);
        let store = SpinConfigStore::open(label.clone())
            .await
            .with_context(|| format!("config store id `{id}` (label `{label}`) failed to open"))?;
        by_id.insert(
            (*id).to_owned(),
            ConfigStoreBinding {
                handle: ConfigStoreHandle::new(Arc::new(store)),
                default_key: env.store_key("config", id),
            },
        );
    }
    // Every id is required to open (any failure returns Err above), so
    // `from_parts` is guaranteed to have the default id present.
    Ok(StoreRegistry::from_parts(by_id, meta.default.to_owned()))
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

async fn resolve_config_handle(label: &str) -> anyhow::Result<Option<ConfigStoreHandle>> {
    // Low-level path (`dispatch` / `dispatch_with_kv_label`): open the
    // KV-backed config store under the same label used for KV. Registry
    // callers (`dispatch_with_registries`) use `build_config_registry`
    // instead, which resolves per-id labels via env.
    let store = SpinConfigStore::open(label.to_owned())
        .await
        .with_context(|| format!("low-level dispatch: open config store `{label}`"))?;
    Ok(Some(ConfigStoreHandle::new(Arc::new(store))))
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
    use edgezero_core::router::RouterService;
    use edgezero_core::secret_store::{NoopSecretStore, SecretHandle};
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct StubConfig;
    #[async_trait::async_trait(?Send)]
    #[expect(
        clippy::missing_trait_methods,
        reason = "the test provider intentionally exercises the bounded-read compatibility default"
    )]
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
    fn production_head_conversion_installs_the_exact_application_outbound_clock() {
        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(7))
            .expect("completed instant");
        let observations = Arc::new(Mutex::new(VecDeque::from([start, completed])));
        let clock_observations = Arc::clone(&observations);
        let mut app = App::new(RouterService::builder().build());
        app.set_monotonic_clock(MonotonicClock::new(move || {
            clock_observations
                .lock()
                .expect("clock observations")
                .pop_front()
                .expect("clock observation")
        }));
        let request = request_builder()
            .method(Method::GET)
            .uri("http://example.test/clock")
            .body(Body::empty())
            .expect("request");
        let (parts, _body) = request.into_parts();
        let core_request = into_core_request_head_for_app(parts, &app);
        let client = core_request
            .extensions()
            .get::<HttpClient>()
            .cloned()
            .expect("HTTP client");
        let outbound_request = edgezero_core::OutboundRequest::get("https://example.com/")
            .expect("request")
            .stream_response();

        let results = block_on(client.send_all(vec![outbound_request]));

        assert_eq!(results[0].elapsed, Duration::from_millis(7));
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

    /// Spec 12.7 / plan line 1526: `EDGEZERO__STORES__CONFIG__<ID>__KEY`
    /// must surface as `ConfigStoreBinding.default_key`.
    ///
    /// `build_config_registry` calls `SpinConfigStore::open` which requires
    /// the Spin executor and cannot be unit-tested here; this test exercises
    /// the env-resolution layer that `build_config_registry` reads from.
    /// Platform-integration coverage relies on the E2 smoke scripts.
    #[test]
    fn config_default_key_env_override_resolved() {
        let env = EnvConfig::from_vars([(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY",
            "app_config_staging",
        )]);
        assert_eq!(
            env.store_key("config", "app_config"),
            "app_config_staging",
            "env override must propagate to the key resolved by build_config_registry"
        );
    }
}
