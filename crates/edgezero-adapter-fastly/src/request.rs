use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::io::Read as _;
use std::str::from_utf8;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use edgezero_core::app::{App, StoreMetadata, StoresMetadata};
use edgezero_core::body::Body;
use edgezero_core::config_store::ConfigStoreHandle;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::error::EdgeError;
use edgezero_core::http::header::HOST;
use edgezero_core::http::{Extensions, HeaderValue, Request, Uri, request_builder};
use edgezero_core::key_value_store::KvHandle;
use edgezero_core::proxy::ProxyHandle;
use edgezero_core::request::{
    CapturedTarget, HeaderFidelity, InboundOrigin, OriginSource, Preservation, RequestIngress,
    TargetUnavailable,
};
use edgezero_core::secret_store::SecretHandle;
use edgezero_core::store_registry::{
    BoundSecretStore, ConfigRegistry, ConfigStoreBinding, KvRegistry, SecretRegistry, StoreRegistry,
};
use fastly::{Error as FastlyError, Request as FastlyRequest, Response as FastlyResponse};
use futures::executor;
use std::collections::BTreeMap;

use crate::config_store::FastlyConfigStore;
use crate::context::FastlyRequestContext;
use crate::key_value_store::FastlyKvStore;
use crate::proxy::FastlyProxyClient;
use crate::response::{from_core_response, parse_uri};
use crate::secret_store::FastlySecretStore;

const WARNED_STORE_CACHE_LIMIT: usize = 64;

#[derive(Default)]
struct RecentStringSet {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentStringSet {
    fn insert(&mut self, key: &str, limit: usize) -> bool {
        let owned = key.to_owned();
        if !self.keys.insert(owned.clone()) {
            return false;
        }
        self.order.push_back(owned);
        while limit > 0 && self.order.len() > limit {
            if let Some(oldest) = self.order.pop_front() {
                self.keys.remove(&oldest);
            }
        }
        true
    }
}

/// Groups the optional per-request store handles injected at dispatch time.
///
/// Use `..Default::default()` for fields you do not need:
///
/// ```rust,ignore
/// let stores = Stores { kv: Some(kv_handle), ..Default::default() };
/// ```
#[derive(Default)]
struct Stores {
    config_registry: Option<ConfigRegistry>,
    config_store: Option<ConfigStoreHandle>,
    kv: Option<KvHandle>,
    kv_registry: Option<KvRegistry>,
    secret_registry: Option<SecretRegistry>,
    secrets: Option<SecretHandle>,
}

enum ConfigSource {
    Handle(ConfigStoreHandle),
    Name(String),
    None,
}

/// Fastly per-request dispatch service.
///
/// Builds a router invocation with the stores the operator wants
/// injected into request extensions, then dispatches one request
/// against the wrapped `App`. The store wiring is a per-Service
/// decision; on Fastly Compute that means per-request (the worker
/// model invokes the entrypoint per HTTP request), but the Service
/// type itself is cheap to build.
///
/// Replaces the prior `dispatch_with_*` variant fan-out. Each
/// builder method is independent: enable any combination of KV,
/// config, and secret stores by chaining the relevant `with_*` /
/// `require_*` calls. The manifest-driven `run_app` is still the
/// recommended entrypoint for normal flows -- the Service builder
/// is for manual / no-manifest deployments.
///
/// ```rust,ignore
/// FastlyService::new(&app)
///     .with_kv("sessions").require_kv()
///     .with_config("app_config")
///     .with_secrets()
///     .dispatch(req)
/// ```
pub struct FastlyService<'app> {
    app: &'app App,
    config: ConfigSource,
    kv: Option<KvSource>,
    request_ingress: Option<RequestIngress>,
    secrets: SecretSource,
}

struct KvSource {
    name: String,
    required: bool,
}

enum SecretSource {
    Off,
    On { required: bool },
}

impl<'app> FastlyService<'app> {
    /// Resolve every wired store at request time and dispatch
    /// against the wrapped `App`. Consumes the service so a builder
    /// can't be reused with stale wiring.
    ///
    /// # Errors
    /// Returns an error if a required store cannot be opened or
    /// the underlying handler returns an error.
    #[inline]
    pub fn dispatch(self, req: FastlyRequest) -> Result<FastlyResponse, FastlyError> {
        let ingress = self
            .request_ingress
            .unwrap_or_else(|| capture_request_ingress(&req));
        let config_store = match self.config {
            ConfigSource::Handle(handle) => Some(handle),
            ConfigSource::Name(name) => match FastlyConfigStore::try_open(&name) {
                Ok(store) => Some(ConfigStoreHandle::new(Arc::new(store))),
                Err(err) => {
                    warn_missing_store_once(&name, &err.to_string());
                    None
                }
            },
            ConfigSource::None => None,
        };
        let kv = match self.kv {
            Some(source) => resolve_kv_handle(&source.name, source.required)?,
            None => None,
        };
        let secrets = match self.secrets {
            SecretSource::Off => None,
            SecretSource::On { required } => Some(resolve_secret_handle(required)),
        };
        dispatch_with_handles(
            self.app,
            req,
            Stores {
                config_store,
                kv,
                secrets,
                ..Default::default()
            },
            ingress,
            |_req, _extensions| {},
        )
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
            request_ingress: None,
            secrets: SecretSource::Off,
        }
    }

    /// Promote the previously-wired KV store to required: an
    /// unavailable store causes dispatch to return an error
    /// instead of silently degrading. No-op when `with_kv` wasn't
    /// called.
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

    /// Open the Fastly Config Store named `name` and inject its
    /// handle into request extensions. If the store is unavailable
    /// at request time, the dispatcher logs the warning once and
    /// proceeds without it.
    ///
    /// Env-overlay limitation: this bare-handle path does not resolve
    /// `EDGEZERO__STORES__CONFIG__*` selectors and binds the config registry's
    /// default key to `"default"`. Use [`runtime_env_config`](crate::runtime_env_config)
    /// with [`dispatch_with_registries`] when a custom entry point needs the
    /// same `__NAME` / `__KEY` resolution as [`run_app`](crate::run_app).
    #[must_use]
    #[inline]
    pub fn with_config<S: Into<String>>(mut self, name: S) -> Self {
        self.config = ConfigSource::Name(name.into());
        self
    }

    /// Inject a pre-built `ConfigStoreHandle`. Use this when the
    /// caller has already opened (or mocked) the backend. Mutually
    /// exclusive with `with_config(name)` -- the last call wins.
    /// Like [`Self::with_config`], this binds the config registry's default key
    /// to `"default"` and does not apply the [`EnvConfig`] overlay. Use
    /// [`runtime_env_config`](crate::runtime_env_config) with
    /// [`dispatch_with_registries`] for manifest-driven selector resolution.
    #[must_use]
    #[inline]
    pub fn with_config_handle(mut self, handle: ConfigStoreHandle) -> Self {
        self.config = ConfigSource::Handle(handle);
        self
    }

    /// Open a Fastly KV Store by `name` and inject its handle.
    /// Non-required by default: an absent store logs once and
    /// dispatch continues. Pair with `require_kv()` when the
    /// manifest declares `[stores.kv]` and a missing store should
    /// fail loudly.
    #[must_use]
    #[inline]
    pub fn with_kv<S: Into<String>>(mut self, name: S) -> Self {
        self.kv = Some(KvSource {
            name: name.into(),
            required: false,
        });
        self
    }

    /// Retain metadata captured before native request mutations or shortcuts.
    ///
    /// The caller must establish the supplied snapshot's provenance and fidelity.
    #[must_use]
    #[inline]
    pub fn with_request_ingress(mut self, ingress: RequestIngress) -> Self {
        self.request_ingress = Some(ingress);
        self
    }

    /// Enable the Fastly Secret Store and inject its handle.
    /// Non-required by default: an absent store leaves no secret
    /// handle in extensions and dispatch continues. Pair with
    /// `require_secrets()` when the manifest declares
    /// `[stores.secrets]`.
    ///
    /// Platform-name binding: the synthesised `SecretRegistry`
    /// binds the handle to platform store name `"default"`.
    /// Handlers reading `ctx.secret_store_default()?.require_str(key)`
    /// open a Fastly Secret Store literally named `"default"`. Use
    /// the manifest-aware `run_app` if your account uses a
    /// different store name -- it routes through the env-overlay
    /// resolution path instead.
    #[must_use]
    #[inline]
    pub fn with_secrets(mut self) -> Self {
        self.secrets = SecretSource::On { required: false };
        self
    }
}

fn dispatch_core_request(
    app: &App,
    mut core_request: Request,
    stores: Stores,
) -> Result<FastlyResponse, FastlyError> {
    insert_store_registries(&mut core_request, stores);
    let response = executor::block_on(app.router().oneshot(core_request))
        .map_err(|err| map_edge_error(&err))?;
    from_core_response(response).map_err(|err| map_edge_error(&err))
}

fn insert_store_registries(core_request: &mut Request, stores: Stores) {
    // Hard-cutoff: legacy bare handles are no longer
    // inserted into request extensions. `with_config_handle`
    // still accepts a `ConfigStoreHandle`, but the dispatcher
    // synthesises a one-id `<kind>Registry` from any wired handle
    // and only the registry goes into extensions. The
    // `ctx.{config,kv,secret}_handle()` accessors are gone; handlers
    // use `ctx.{config,kv,secret}_store_default()` or the
    // `Kv` / `Config` / `Secrets` extractors.
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
}

/// Run an app-provided closure against a scratch `Extensions` populated from the
/// RAW `fastly::Request` (JA4 / H2 / etc.), BEFORE `into_core_request` consumes
/// the request. Returns the scratch bag to be `extend`ed into the core request.
fn apply_request_extend<F>(req: &FastlyRequest, extend: F) -> Extensions
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let mut scratch = Extensions::default();
    extend(req, &mut scratch);
    scratch
}

fn dispatch_with_handles<F>(
    app: &App,
    req: FastlyRequest,
    stores: Stores,
    ingress: RequestIngress,
    extend: F,
) -> Result<FastlyResponse, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    // Read raw-request signals into a scratch bag BEFORE conversion consumes `req`.
    let scratch = apply_request_extend(&req, extend);
    let mut core_request = convert_request(req).map_err(|err| map_edge_error(&err))?;
    core_request.extensions_mut().extend(scratch);
    core_request.extensions_mut().insert(ingress);
    dispatch_core_request(app, core_request, stores)
}

/// Dispatch with per-id store registries built from baked metadata — the same
/// store wiring [`run_app`](crate::run_app) uses.
///
/// Fastly is `Multi` for all three kinds, so each declared id resolves to
/// its own platform store through the [`EnvConfig`] overlay: the
/// `EDGEZERO__STORES__CONFIG__<ID>__NAME` selector (and its KV / secrets
/// counterparts) picks the platform store, and the config-only `__KEY`
/// selector picks that store's [`ConfigStoreBinding::default_key`]. Pair this
/// with [`runtime_env_config`](crate::runtime_env_config) in a custom entry
/// point for full parity with `run_app`. Contrast [`FastlyService`], whose
/// bare-handle path binds `default_key: "default"` and ignores those selectors.
///
/// KV failures escalate via `resolve_kv_handle`'s `kv_required=true` path;
/// missing config / secret stores degrade silently with a one-time warning.
///
/// # Errors
/// Returns an error if a declared KV store cannot be opened, or if the
/// underlying handler returns an error.
#[inline]
pub fn dispatch_with_registries<F>(
    app: &App,
    req: FastlyRequest,
    stores: StoresMetadata,
    env: &EnvConfig,
    extend: F,
) -> Result<FastlyResponse, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let ingress = capture_request_ingress(&req);
    dispatch_with_registries_and_ingress(app, req, stores, env, ingress, extend)
}

/// Dispatch with manifest store selectors and an already captured ingress snapshot.
///
/// The snapshot is inserted after the extension callback, preventing scratch
/// metadata from replacing adapter-captured facts. Store semantics match
/// [`dispatch_with_registries`].
///
/// # Errors
/// Returns an error if a declared KV store cannot be opened or dispatch fails.
#[inline]
pub fn dispatch_with_registries_and_ingress<F>(
    app: &App,
    req: FastlyRequest,
    stores: StoresMetadata,
    env: &EnvConfig,
    ingress: RequestIngress,
    extend: F,
) -> Result<FastlyResponse, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let core_request =
        into_core_request_with_registries_and_ingress(req, stores, env, ingress, extend)?;
    let response = executor::block_on(app.router().oneshot(core_request))
        .map_err(|err| map_edge_error(&err))?;
    from_core_response(response).map_err(|err| map_edge_error(&err))
}

/// Convert a Fastly request with the per-id store registries used by
/// [`dispatch_with_registries`], without dispatching or converting a response.
///
/// The extension callback reads the native request before conversion. Registries
/// are inserted afterward, with the same precedence and binding policy as standard
/// dispatch. Drive `app.router().oneshot(request)` with `futures::executor::block_on`
/// when the caller needs response extensions or progressive streaming.
///
/// # Errors
/// Returns an error if a declared KV store cannot be opened or the request
/// cannot be converted, including an inbound body read failure.
#[inline]
pub fn into_core_request_with_registries<F>(
    req: FastlyRequest,
    stores: StoresMetadata,
    env: &EnvConfig,
    extend: F,
) -> Result<Request, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let ingress = capture_request_ingress(&req);
    into_core_request_with_registries_and_ingress(req, stores, env, ingress, extend)
}

/// Convert with store registries and an already captured ingress snapshot.
///
/// The snapshot is inserted after the extension callback, preventing scratch
/// metadata from replacing adapter-captured facts. Store semantics match
/// [`into_core_request_with_registries`].
///
/// # Errors
/// Returns an error if a declared KV store cannot be opened or the request
/// cannot be converted, including an inbound body read failure.
#[inline]
pub fn into_core_request_with_registries_and_ingress<F>(
    req: FastlyRequest,
    stores: StoresMetadata,
    env: &EnvConfig,
    ingress: RequestIngress,
    extend: F,
) -> Result<Request, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let kv_registry = build_kv_registry(stores.kv, env)?;
    let config_registry = build_config_registry(stores.config, env);
    let secret_registry = build_secret_registry(stores.secrets, env);
    let scratch = apply_request_extend(&req, extend);
    let mut core_request = convert_request(req).map_err(|err| map_edge_error(&err))?;
    core_request.extensions_mut().extend(scratch);
    core_request.extensions_mut().insert(ingress);
    insert_store_registries(
        &mut core_request,
        Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Default::default()
        },
    );
    Ok(core_request)
}

/// Pure synthesis: collapse a `Stores` (which may carry both a
/// wired multi-id registry AND a legacy bare handle) into the
/// three registries that go into request extensions. Precedence
/// is "registry wins": a wired registry is taken verbatim; only
/// in its absence is a bare handle wrapped into a one-id registry
/// keyed under `"default"`. The bare handle is never merged
/// in, never used as a fallback for ids the registry doesn't
/// define. Pulled out as a pure function so the precedence
/// contract is unit-testable without spinning up a real
/// `Request` and async dispatcher.
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

fn build_kv_registry(
    kv_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Result<Option<KvRegistry>, FastlyError> {
    let Some(meta) = kv_meta else {
        return Ok(None);
    };
    let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("kv", id);
        // KV is required: if `[stores.kv]` is declared, an id failing to open
        // is a runtime error rather than a silent degradation.
        let Some(handle) = resolve_kv_handle(&store_name, true)? else {
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

fn build_config_registry(
    config_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<ConfigRegistry> {
    let meta = config_meta?;
    let mut by_id: BTreeMap<String, ConfigStoreBinding> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("config", id);
        match FastlyConfigStore::try_open(&store_name) {
            Ok(store) => {
                by_id.insert(
                    (*id).to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(store)),
                        default_key: env.store_key("config", id),
                    },
                );
            }
            Err(err) => warn_missing_store_once(&store_name, &err.to_string()),
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

fn build_secret_registry(
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
) -> Option<SecretRegistry> {
    let meta = secret_meta?;
    // Fastly is `Multi` for secrets. The provider trait is stateless —
    // `FastlySecretStore::get_bytes(store_name, key)` opens the named Fastly
    // Secret Store per call — so we share one provider handle across all
    // bindings, then capture the per-id platform store name in the bound
    // wrapper. `EDGEZERO__STORES__SECRETS__<ID>__NAME` (default = the logical
    // id) decides which Fastly store each id resolves to at runtime.
    let handle = SecretHandle::new(Arc::new(FastlySecretStore));
    let mut by_id: BTreeMap<String, BoundSecretStore> = BTreeMap::new();
    for id in meta.ids {
        let store_name = env.store_name("secrets", id);
        by_id.insert(
            (*id).to_owned(),
            BoundSecretStore::new(handle.clone(), store_name),
        );
    }
    // Fastly's secret-store handle wrappers are infallible to construct;
    // `from_parts` keeps the API symmetric with the KV / config builders.
    StoreRegistry::from_parts(by_id, meta.default.to_owned())
}

/// Capture available ingress facts without consuming the native request.
///
/// Fastly SDK 0.12.1 does not expose a safe borrowed raw target accessor.
/// Record target unavailability before reading the SDK URL for the canonical
/// origin. That URL may normalize the path and is never used as an original
/// target. Client requests require exactly one validated Host matching the
/// runtime URL authority. No forwarded headers or missing TLS facts supply a
/// scheme. This function does not clone handles or change client provenance.
///
/// # Panics
/// The SDK URL accessor can panic if its runtime URI cannot be parsed.
/// The static empty override list is asserted to be valid; this invariant
/// cannot fail under the current metadata constructor.
#[expect(
    clippy::expect_used,
    reason = "an empty override list cannot contain duplicate names"
)]
#[inline]
#[must_use]
pub fn capture_request_ingress(req: &FastlyRequest) -> RequestIngress {
    let target = CapturedTarget::Unavailable(TargetUnavailable::NotExposed);
    let origin = if req.is_from_client() {
        parse_uri(req.get_url_str()).ok().and_then(|uri| {
            origin_from_runtime_uri(&uri, req.get_header_all(HOST).map(HeaderValue::as_bytes))
        })
    } else {
        None
    };
    RequestIngress::new(
        target,
        origin,
        HeaderFidelity::new(
            Preservation::Unknown,
            Preservation::Unknown,
            Preservation::Unknown,
            Preservation::Unavailable,
        ),
        Vec::new(),
    )
    .expect("should use unique header overrides")
}

fn origin_from_runtime_uri<'header>(
    uri: &Uri,
    mut hosts: impl Iterator<Item = &'header [u8]>,
) -> Option<InboundOrigin> {
    let host = from_utf8(hosts.next()?).ok()?;
    if hosts.next().is_some() {
        return None;
    }
    let scheme = uri.scheme_str()?;
    let _runtime_origin =
        InboundOrigin::parse(scheme, uri.authority()?.as_str(), OriginSource::RuntimeUri).ok()?;
    let origin = InboundOrigin::parse(scheme, host, OriginSource::RuntimeUri).ok()?;
    let host_uri = Uri::builder()
        .scheme(origin.scheme())
        .authority(origin.authority())
        .path_and_query("/")
        .build()
        .ok()?;
    let default_port = if origin.scheme() == "https" { 443 } else { 80 };
    if !uri.host()?.eq_ignore_ascii_case(host_uri.host()?)
        || uri.port_u16().unwrap_or(default_port) != host_uri.port_u16().unwrap_or(default_port)
    {
        return None;
    }
    Some(origin)
}

/// Convert a native request, capturing ingress before native mutation.
///
/// # Errors
/// Returns an error if method, URI or body conversion fails.
#[inline]
pub fn into_core_request(req: FastlyRequest) -> Result<Request, EdgeError> {
    let ingress = capture_request_ingress(&req);
    into_core_request_with_ingress(req, ingress)
}

/// Convert a native request while retaining the supplied bounded snapshot.
///
/// The caller must establish the supplied snapshot's provenance and fidelity.
/// Conversion retains its facts even if the native request has since changed.
///
/// # Errors
/// Returns an error if method, URI or body conversion fails.
#[inline]
pub fn into_core_request_with_ingress(
    req: FastlyRequest,
    ingress: RequestIngress,
) -> Result<Request, EdgeError> {
    let mut request = convert_request(req)?;
    request.extensions_mut().insert(ingress);
    Ok(request)
}

fn convert_request(mut req: FastlyRequest) -> Result<Request, EdgeError> {
    let method = req.get_method().clone();
    let uri = parse_uri(req.get_url_str())?;

    let mut builder = request_builder().method(method).uri(uri);
    for (name, value) in req.get_headers() {
        builder = builder.header(name.as_str(), value.as_bytes());
    }

    let mut body = req.take_body();
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).map_err(EdgeError::internal)?;

    let mut request = builder
        .body(Body::from(bytes))
        .map_err(EdgeError::internal)?;

    let context = FastlyRequestContext {
        client_ip: req.get_client_ip_addr(),
    };
    FastlyRequestContext::insert(&mut request, context);
    request
        .extensions_mut()
        .insert(ProxyHandle::with_client(FastlyProxyClient));

    Ok(request)
}

fn map_edge_error(err: &EdgeError) -> FastlyError {
    FastlyError::msg(err.to_string())
}

fn resolve_kv_handle(
    kv_store_name: &str,
    kv_required: bool,
) -> Result<Option<KvHandle>, FastlyError> {
    match FastlyKvStore::open(kv_store_name) {
        Ok(store) => Ok(Some(KvHandle::new(Arc::new(store)))),
        Err(err) => {
            if kv_required {
                return Err(FastlyError::msg(format!(
                    "KV store '{kv_store_name}' is explicitly configured but could not be opened: {err}"
                )));
            }
            warn_missing_kv_store_once(kv_store_name, &err);
            Ok(None)
        }
    }
}

/// Construct the Fastly secret-store handle. Called from the
/// `SecretSource::On { .. }` arm of `dispatch`. The `_required`
/// parameter is preserved (and ignored) for symmetry with the kv
/// path's `resolve_kv_handle(_, _required)`, where `required`
/// decides whether a runtime open failure is fatal or silently
/// degrades. `FastlySecretStore` is a unit struct whose
/// construction can't fail, so there's nothing for `required` to
/// gate here — and `clippy::unnecessary_wraps` would flag an
/// `Option<SecretHandle>` return on a function that never returns
/// None. The caller wraps the result in `Some(...)` so the
/// `SecretSource::Off => None` branch still produces the right
/// `Option`.
///
/// Pre-fix, the return type was `Option<SecretHandle>` and the
/// body short-circuited to `None` when `!_required`, which
/// silently swallowed `.with_secrets()` (which sets `required:
/// false`): handlers ran without a `SecretRegistry` even though the
/// builder claimed to inject one.
fn resolve_secret_handle(_required: bool) -> SecretHandle {
    SecretHandle::new(Arc::new(FastlySecretStore))
}

fn warn_missing_kv_store_once(kv_store_name: &str, error: &impl Display) {
    static WARNED_KV_STORES: OnceLock<Mutex<RecentStringSet>> = OnceLock::new();
    warn_missing_once(&WARNED_KV_STORES, "KV store", kv_store_name, error);
}

fn warn_missing_once(
    cache: &'static OnceLock<Mutex<RecentStringSet>>,
    item_type: &str,
    name: &str,
    detail: &impl Display,
) {
    let set = cache.get_or_init(|| Mutex::new(RecentStringSet::default()));
    let mut guard = set.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.insert(name, WARNED_STORE_CACHE_LIMIT) {
        log::warn!("{item_type} '{name}' not available: {detail}");
    }
}

fn warn_missing_store_once(store_name: &str, detail: &str) {
    static WARNED_STORES: OnceLock<Mutex<RecentStringSet>> = OnceLock::new();
    warn_missing_once(
        &WARNED_STORES,
        "configured Fastly config store",
        store_name,
        &format!("{detail}; skipping config-store injection"),
    );
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
    fn apply_request_extend_populates_scratch_from_raw_request() {
        use edgezero_core::http::Method;

        #[derive(Clone, Debug, PartialEq)]
        struct Ja4(String);

        let raw = FastlyRequest::new(Method::GET, "http://example.test/");
        let scratch = apply_request_extend(&raw, |req, extensions| {
            // A real closure would call req.get_tls_ja4(); deriving from the URL
            // keeps the assertion deterministic under Viceroy.
            let marker = req.get_url_str().to_owned();
            extensions.insert(Ja4(marker));
        });

        assert_eq!(
            scratch.get::<Ja4>(),
            Some(&Ja4("http://example.test/".to_owned()))
        );
    }

    #[test]
    fn extended_request_extensions_are_visible_to_handler() {
        use edgezero_core::body::Body;
        use edgezero_core::context::RequestContext;
        use edgezero_core::http::{Method, StatusCode, request_builder};
        use edgezero_core::router::RouterService;
        use futures::executor::block_on;

        #[derive(Clone)]
        struct Ja4(String);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let ja4 = ctx
                .request()
                .extensions()
                .get::<Ja4>()
                .map_or_else(|| "missing".to_owned(), |value| value.0.clone());
            Ok(ja4)
        }

        // Mirror what `dispatch_with_handles` does: a scratch bag built from the
        // raw request is `extend`ed into the core request before dispatch.
        let mut scratch = Extensions::default();
        scratch.insert(Ja4("t13d1516h2".to_owned()));

        let mut core_request = request_builder()
            .method(Method::GET)
            .uri("/ja4")
            .body(Body::empty())
            .expect("request");
        core_request.extensions_mut().extend(scratch);

        let service = RouterService::builder().get("/ja4", handler).build();
        let response = block_on(service.oneshot(core_request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"t13d1516h2");
    }

    #[test]
    fn synthesis_wraps_bare_kv_handle_under_default_when_no_registry() {
        let stores = Stores {
            kv: Some(kv_handle()),
            ..Default::default()
        };
        let (config_out, kv_out, secret_out) = synthesise_store_registries(stores);
        assert!(
            config_out.is_none(),
            "no config wiring -> no config registry"
        );
        assert!(
            secret_out.is_none(),
            "no secret wiring -> no secret registry"
        );
        let kv_reg = kv_out.expect("kv registry synthesised from bare handle");
        assert_eq!(
            kv_reg.default_id(),
            "default",
            "synthesised id is `default`"
        );
        assert!(kv_reg.named("default").is_some());
        assert!(
            kv_reg.named("other").is_none(),
            "synthesised registry only knows the `default` id"
        );
    }

    #[test]
    fn synthesis_registry_wins_over_bare_handle_when_both_wired() {
        // Multi-id registry declaring only `sessions` paired with a
        // bare handle that would otherwise synthesise to a
        // `default`-keyed entry. Precedence rule: the bare handle
        // is dropped entirely; the registry stands alone with no
        // `default` id.
        let mut by_id: BTreeMap<String, KvHandle> = BTreeMap::new();
        by_id.insert("sessions".to_owned(), kv_handle());
        let registry = KvRegistry::new(by_id, "sessions".to_owned());
        let stores = Stores {
            kv: Some(kv_handle()),
            kv_registry: Some(registry),
            ..Default::default()
        };
        let (_, kv_out, _) = synthesise_store_registries(stores);
        let kv_reg = kv_out.expect("registry survives synthesis");
        assert_eq!(kv_reg.default_id(), "sessions");
        assert!(
            kv_reg.named("default").is_none(),
            "bare handle's `default` synth NOT merged in"
        );
    }

    #[test]
    fn synthesis_returns_none_for_each_kind_with_no_wiring() {
        let (config, kv, secret) = synthesise_store_registries(Stores::default());
        assert!(config.is_none() && kv.is_none() && secret.is_none());
    }

    #[test]
    fn synthesis_handles_config_and_secret_bare_handles_symmetrically() {
        let stores = Stores {
            config_store: Some(config_handle()),
            secrets: Some(secret_handle()),
            ..Default::default()
        };
        let (config_out, _, secret_out) = synthesise_store_registries(stores);
        let config_reg = config_out.expect("config wrapped");
        assert_eq!(config_reg.default_id(), "default");
        let secret_reg = secret_out.expect("secret wrapped");
        assert_eq!(secret_reg.default_id(), "default");
        // BoundSecretStore binds the synthesised secret to platform
        // store name "default" -- if the underlying Fastly account
        // has no Secret Store literally named "default", the
        // require_str() call from a handler will fail with a clear
        // store-name error rather than silent miss.
        assert_eq!(
            secret_reg.default().expect("default bound").store_name(),
            "default"
        );
    }

    // Regression for the `.with_secrets()` bug — the pre-fix
    // `resolve_secret_handle(false)` short-circuited to `None`, so the
    // documented `.with_secrets().dispatch(...)` path silently ran
    // handlers without a `SecretRegistry`. After the fix, the handle
    // is always built when `SecretSource::On` is selected; `_required`
    // is reserved for whichever future per-secret-lookup availability
    // policy lands.

    #[test]
    fn resolve_secret_handle_builds_handle_when_required_false_matches_with_secrets_default() {
        // The return type is unconditionally `SecretHandle` (post-
        // clippy::unnecessary_wraps cleanup); just exercise the
        // call to lock in that `.with_secrets()`-shaped paths
        // (required=false) still build a handle without panicking.
        let _handle = resolve_secret_handle(false);
    }

    #[test]
    fn resolve_secret_handle_builds_handle_when_required_true_matches_require_secrets() {
        let _handle = resolve_secret_handle(true);
    }

    /// Spec 12.7 / plan line 1526: `EDGEZERO__STORES__CONFIG__<ID>__KEY`
    /// must surface as `ConfigStoreBinding.default_key`.
    ///
    /// `build_config_registry` calls `FastlyConfigStore::try_open` which
    /// requires live Fastly hostcalls and cannot be unit-tested here; this
    /// test exercises the env-resolution layer that `build_config_registry`
    /// reads from. Platform-integration coverage relies on the E2 smoke
    /// scripts.
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

#[cfg(test)]
mod ingress_tests {
    use super::{
        FastlyService, capture_request_ingress, dispatch_with_registries_and_ingress,
        into_core_request_with_ingress, origin_from_runtime_uri,
    };
    use crate::context::FastlyRequestContext;
    use async_trait::async_trait;
    use edgezero_core::app::{App, StoresMetadata};
    use edgezero_core::body::Body;
    use edgezero_core::env_config::EnvConfig;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{HeaderValue, Request, Response, Uri, response_builder};
    use edgezero_core::proxy::ProxyHandle;
    use edgezero_core::request::{
        CapturedTarget, HeaderFidelity, OriginSource, Preservation, RequestIngress, TargetSource,
        TargetUnavailable,
    };
    use edgezero_core::router::{PreDispatchHook, RouterService};
    use fastly::Request as FastlyRequest;
    use std::sync::Arc;

    #[derive(Clone)]
    struct ScratchMarker;

    struct VerifySnapshot {
        scratch_expected: bool,
    }

    #[async_trait(?Send)]
    impl PreDispatchHook for VerifySnapshot {
        async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
            assert_eq!(
                request.extensions().get::<ScratchMarker>().is_some(),
                self.scratch_expected
            );
            let ingress = request
                .extensions()
                .get::<RequestIngress>()
                .expect("should expose chosen snapshot");
            let CapturedTarget::Complete(target) = ingress.target() else {
                panic!("should retain captured target")
            };
            assert_eq!(target.value(), "/reserved/%2e");
            Ok(Some(
                response_builder()
                    .status(204)
                    .body(Body::empty())
                    .map_err(EdgeError::internal)?,
            ))
        }
    }

    #[test]
    fn ingress_runtime_origin_accepts_one_matching_authority() {
        for (url, host, expected_scheme) in [
            ("http://example.com/reserved", "EXAMPLE.com:80", "http"),
            ("https://example.com/reserved", "example.com:443", "https"),
            (
                "https://example.com:8443/reserved",
                "example.com:8443",
                "https",
            ),
            ("https://[::1]/reserved", "[::1]:443", "https"),
        ] {
            let uri: Uri = url.parse().expect("should parse runtime URI");
            let origin = origin_from_runtime_uri(&uri, [host.as_bytes()].into_iter())
                .expect("should recover the matching runtime origin");
            assert_eq!(
                origin.scheme(),
                expected_scheme,
                "should use runtime scheme"
            );
            assert_eq!(
                origin.authority(),
                host,
                "should retain validated authority"
            );
            assert_eq!(
                origin.source(),
                OriginSource::RuntimeUri,
                "should identify runtime provenance"
            );
        }
    }

    #[test]
    fn ingress_runtime_origin_rejects_missing_duplicate_and_conflicting_hosts() {
        let uri: Uri = "https://example.com/reserved"
            .parse()
            .expect("should parse URI");
        for hosts in [
            vec![],
            vec![b"example.com".as_slice(), b"example.com".as_slice()],
            vec![b"other.example.com".as_slice()],
            vec![b"example.com:80".as_slice()],
            vec![b"example.com, other.example.com".as_slice()],
            vec![b"example.com:invalid".as_slice()],
            vec![b"a=\xff".as_slice()],
        ] {
            assert!(
                origin_from_runtime_uri(&uri, hosts.into_iter()).is_none(),
                "should not invent an origin from unavailable or inconsistent authority"
            );
        }
    }

    #[test]
    fn ingress_runtime_origin_rejects_invalid_runtime_authorities() {
        for url in [
            "https://user@example.com/reserved",
            "https://example.com:invalid/reserved",
            "https://example.com:65536/reserved",
        ] {
            let uri: Uri = url
                .parse()
                .expect("should represent invalid runtime authority");
            assert!(
                origin_from_runtime_uri(&uri, [b"example.com".as_slice()].into_iter()).is_none(),
                "should validate the complete runtime authority before comparing host or default port"
            );
        }
    }

    fn snapshot() -> RequestIngress {
        RequestIngress::new(
            CapturedTarget::capture(
                "/reserved/%2e",
                TargetSource::TransportRequestTarget,
                Preservation::Unknown,
            ),
            None,
            HeaderFidelity::default(),
            Vec::new(),
        )
        .expect("should build snapshot")
    }

    #[test]
    fn ingress_current_sdk_reports_missing_target_and_no_synthetic_origin() {
        let native = FastlyRequest::get("https://example.com/reserved");
        let ingress = capture_request_ingress(&native);
        assert!(matches!(
            ingress.target(),
            CapturedTarget::Unavailable(TargetUnavailable::NotExposed)
        ));
        assert!(ingress.origin().is_none());
    }

    #[test]
    fn ingress_snapshot_and_header_bytes_survive_conversion() {
        let mut native = FastlyRequest::new("EXAMPLE-METHOD", "https://example.com/later");
        native.append_header("cookie", b"a=\xff".as_slice());
        native.append_header("cookie", "b=2");
        let client_ip = native.get_client_ip_addr();
        let core =
            into_core_request_with_ingress(native, snapshot()).expect("should convert snapshot");
        assert_eq!(
            FastlyRequestContext::get(&core)
                .expect("should preserve context")
                .client_ip,
            client_ip
        );
        assert!(core.extensions().get::<ProxyHandle>().is_some());
        assert_eq!(core.method().as_str(), "EXAMPLE-METHOD");
        let values: Vec<_> = core
            .headers()
            .get_all("cookie")
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(values, [b"a=\xff".as_slice(), b"b=2".as_slice()]);
        let CapturedTarget::Complete(target) = core
            .extensions()
            .get::<RequestIngress>()
            .expect("should carry snapshot")
            .target()
        else {
            panic!("should retain target")
        };
        assert_eq!(target.value(), "/reserved/%2e");
    }

    #[test]
    fn ingress_registry_dispatch_wins_scratch_collision() {
        let app = App::new(
            RouterService::builder()
                .pre_dispatch_hook(Arc::new(VerifySnapshot {
                    scratch_expected: true,
                }))
                .build(),
        );
        let response = dispatch_with_registries_and_ingress(
            &app,
            FastlyRequest::get("https://example.com/later"),
            StoresMetadata::default(),
            &EnvConfig::default(),
            snapshot(),
            |native, scratch| {
                scratch.insert(capture_request_ingress(native));
                scratch.insert(ScratchMarker);
            },
        )
        .expect("should dispatch with chosen snapshot");
        assert_eq!(response.get_status().as_u16(), 204);
        let service_app = App::new(
            RouterService::builder()
                .pre_dispatch_hook(Arc::new(VerifySnapshot {
                    scratch_expected: false,
                }))
                .build(),
        );
        let service_response = FastlyService::new(&service_app)
            .with_request_ingress(snapshot())
            .dispatch(FastlyRequest::get("https://example.com/later"))
            .expect("should dispatch service snapshot");
        assert_eq!(service_response.get_status().as_u16(), 204);
    }
}
