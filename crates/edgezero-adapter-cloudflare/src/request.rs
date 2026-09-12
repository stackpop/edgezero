use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
#[cfg(feature = "test-utils")]
use std::{
    io,
    sync::atomic::{AtomicUsize, Ordering},
    task::Poll,
};

#[cfg(feature = "test-utils")]
use bytes::Bytes;
use edgezero_core::app::{App, StoreMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Method as CoreMethod, Request, Uri, request_builder};
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
#[cfg(feature = "test-utils")]
use futures::executor::block_on;
use futures_util::future::{Either, select};
#[cfg(feature = "test-utils")]
use futures_util::stream::poll_fn;
use futures_util::stream::{LocalBoxStream, once, unfold};
use futures_util::{StreamExt as _, TryStreamExt as _};
use worker::{
    Context, Delay, Env, Error as WorkerError, Method, Request as CfRequest, Response as CfResponse,
};

use crate::config_store::CloudflareConfigStore;
use crate::context::CloudflareRequestContext;
use crate::key_value_store::CloudflareKvStore;
use crate::outbound::CloudflareOutboundClient;
use crate::response::from_egress_response;
use crate::secret_store::CloudflareSecretStore;

/// Groups the optional per-request store handles injected at dispatch time.
///
/// Use `..Default::default()` for fields you do not need:
///
/// ```rust,ignore
/// let stores = Stores { kv: Some(kv_handle), ..Default::default() };
/// ```
#[derive(Default)]
pub(crate) struct Stores {
    config_registry: Option<ConfigRegistry>,
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    secret_registry: Option<SecretRegistry>,
    secrets: Option<SecretHandle>,
}

/// Cloudflare per-request dispatch service.
///
/// Builds a Worker invocation with the stores the operator wants
/// injected into request extensions, then dispatches one request
/// against the wrapped `App`. The store wiring is a per-Service
/// decision; on Cloudflare Workers that means per-request (the
/// runtime invokes the entrypoint per HTTP request), but the
/// Service type itself is cheap to build.
///
/// Replaces the prior `dispatch_with_*` variant fan-out. Each
/// builder method is independent: enable any combination of KV,
/// config, and secret stores by chaining the relevant `with_*` /
/// `require_*` calls. The manifest-driven `run_app` is still the
/// recommended entrypoint for normal flows -- the Service builder
/// is for manual / no-manifest deployments.
///
/// ```rust,ignore
/// CloudflareService::new(&app)
///     .with_kv("sessions").require_kv()
///     .with_config("app_config")
///     .with_secrets()
///     .dispatch(req, env, ctx).await
/// ```
pub struct CloudflareService<'app> {
    app: &'app App,
    config: ConfigSource,
    kv: Option<KvSource>,
    secrets: SecretSource,
}

enum ConfigSource {
    Binding(String),
    Handle(ConfigStoreHandle),
    None,
}

struct KvSource {
    binding: String,
    required: bool,
}

enum SecretSource {
    Off,
    On { required: bool },
}

impl<'app> CloudflareService<'app> {
    /// Resolve every wired store at request time and dispatch
    /// against the wrapped `App`. `env` and `ctx` come from the
    /// Worker runtime per request, NOT the Service builder.
    /// Consumes the service so a builder can't be reused with stale
    /// wiring.
    ///
    /// # Errors
    /// Returns [`worker::Error`] if a required store binding cannot be
    /// opened, the core request cannot be built, or the inner router
    /// dispatch fails.
    #[inline]
    pub async fn dispatch(
        self,
        req: CfRequest,
        env: Env,
        ctx: Context,
    ) -> Result<CfResponse, WorkerError> {
        let request_start = self.app.monotonic_now();
        let config_store = match self.config {
            ConfigSource::Binding(binding) => open_config_or_warn(&env, &binding),
            ConfigSource::Handle(handle) => Some(handle),
            ConfigSource::None => None,
        };
        let kv = match self.kv {
            Some(source) => resolve_kv_handle(&env, &source.binding, source.required)?,
            None => None,
        };
        let secrets = match self.secrets {
            SecretSource::Off => None,
            // Always Some — post-PR-269 fix; `required=false`
            // (set by `.with_secrets()` without
            // `.require_secrets()`) no longer suppresses the
            // handle. See `resolve_secret_handle`.
            SecretSource::On { required } => Some(resolve_secret_handle(&env, required)),
        };
        dispatch_with_handles(
            self.app,
            req,
            env,
            ctx,
            Stores {
                config_store,
                kv,
                secrets,
                ..Default::default()
            },
            request_start,
        )
        .await
    }

    /// Build a new service that dispatches against `app` with NO
    /// stores wired. Chain `.with_*` / `.require_*` to add stores.
    #[must_use]
    #[inline]
    pub fn new(app: &'app App) -> Self {
        Self {
            app,
            config: ConfigSource::None,
            kv: None,
            secrets: SecretSource::Off,
        }
    }

    /// Promote the previously-wired KV binding to required: an
    /// unavailable namespace causes dispatch to return an error.
    /// No-op when `with_kv` wasn't called.
    #[must_use]
    #[inline]
    pub fn require_kv(mut self) -> Self {
        if let Some(kv) = self.kv.as_mut() {
            kv.required = true;
        }
        self
    }

    /// Promote the previously-wired secret store to required.
    /// No-op when `with_secrets` wasn't called.
    #[must_use]
    #[inline]
    pub fn require_secrets(mut self) -> Self {
        if let SecretSource::On { ref mut required } = self.secrets {
            *required = true;
        }
        self
    }

    /// Open the KV namespace bound as `binding` (per `wrangler.toml`)
    /// as a Cloudflare config store and inject its handle. If the
    /// binding is absent the dispatcher logs once and proceeds
    /// without it.
    #[must_use]
    #[inline]
    pub fn with_config<S: Into<String>>(mut self, binding: S) -> Self {
        self.config = ConfigSource::Binding(binding.into());
        self
    }

    /// Inject a pre-built `ConfigStoreHandle`. Use this when the
    /// caller has already opened (or mocked) the backend. Mutually
    /// exclusive with `with_config(binding)` -- the last call wins.
    #[must_use]
    #[inline]
    pub fn with_config_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config = ConfigSource::Handle(handle);
        self
    }

    /// Open the KV namespace bound as `binding` and inject its
    /// handle. Non-required by default: an absent binding logs
    /// once and dispatch continues. Pair with `require_kv()` when
    /// the manifest declares `[stores.kv]`.
    #[must_use]
    #[inline]
    pub fn with_kv<S: Into<String>>(mut self, binding: S) -> Self {
        self.kv = Some(KvSource {
            binding: binding.into(),
            required: false,
        });
        self
    }

    /// Enable Cloudflare Worker secrets and inject the secret-store
    /// handle. Worker secrets have no namespace concept, so no
    /// name is needed. Non-required by default; pair with
    /// `require_secrets()` when the manifest declares
    /// `[stores.secrets]`. Individual missing secrets surface as
    /// `SecretError::NotFound` at access time.
    #[must_use]
    #[inline]
    pub fn with_secrets(mut self) -> Self {
        self.secrets = SecretSource::On { required: false };
        self
    }
}

/// Groups the multi-id store metadata + env config inputs threaded into
/// the registry-based dispatcher. Carved out so `dispatch_with_registries`
/// stays under the `too_many_arguments` ceiling.
pub(crate) struct RegistryInputs<'env> {
    pub config_meta: Option<StoreMetadata>,
    pub env_config: &'env EnvConfig,
    pub kv_meta: Option<StoreMetadata>,
    pub secret_meta: Option<StoreMetadata>,
}

#[cfg(feature = "test-utils")]
struct DropSignal(Arc<AtomicUsize>);

#[cfg(feature = "test-utils")]
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Convert a Cloudflare Worker request into an `EdgeZero` core request.
///
/// # Errors
/// Returns [`EdgeError::bad_request`] if the URL or URI cannot be parsed,
/// and [`EdgeError::internal`] if the body cannot be read or the core
/// request cannot be built.
#[inline]
#[expect(
    clippy::unused_async,
    reason = "the public converter retains its established async API while request bodies remain lazy"
)]
pub async fn into_core_request(
    req: CfRequest,
    env: Env,
    ctx: Context,
) -> Result<Request, EdgeError> {
    let request = into_core_request_head(&req, env, ctx, MonotonicClock::default())?;
    attach_core_body(req, request, None)
}

fn into_core_request_head(
    req: &CfRequest,
    env: Env,
    ctx: Context,
    outbound_clock: MonotonicClock,
) -> Result<Request, EdgeError> {
    let method = into_core_method(&req.method());
    let url = req
        .url()
        .map_err(|err| EdgeError::bad_request(format!("invalid URL: {err}")))?;
    let uri: Uri = url
        .as_str()
        .parse()
        .map_err(|err| EdgeError::bad_request(format!("invalid URI: {err}")))?;

    let mut builder = request_builder().method(method).uri(uri);
    let headers = req.headers();
    for (name, value) in headers.entries() {
        builder = builder.header(name.as_str(), value);
    }

    let mut request = builder.body(Body::empty()).map_err(EdgeError::internal)?;

    CloudflareRequestContext::insert(&mut request, env, ctx);
    request
        .extensions_mut()
        .insert(outbound_client(outbound_clock));
    Ok(request)
}

fn outbound_client(clock: MonotonicClock) -> HttpClient {
    HttpClient::with_client(CloudflareOutboundClient::with_clock(clock))
}

fn attach_core_body(
    req: CfRequest,
    mut request: Request,
    read_lifetime: Option<(Deadline, MonotonicClock)>,
) -> Result<Request, EdgeError> {
    let stream = cloudflare_body_stream(req)?;
    *request.body_mut() = match read_lifetime {
        Some((deadline, monotonic_clock)) => {
            cloudflare_deadline_body(stream, deadline, monotonic_clock)
        }
        None => Body::from_external_stream(stream),
    };
    Ok(request)
}

fn cloudflare_body_stream(
    mut req: CfRequest,
) -> Result<LocalBoxStream<'static, Result<bytes::Bytes, WorkerError>>, EdgeError> {
    if req.inner().body().is_some() {
        return Ok(req
            .stream()
            .map_err(EdgeError::internal)?
            .map_ok(bytes::Bytes::from)
            .boxed_local());
    }

    // The Workers test runtime and some host-created requests expose an
    // ArrayBuffer but no ReadableStream. Keep the fallback read lazy so
    // admission still runs before the first body byte is requested.
    Ok(once(async move { req.bytes().await.map(bytes::Bytes::from) }).boxed_local())
}

fn cloudflare_deadline_body<Source, SourceError>(
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

            Delay::from(Duration::ZERO).await;

            let item = {
                let Some(remaining) = deadline.remaining_at(clock.now()) else {
                    return Some((
                        Err(EdgeError::request_timeout(
                            "inbound body read deadline exceeded",
                        )),
                        None,
                    ));
                };
                let timer = Delay::from(remaining);
                let next = body_stream.next();
                match select(timer, next).await {
                    Either::Left(((), _)) => {
                        return Some((
                            Err(EdgeError::request_timeout(
                                "inbound body read deadline exceeded",
                            )),
                            None,
                        ));
                    }
                    Either::Right((item, _)) => item,
                }
            };
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
    let body = cloudflare_deadline_body(source, Deadline::at_instant(start), clock);
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
    method: CoreMethod,
    uri: Uri,
    source: Source,
) -> Result<CfResponse, WorkerError>
where
    Source: futures_util::Stream<Item = Result<Bytes, SourceError>> + 'static,
    SourceError: Into<anyhow::Error> + 'static,
{
    let request_start = app.monotonic_now();
    let mut core_request = request_builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .map_err(|error| edge_error_to_worker(&EdgeError::internal(error)))?;
    core_request
        .extensions_mut()
        .insert(outbound_client(app.monotonic_clock()));
    dispatch_ingress_stream(
        app,
        core_request,
        Stores::default(),
        request_start,
        move || Ok(source),
    )
    .await
}

pub(crate) async fn dispatch_with_handles(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    stores: Stores,
    request_start: MonotonicInstant,
) -> Result<CfResponse, WorkerError> {
    let head_request = into_core_request_head(&req, env, ctx, app.monotonic_clock())
        .map_err(|error| edge_error_to_worker(&error))?;
    dispatch_ingress_stream(app, head_request, stores, request_start, move || {
        cloudflare_body_stream(req)
    })
    .await
}

async fn dispatch_ingress_stream<Source, SourceError, MakeSource>(
    app: &App,
    mut head_request: Request,
    stores: Stores,
    request_start: MonotonicInstant,
    make_source: MakeSource,
) -> Result<CfResponse, WorkerError>
where
    Source: futures_util::Stream<Item = Result<bytes::Bytes, SourceError>> + 'static,
    SourceError: Into<anyhow::Error> + 'static,
    MakeSource: FnOnce() -> Result<Source, EdgeError>,
{
    let head_parts = IngressHeadParts::from_request(
        &head_request,
        IngressHeadAccounting::HostManaged,
        IngressFraming::HostManaged,
    );
    head_parts
        .validate_normalized(app.ingress_head_limits())
        .map_err(|error| edge_error_to_worker(&error))?;
    let prepared = match app
        .begin_ingress(head_parts, request_start)
        .map_err(|error| edge_error_to_worker(&error))?
    {
        IngressBeginOutcome::Admitted(prepared) => prepared,
        IngressBeginOutcome::Refused(response) => {
            return from_egress_response(response).map_err(|error| edge_error_to_worker(&error));
        }
        _ => {
            return Err(WorkerError::RustError(
                "unsupported ingress admission outcome".to_owned(),
            ));
        }
    };
    let source = make_source().map_err(|error| edge_error_to_worker(&error))?;
    *head_request.body_mut() =
        cloudflare_deadline_body(source, prepared.read_deadline(), prepared.monotonic_clock());
    dispatch_core_request(app, head_request, stores, prepared).await
}

/// Dispatch with per-id store registries built from baked metadata.
///
/// Cloudflare capability map:
/// - KV (Multi): each declared id opens its own KV namespace binding via
///   `EDGEZERO__STORES__KV__<ID>__NAME` (default = id).
/// - Config (Multi): each declared id opens its own KV namespace via
///   `EDGEZERO__STORES__CONFIG__<ID>__NAME`, read asynchronously.
/// - Secrets (Single): one shared [`CloudflareSecretStore`] is registered
///   under every declared id.
pub(crate) async fn dispatch_with_registries(
    app: &App,
    req: CfRequest,
    env: Env,
    ctx: Context,
    inputs: RegistryInputs<'_>,
) -> Result<CfResponse, WorkerError> {
    let request_start = app.monotonic_now();
    let kv_registry = build_kv_registry(&env, inputs.kv_meta, inputs.env_config)?;
    let config_registry = build_config_registry(&env, inputs.config_meta, inputs.env_config);
    let secret_registry = build_secret_registry(&env, inputs.secret_meta, inputs.env_config);
    dispatch_with_handles(
        app,
        req,
        env,
        ctx,
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

pub(crate) fn resolve_kv_handle(
    env: &Env,
    kv_binding: &str,
    kv_required: bool,
) -> Result<Option<KvHandle>, WorkerError> {
    match CloudflareKvStore::from_env(env, kv_binding) {
        Ok(store) => Ok(Some(KvHandle::new(Arc::new(store)))),
        Err(err) => {
            if kv_required {
                return Err(WorkerError::RustError(format!(
                    "KV binding '{kv_binding}' is explicitly configured but could not be opened: {err}"
                )));
            }
            warn_missing_kv_binding_once(kv_binding, &err);
            Ok(None)
        }
    }
}

/// Construct the Cloudflare secret-store handle. Called from the
/// `SecretSource::On { .. }` arm of `dispatch`, so it always builds
/// the handle — the `_required` parameter is preserved (and ignored)
/// for symmetry with the kv path's `resolve_kv_handle(_, _, required)`
/// signature, where `required` decides whether a runtime open
/// failure is fatal or silently degrades. `CloudflareSecretStore` is
/// a thin `env`-wrapper whose construction can't fail, so there's
/// nothing for `required` to gate at handle-construction time —
/// `_required` is reserved for whichever future per-secret-lookup
/// availability policy we add.
///
/// Pre-fix, this short-circuited to `None` when `!_required`, which
/// silently swallowed `.with_secrets()` (which sets `required:
/// false`): handlers ran without a `SecretRegistry` even though the
/// builder claimed to inject one.
pub(crate) fn resolve_secret_handle(env: &Env, _required: bool) -> SecretHandle {
    let secret_store = CloudflareSecretStore::from_env(env.clone());
    SecretHandle::new(Arc::new(secret_store))
}

fn build_config_registry(
    env: &Env,
    config_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    let mut by_id: BTreeMap<String, ConfigStoreBinding> = BTreeMap::new();
    for id in meta.ids {
        let binding_name = env_config.store_name("config", id);
        if let Some(handle) = open_config_or_warn(env, &binding_name) {
            by_id.insert(
                (*id).to_owned(),
                ConfigStoreBinding {
                    handle,
                    default_key: env_config.store_key("config", id),
                },
            );
        }
    }
    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            "config registry default id `{default_id}` could not be opened; dropping the config registry"
        );
    }
    StoreRegistry::from_parts(by_id, default_id)
}

fn build_kv_registry(
    env: &Env,
    kv_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Result<Option<KvRegistry>, WorkerError> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };
    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let binding = env_config.store_name("kv", id);
        // Required per-id: `[stores.kv]` is declared, so failure to open is a
        // runtime error rather than a silent skip.
        let Some(handle) = resolve_kv_handle(env, &binding, true)? else {
            continue;
        };
        by_id.insert((*id).to_owned(), handle);
    }
    let default_id = meta.default.to_owned();
    if !by_id.contains_key(&default_id) {
        log::warn!(
            "KV registry default id `{default_id}` could not be opened; dropping the KV registry"
        );
    }
    Ok(StoreRegistry::from_parts(by_id, default_id))
}

fn build_secret_registry(
    env: &Env,
    secret_meta: Option<StoreMetadata>,
    env_config: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Cloudflare is `Single` for secrets — one shared handle binds every id.
    // `CloudflareSecretStore::get_bytes` ignores `store_name` (worker
    // secrets are a flat namespace), so the per-id bound name is
    // observable only via [`BoundSecretStore::store_name`].
    let handle = SecretHandle::new(Arc::new(CloudflareSecretStore::from_env(env.clone())));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env_config.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    // Cloudflare secret handles are infallible to construct; `from_parts`
    // keeps the API symmetric with the KV / config builders.
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

async fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    stores: Stores,
    prepared: PreparedIngress,
) -> Result<CfResponse, WorkerError> {
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
    let response = app
        .dispatch_admitted(prepared, core_request)
        .await
        .map_err(|err| edge_error_to_worker(&err))?;
    from_egress_response(response).map_err(|err| edge_error_to_worker(&err))
}

fn edge_error_to_worker(err: &EdgeError) -> WorkerError {
    WorkerError::RustError(err.to_string())
}

fn into_core_method(method: &Method) -> CoreMethod {
    let bytes = method.as_ref().as_bytes();
    CoreMethod::from_bytes(bytes).unwrap_or_else(|_| {
        log::warn!(
            "unknown HTTP method {:?}, defaulting to GET",
            method.as_ref()
        );
        CoreMethod::GET
    })
}

fn open_config_or_warn(env: &Env, binding_name: &str) -> Option<ConfigStoreHandle> {
    match CloudflareConfigStore::from_env(env, binding_name) {
        Ok(store) => Some(ConfigStoreHandle::new(Arc::new(store))),
        Err(err) => {
            warn_missing_config_binding_once(binding_name, &err.to_string());
            None
        }
    }
}

/// Pure synthesis: collapse a `Stores` (which may carry both a
/// wired multi-id registry AND a legacy bare handle) into the
/// three registries that go into request extensions. Precedence
/// is "registry wins": a wired registry is taken verbatim; only
/// in its absence is a bare handle wrapped into a one-id registry
/// keyed under `"default"`. The bare handle is never merged in,
/// never used as a fallback for ids the registry doesn't define.
/// Pulled out as a pure function so the precedence contract is
/// unit-testable without spinning up a real `Request` and async
/// dispatcher.
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

fn warn_missing_config_binding_once(binding: &str, error: &impl Display) {
    static WARNED_BINDINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned_bindings = WARNED_BINDINGS.get_or_init(|| Mutex::new(BTreeSet::new()));

    match warned_bindings.lock() {
        Ok(mut guard) => {
            if !guard.insert(binding.to_owned()) {
                return;
            }
            log::warn!("config KV binding '{binding}' not available: {error}");
        }
        Err(_) => {
            log::warn!("config KV binding '{binding}' not available: {error}");
        }
    }
}

fn warn_missing_kv_binding_once(kv_binding: &str, error: &impl Display) {
    static WARNED_BINDINGS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let warned_bindings = WARNED_BINDINGS.get_or_init(|| Mutex::new(BTreeSet::new()));

    match warned_bindings.lock() {
        Ok(mut guard) => {
            if !guard.insert(kv_binding.to_owned()) {
                return;
            }
            log::warn!("KV binding '{kv_binding}' not available: {error}");
        }
        Err(_) => {
            log::warn!("KV binding '{kv_binding}' not available: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use edgezero_core::context::RequestContext;
    use edgezero_core::outbound::OutboundRequest;
    use edgezero_core::router::RouterService;
    use wasm_bindgen_test::wasm_bindgen_test;
    use worker::js_sys::Object;
    use worker::wasm_bindgen::JsCast as _;
    use worker::worker_sys::Context as WorkerSysContext;

    #[wasm_bindgen_test]
    fn into_http_method_defaults_unknown_to_get() {
        let method = Method::from("FOO".to_owned());
        assert_eq!(into_core_method(&method), CoreMethod::GET);
    }

    #[wasm_bindgen_test]
    fn into_http_method_maps_known_methods() {
        assert_eq!(into_core_method(&Method::Get), CoreMethod::GET);
        assert_eq!(into_core_method(&Method::Post), CoreMethod::POST);
        assert_eq!(into_core_method(&Method::Put), CoreMethod::PUT);
        assert_eq!(into_core_method(&Method::Delete), CoreMethod::DELETE);
    }

    #[wasm_bindgen_test]
    async fn standard_service_installs_the_exact_application_outbound_clock() {
        async fn elapsed(ctx: RequestContext) -> Result<String, EdgeError> {
            let client = ctx
                .http_client()
                .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("missing HTTP client")))?;
            let request = OutboundRequest::get("https://example.com/")?.stream_response();
            let results = client.send_all(vec![request]).await;
            Ok(results[0].elapsed.as_millis().to_string())
        }

        let start = MonotonicInstant::now();
        let completed = start
            .checked_add(Duration::from_millis(7))
            .expect("completed instant");
        let observations = Arc::new(AtomicUsize::new(0));
        let clock_observations = Arc::clone(&observations);
        let mut app = App::new(RouterService::builder().get("/clock", elapsed).build());
        app.set_monotonic_clock(MonotonicClock::new(move || {
            if clock_observations.fetch_add(1, Ordering::SeqCst) < 2 {
                start
            } else {
                completed
            }
        }));
        let init = worker::RequestInit::new();
        let request = CfRequest::new_with_init("https://example.com/clock", &init)
            .expect("Cloudflare request");
        let env = Object::new().unchecked_into::<Env>();
        let js_context = Object::new().unchecked_into::<WorkerSysContext>();

        let mut response = CloudflareService::new(&app)
            .dispatch(request, env, Context::new(js_context))
            .await
            .expect("Cloudflare response");

        assert_eq!(response.text().await.expect("response body"), "7");
        assert!(observations.load(Ordering::SeqCst) >= 3);
    }
}

#[cfg(test)]
mod synthesis_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::key_value_store::{KvStore, NoopKvStore};
    use edgezero_core::secret_store::{NoopSecretStore, SecretHandle};

    use super::*;

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

    fn config_handle() -> ConfigStoreHandle {
        ConfigStoreHandle::new(Arc::new(StubConfig))
    }

    fn kv_handle() -> KvHandle {
        let store: Arc<dyn KvStore> = Arc::new(NoopKvStore);
        KvHandle::new(store)
    }

    fn secret_handle() -> SecretHandle {
        SecretHandle::new(Arc::new(NoopSecretStore))
    }

    #[test]
    fn synthesis_handles_config_and_secret_bare_handles_symmetrically() {
        let stores = Stores {
            config_store: Some(config_handle()),
            secrets: Some(secret_handle()),
            ..Default::default()
        };
        let (config, _, secret) = synthesise_store_registries(stores);
        assert_eq!(config.expect("config").default_id(), "default");
        let secret_registry = secret.expect("secret");
        assert_eq!(secret_registry.default_id(), "default");
        // BoundSecretStore binds the synthesised secret to platform
        // store name "default". A handler reading via
        // `ctx.secret_store_default()?.require_str(key)` resolves
        // the cloudflare Worker Secret literally named "default";
        // if the operator's wrangler.toml uses a different name,
        // the runtime require_str() surfaces a clear store-name
        // error rather than a silent miss.
        assert_eq!(
            secret_registry.default().expect("bound").store_name(),
            "default"
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
        assert_eq!(kv_registry.default_id(), "sessions");
        assert!(
            kv_registry.named("default").is_none(),
            "bare handle's `default` synth NOT merged in"
        );
    }

    #[test]
    fn synthesis_returns_none_for_each_kind_with_no_wiring() {
        let (config, kv, secret) = synthesise_store_registries(Stores::default());
        assert!(config.is_none() && kv.is_none() && secret.is_none());
    }

    #[test]
    fn synthesis_wraps_bare_kv_handle_under_default_when_no_registry() {
        let stores = Stores {
            kv: Some(kv_handle()),
            ..Default::default()
        };
        let (config, kv, secret) = synthesise_store_registries(stores);
        assert!(config.is_none());
        assert!(secret.is_none());
        let kv_registry = kv.expect("kv registry synthesised");
        assert_eq!(kv_registry.default_id(), "default");
        assert!(kv_registry.named("other").is_none());
    }

    /// Spec 12.7 / plan line 1526: `EDGEZERO__STORES__CONFIG__<ID>__KEY`
    /// must surface as `ConfigStoreBinding.default_key`.
    ///
    /// `build_config_registry` requires a live `worker::Env` (Cloudflare
    /// runtime type) so cannot be unit-tested here; this test exercises the
    /// env-resolution layer that `build_config_registry` reads from.
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
