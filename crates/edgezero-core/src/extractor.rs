use std::any;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;

use async_trait::async_trait;
use http::header;
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::app_config::{AppConfigMeta, SecretField, SecretKind, SecretPathSegment};
use crate::blob_envelope::{BlobEnvelope, BlobEnvelopeError};
use crate::config_store::{ConfigExtractionLimits, ConfigStoreError, ConfigStoreHandle};
use crate::context::RequestContext;
use crate::error::{EdgeError, StoreExtractionReason};
use crate::http::HeaderMap;
use crate::secret_store::SecretError;
use crate::store_registry::{
    BoundConfigStore, BoundKvStore, BoundSecretStore, ConfigRegistry, ConfigStoreBinding,
    KvRegistry, SecretRegistry,
};
use crate::time::{Deadline, MonotonicInstant};
use serde::de::IntoDeserializer as _;

#[async_trait(?Send)]
pub trait FromRequest: Sized {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError>;
}

pub struct Json<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for Json<T>
where
    T: DeserializeOwned + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.json().map(Json)
    }
}

impl<T> Deref for Json<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Json<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Json<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct ValidatedJson<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for ValidatedJson<T>
where
    T: DeserializeOwned + Validate + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let Json(value) = Json::<T>::from_request(ctx).await?;
        value
            .validate()
            .map_err(|err| EdgeError::validation(err.to_string()))?;
        Ok(ValidatedJson(value))
    }
}

impl<T> Deref for ValidatedJson<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedJson<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedJson<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct Headers(pub HeaderMap);

#[async_trait(?Send)]
impl FromRequest for Headers {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        Ok(Headers(ctx.request().headers().clone()))
    }
}

impl Deref for Headers {
    type Target = HeaderMap;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Headers {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Headers {
    #[must_use]
    #[inline]
    pub fn into_inner(self) -> HeaderMap {
        self.0
    }
}

/// Extracts the host from the standard `Host` header.
///
/// Falls back to "localhost" if the header is not present.
///
/// # Example
/// ```ignore
/// #[action]
/// pub async fn handler(Host(host): Host) -> Response {
///     // host contains the hostname from the Host header
/// }
/// ```
pub struct Host(pub String);

#[async_trait(?Send)]
impl FromRequest for Host {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let headers = ctx.request().headers();
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost")
            .to_owned();
        Ok(Host(host))
    }
}

impl Deref for Host {
    type Target = String;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Host {
    #[must_use]
    #[inline]
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// Extracts the effective host from the request, checking forwarded headers first.
///
/// Checks headers in this order:
/// 1. `X-Forwarded-Host` - set by reverse proxies/load balancers
/// 2. `Host` - standard HTTP host header
/// 3. Falls back to "localhost" if neither is present
///
/// Use this extractor when your application is behind a reverse proxy or load balancer.
///
/// # Example
/// ```ignore
/// #[action]
/// pub async fn handler(ForwardedHost(host): ForwardedHost) -> Response {
///     // host contains the effective hostname (X-Forwarded-Host or Host)
/// }
/// ```
pub struct ForwardedHost(pub String);

#[async_trait(?Send)]
impl FromRequest for ForwardedHost {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let headers = ctx.request().headers();
        let host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost")
            .to_owned();
        Ok(ForwardedHost(host))
    }
}

impl Deref for ForwardedHost {
    type Target = String;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ForwardedHost {
    #[must_use]
    #[inline]
    pub fn into_inner(self) -> String {
        self.0
    }
}

pub struct Query<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for Query<T>
where
    T: DeserializeOwned + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.query().map(Query)
    }
}

impl<T> Deref for Query<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Query<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Query<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct ValidatedQuery<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for ValidatedQuery<T>
where
    T: DeserializeOwned + Validate + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let Query(value) = Query::<T>::from_request(ctx).await?;
        value
            .validate()
            .map_err(|err| EdgeError::validation(err.to_string()))?;
        Ok(ValidatedQuery(value))
    }
}

impl<T> Deref for ValidatedQuery<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedQuery<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedQuery<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct Path<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for Path<T>
where
    T: DeserializeOwned + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.path().map(Path)
    }
}

impl<T> Deref for Path<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Path<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Path<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct ValidatedPath<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for ValidatedPath<T>
where
    T: DeserializeOwned + Validate + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let Path(value) = Path::<T>::from_request(ctx).await?;
        value
            .validate()
            .map_err(|err| EdgeError::validation(err.to_string()))?;
        Ok(ValidatedPath(value))
    }
}

impl<T> Deref for ValidatedPath<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedPath<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedPath<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct Form<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for Form<T>
where
    T: DeserializeOwned + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.form().map(Form)
    }
}

impl<T> Deref for Form<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Form<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Form<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct ValidatedForm<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for ValidatedForm<T>
where
    T: DeserializeOwned + Validate + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let Form(value) = Form::<T>::from_request(ctx).await?;
        value
            .validate()
            .map_err(|err| EdgeError::validation(err.to_string()))?;
        Ok(ValidatedForm(value))
    }
}

impl<T> Deref for ValidatedForm<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedForm<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedForm<T> {
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// Extractor that yields the per-request [`KvRegistry`].
///
/// Handlers pick a bound store by id at the call site:
///
/// ```ignore
/// #[action]
/// pub async fn handler(kv: Kv) -> Result<String, EdgeError> {
///     let store = kv.default().ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no default kv")))?;
///     let count: i32 = store.get_or("visits", 0).await?;
///     store.put("visits", &(count + 1)).await?;
///     Ok(format!("visits: {}", count + 1))
/// }
/// ```
///
/// Or, for a non-default id:
///
/// ```ignore
/// let cache = kv.named("cache").ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no `cache` kv")))?;
/// ```
#[derive(Clone, Debug)]
pub struct Kv(KvRegistry);

#[async_trait(?Send)]
impl FromRequest for Kv {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        // Spec hard-cutoff ( intro): no backward compatibility for
        // the pre-rewrite runtime store API. Pre-Stage-9.3 this
        // extractor silently synthesised a one-id registry from a
        // lone `ctx.kv_handle()` when no `KvRegistry` was wired,
        // which masked missing registry wiring. Adapter dispatchers
        // (axum / cloudflare / fastly / spin) now normalise
        // legacy bare-handle inputs to single-id registries at the
        // dispatch boundary, so this path no longer needs a
        // fallback — a missing registry is a real bug.
        ctx.request()
            .extensions()
            .get::<KvRegistry>()
            .cloned()
            .map(Kv)
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "no kv store configured -- check [stores.kv] in edgezero.toml and platform bindings"
                ))
            })
    }
}

impl Kv {
    /// Resolve the default [`BoundKvStore`].
    #[must_use]
    #[inline]
    pub fn default(&self) -> Option<BoundKvStore> {
        self.0.default()
    }

    /// Resolve the [`BoundKvStore`] for `id`. Strict lookup — unknown ids
    /// yield `None`.
    #[must_use]
    #[inline]
    pub fn named(&self, id: &str) -> Option<BoundKvStore> {
        self.0.named(id)
    }

    /// Access the underlying registry directly (rarely needed; most handlers
    /// should use [`Self::default`] / [`Self::named`]).
    #[must_use]
    #[inline]
    pub fn registry(&self) -> &KvRegistry {
        &self.0
    }
}

/// Extractor for app-owned shared state registered via
/// [`RouterBuilder::with_state`]. Resolves by type from request extensions.
///
/// Typically `T = Arc<AppState>`. The registered value is cloned into every
/// request's extensions before dispatch; registering the same `T` twice is
/// last-write-wins.
///
/// ```ignore
/// use edgezero_core::extractor::State;
/// use std::sync::Arc;
///
/// #[edgezero_core::action]
/// async fn handle(State(state): State<Arc<AppState>>) -> Result<String, edgezero_core::error::EdgeError> {
///     Ok(state.greeting.clone())
/// }
/// ```
///
/// [`RouterBuilder::with_state`]: crate::router::RouterBuilder::with_state
pub struct State<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.extension::<T>().map(State).ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no `State<{}>` registered -- call RouterBuilder::with_state(..) before build()",
                any::type_name::<T>()
            ))
        })
    }
}

impl<T> Deref for State<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for State<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> State<T> {
    /// Consume the extractor and return the inner value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// Extractor that yields the per-request [`SecretRegistry`].
///
/// The returned [`BoundSecretStore`] is pre-bound to a platform store name
/// (resolved per id from `EDGEZERO__STORES__SECRETS__<ID>__NAME`), so
/// handler code passes only the key:
///
/// ```ignore
/// #[action]
/// pub async fn handler(secrets: Secrets) -> Result<Response, EdgeError> {
///     let bound = secrets.default().ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no secrets")))?;
///     let key = bound.require_str("API_KEY").await.map_err(EdgeError::from)?;
///     // ...
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Secrets(SecretRegistry);

#[async_trait(?Send)]
impl FromRequest for Secrets {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        // Hard-cutoff: see `impl FromRequest for Kv`. Adapter
        // dispatchers normalise legacy bare-handle inputs to
        // single-id `SecretRegistry`s at the dispatch boundary.
        ctx.request()
            .extensions()
            .get::<SecretRegistry>()
            .cloned()
            .map(Secrets)
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "no secret store configured -- check [stores.secrets] in edgezero.toml and platform bindings"
                ))
            })
    }
}

impl Secrets {
    /// Resolve the default [`BoundSecretStore`].
    #[must_use]
    #[inline]
    pub fn default(&self) -> Option<BoundSecretStore> {
        self.0.default()
    }

    /// Resolve the [`BoundSecretStore`] for `id`. Strict lookup — unknown ids
    /// yield `None`.
    #[must_use]
    #[inline]
    pub fn named(&self, id: &str) -> Option<BoundSecretStore> {
        self.0.named(id)
    }

    /// Access the underlying registry directly.
    #[must_use]
    #[inline]
    pub fn registry(&self) -> &SecretRegistry {
        &self.0
    }
}

/// Extractor that yields the per-request [`ConfigRegistry`].
///
/// ```ignore
/// #[action]
/// pub async fn handler(config: Config) -> Result<Response, EdgeError> {
///     let bound = config.default().ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no config")))?;
///     let greeting = bound.get("greeting").await?.unwrap_or_default();
///     // ...
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Config(ConfigRegistry);

#[async_trait(?Send)]
impl FromRequest for Config {
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        // Hard-cutoff: see `impl FromRequest for Kv`. Adapter
        // dispatchers normalise legacy bare-handle inputs to
        // single-id `ConfigRegistry`s at the dispatch boundary.
        ctx.request()
            .extensions()
            .get::<ConfigRegistry>()
            .cloned()
            .map(Config)
            .ok_or_else(|| {
                EdgeError::internal(anyhow::anyhow!(
                    "no config store configured -- check [stores.config] in edgezero.toml and platform bindings"
                ))
            })
    }
}

impl Config {
    /// Resolve the default [`BoundConfigStore`].
    #[must_use]
    #[inline]
    pub fn default(&self) -> Option<BoundConfigStore> {
        self.0.default().map(|binding| binding.handle)
    }

    /// Borrow the default binding (handle + resolved __KEY) without
    /// cloning. Used by the typed `AppConfig<C>` extractor.
    #[must_use]
    #[inline]
    pub fn default_binding(&self) -> Option<&ConfigStoreBinding> {
        self.0.default_ref()
    }

    /// Resolve the [`BoundConfigStore`] for `id`. Strict lookup — unknown ids
    /// yield `None`.
    #[must_use]
    #[inline]
    pub fn named(&self, id: &str) -> Option<BoundConfigStore> {
        self.0.named(id).map(|binding| binding.handle)
    }

    /// Borrow a binding by id.
    #[must_use]
    #[inline]
    pub fn named_binding(&self, id: &str) -> Option<&ConfigStoreBinding> {
        self.0.named_ref(id)
    }

    /// Access the underlying registry directly.
    #[must_use]
    #[inline]
    pub fn registry(&self) -> &ConfigRegistry {
        &self.0
    }
}

// removed the private `single_id_registry` helper that
// the Kv/Config/Secrets extractors used to synthesise a one-id
// registry from a legacy bare handle. The equivalent normalisation
// now happens at each adapter's dispatch boundary via
// `StoreRegistry::single_id`, so this fallback is no longer
// reachable from the extractor path.

// ---------------------------------------------------------------------------
// AppConfig<C> — typed app-config extractor (spec 3.3, 3.3.3, 4.3)
// ---------------------------------------------------------------------------

/// Typed app-config extractor. See spec 3.3.3 + 4.3.
///
/// ```ignore
/// #[action]
/// pub async fn handler(AppConfig(cfg): AppConfig<MyConfig>) -> Result<Response, EdgeError> {
///     // cfg.api_token is the RESOLVED secret value, not the key name.
///     Ok(text(cfg.greeting))
/// }
/// ```
#[derive(Debug)]
pub struct AppConfig<C>(pub C);

#[async_trait(?Send)]
impl<C> FromRequest for AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::store_extraction(
                StoreExtractionReason::MissingRegistry,
                "no default config store registered; check [stores.config] in edgezero.toml",
                None,
            )
        })?;
        let key = binding.default_key.clone();
        extract_from_handle::<C>(ctx, &binding.handle, &key)
            .await
            .map(AppConfig)
    }
}

impl<C> AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    /// Read the typed config from a NON-default config store.
    /// `key = None` falls back to that store's `binding.default_key`.
    /// Returns the inner `C` directly per spec 6.2.1.
    ///
    /// # Errors
    /// See `extract_from_handle`.
    #[inline]
    pub async fn from_store(
        ctx: &RequestContext,
        store_id: &str,
        key: Option<&str>,
    ) -> Result<C, EdgeError> {
        let binding = ctx.config_store_binding(store_id).ok_or_else(|| {
            EdgeError::store_extraction(
                StoreExtractionReason::UnknownStore,
                "no config store is registered for the requested id",
                None,
            )
        })?;
        let resolved_key = key.unwrap_or(&binding.default_key).to_owned();
        extract_from_handle::<C>(ctx, &binding.handle, &resolved_key).await
    }

    /// Read the typed config from the default store under an
    /// EXPLICIT key (instead of the binding's `default_key`).
    /// Returns the inner `C` directly per spec 6.2 — handlers
    /// usually destructure the `FromRequest` extractor; the inherent
    /// methods exist for call sites that need a different key or
    /// store and prefer the bare `C` over wrapping/unwrapping.
    ///
    /// # Errors
    /// See `extract_from_handle`.
    #[inline]
    pub async fn named(ctx: &RequestContext, key: &str) -> Result<C, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::store_extraction(
                StoreExtractionReason::MissingRegistry,
                "no default config store registered; check [stores.config] in edgezero.toml",
                None,
            )
        })?;
        extract_from_handle::<C>(ctx, &binding.handle, key).await
    }
}

/// Extraction-scoped accounting. One instance is created for each public
/// `AppConfig` call and shared by the root blob and all secret reads.
struct ConfigExtractionBudget {
    deadline: Deadline,
    max_blob_bytes: u64,
    max_secret_bytes: u64,
    remaining_backend_bytes: u64,
    remaining_total_bytes: u64,
}

impl ConfigExtractionBudget {
    fn accept_read(
        &mut self,
        backend_bytes: u64,
        value_length: Option<usize>,
        max_value_bytes: u64,
    ) -> Result<(), EdgeError> {
        if self.deadline.is_expired() {
            return Err(store_extraction_error(
                StoreExtractionReason::DeadlineExceeded,
                "typed app-config extraction deadline exceeded",
                None,
            ));
        }

        let converted_value_bytes = value_length
            .map(u64::try_from)
            .transpose()
            .map_err(|_length_error| backend_contract_error())?;
        if backend_bytes > self.remaining_backend_bytes
            || converted_value_bytes
                .is_some_and(|bytes| bytes > max_value_bytes || backend_bytes < bytes)
        {
            return Err(backend_contract_error());
        }

        self.remaining_backend_bytes = self
            .remaining_backend_bytes
            .checked_sub(backend_bytes)
            .ok_or_else(backend_contract_error)?;
        if let Some(retained_bytes) = converted_value_bytes {
            self.remaining_total_bytes = self
                .remaining_total_bytes
                .checked_sub(retained_bytes)
                .ok_or_else(|| {
                    store_extraction_error(
                        StoreExtractionReason::ValueTooLarge,
                        "typed app-config extraction exceeded its cumulative byte limit",
                        None,
                    )
                })?;
        }
        Ok(())
    }

    fn deadline(&self) -> Deadline {
        self.deadline
    }

    fn max_blob_bytes(&self) -> u64 {
        self.max_blob_bytes
    }

    fn max_secret_bytes(&self) -> u64 {
        self.max_secret_bytes
    }

    fn remaining_backend_bytes(&self) -> u64 {
        self.remaining_backend_bytes
    }

    fn start(configured_limits: ConfigExtractionLimits) -> Result<Self, EdgeError> {
        let validated_limits = configured_limits.validate()?;
        let started_at = MonotonicInstant::now();
        let deadline = started_at
            .checked_add(validated_limits.timeout)
            .ok_or_else(|| {
                EdgeError::store_extraction(
                    StoreExtractionReason::BackendFailure,
                    "config extraction deadline could not be represented",
                    None,
                )
            })?;
        Ok(Self {
            deadline: Deadline::at_instant(deadline),
            max_blob_bytes: validated_limits.max_blob_bytes,
            max_secret_bytes: validated_limits.max_secret_bytes,
            remaining_backend_bytes: validated_limits.max_backend_bytes,
            remaining_total_bytes: validated_limits.max_total_bytes,
        })
    }
}

fn backend_contract_error() -> EdgeError {
    store_extraction_error(
        StoreExtractionReason::BackendFailure,
        "a store backend violated the bounded-read contract",
        None,
    )
}

fn store_extraction_error(
    reason: StoreExtractionReason,
    message: impl Into<String>,
    field_path: Option<String>,
) -> EdgeError {
    EdgeError::store_extraction(reason, message, field_path)
}

/// A redacted reason when `raw` is a NEWER format this build must not apply, or
/// `None` when it is not (that stays ordinary corruption).
///
/// A cheap, schema-agnostic pre-check that parses only as a generic JSON object,
/// so it catches a newer format even when the value no longer deserializes as the
/// exact v1 [`BlobEnvelope`]. It is future when EITHER:
/// - it carries an `edgezero_kind` field. A v1 `BlobEnvelope` never has one;
///   serde silently ignores the unknown field, so without this a v1-shaped
///   envelope tagged `edgezero_kind: "new_format"` would deserialize and apply on
///   non-Fastly runtimes (the generic push already refuses that exact value); or
/// - it carries a `version` field present but NOT 1.
///
/// The reason names only the integer version or the field name — never a value,
/// so it is safe to surface in the (HTTP-visible) remediation message.
fn future_format_reason(raw: &str) -> Option<String> {
    use crate::blob_envelope::ENVELOPE_VERSION_V1;
    let serde_json::Value::Object(obj) = serde_json::from_str::<serde_json::Value>(raw).ok()?
    else {
        return None;
    };
    if obj.contains_key("edgezero_kind") {
        return Some("an unrecognised `edgezero_kind` discriminator".to_owned());
    }
    // A `version` PRESENT but not EXACTLY integer 1 is a newer format. `as_u64()`
    // alone fails open on `"2"`, `-1`, `2.5`, or `1.0`. A non-integer version is
    // value-controlled, so it is NOT echoed -- only a plain integer is named.
    match obj.get("version") {
        Some(version) if version.as_u64() != Some(u64::from(ENVELOPE_VERSION_V1)) => {
            Some(match version.as_u64() {
                Some(number) => format!("envelope version {number}"),
                None => "an unrecognised envelope version".to_owned(),
            })
        }
        _ => None,
    }
}

/// Shared body: fetch + envelope + sha + secret walk + deserialise + validate.
///
/// The `FromRequest` impl and the `named`/`from_store` inherent methods all
/// delegate here so there is one implementation path.
///
/// # Errors
///
/// Returns [`EdgeError::StoreExtraction`] with the stable reason corresponding
/// to store, envelope, secret, or schema failure.
async fn extract_from_handle<C>(
    ctx: &RequestContext,
    handle: &ConfigStoreHandle,
    key: &str,
) -> Result<C, EdgeError>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    let mut budget = ConfigExtractionBudget::start(ctx.config_extraction_limits())?;
    let read = handle
        .get_bounded(
            key,
            budget.deadline(),
            budget.remaining_backend_bytes(),
            budget.max_blob_bytes(),
        )
        .await
        .map_err(|store_error| map_config_store_error(&store_error))?;
    budget.accept_read(
        read.backend_bytes,
        read.value.as_ref().map(String::len),
        budget.max_blob_bytes(),
    )?;
    let raw = read.value.ok_or_else(|| {
        store_extraction_error(
            StoreExtractionReason::MissingBlob,
            format!(
                "missing typed app-config blob at key `{key}`; run `<app-cli> config push` for this deploy"
            ),
            None,
        )
    })?;

    // Neither parsing nor verification diagnostics may echo stored values.
    let kind_tagged = raw.contains("edgezero_kind");
    let envelope: BlobEnvelope = match serde_json::from_str::<BlobEnvelope>(&raw) {
        Ok(envelope) if !kind_tagged => envelope,
        parsed => {
            if let Some(reason) = future_format_reason(&raw) {
                return Err(store_extraction_error(
                    StoreExtractionReason::InvalidEnvelope,
                    format!(
                        "typed app-config blob uses {reason}, which this build does not understand; redeploy this service with an updated build"
                    ),
                    None,
                ));
            }
            parsed.map_err(|_err| {
                store_extraction_error(
                    StoreExtractionReason::InvalidEnvelope,
                    "typed app-config blob is not a valid envelope (details redacted)",
                    None,
                )
            })?
        }
    };
    envelope.verify().map_err(|err| match err {
        BlobEnvelopeError::UnknownVersion(version) => store_extraction_error(
            StoreExtractionReason::InvalidEnvelope,
            format!(
                "typed app-config blob uses envelope version {version}, which this build does not understand; redeploy this service with an updated build"
            ),
            None,
        ),
        BlobEnvelopeError::ShaMismatch { .. } => store_extraction_error(
            StoreExtractionReason::IntegrityMismatch,
            "typed app-config blob failed its integrity check (details redacted)",
            None,
        ),
    })?;
    let mut data = envelope.into_data();
    secret_walk::<C>(ctx, &mut budget, &mut data).await?;
    let cfg: C = serde_path_to_error::deserialize(data.into_deserializer())
        .map_err(|serde_error| EdgeError::store_schema_mismatch_from_serde(&serde_error))?;
    cfg.validate().map_err(|err| {
        let field = first_violating_field(&err).unwrap_or_default();
        let message = if field.is_empty() {
            "app config failed validation".to_owned()
        } else {
            format!("app config failed validation for field `{field}`")
        };
        store_extraction_error(
            StoreExtractionReason::SchemaMismatch,
            message,
            (!field.is_empty()).then_some(field),
        )
    })?;
    Ok(cfg)
}

fn map_config_store_error(err: &ConfigStoreError) -> EdgeError {
    match err {
        ConfigStoreError::DeadlineExceeded => store_extraction_error(
            StoreExtractionReason::DeadlineExceeded,
            "typed app-config store read deadline exceeded",
            None,
        ),
        ConfigStoreError::Internal { .. } => store_extraction_error(
            StoreExtractionReason::BackendFailure,
            "typed app-config store read failed (details redacted)",
            None,
        ),
        ConfigStoreError::InvalidKey { .. } => store_extraction_error(
            StoreExtractionReason::InvalidKey,
            "typed app-config store rejected the requested key (details redacted)",
            None,
        ),
        ConfigStoreError::Unavailable { .. } => store_extraction_error(
            StoreExtractionReason::BackendUnavailable,
            "typed app-config store is unavailable",
            None,
        ),
        ConfigStoreError::ValueTooLarge => store_extraction_error(
            StoreExtractionReason::ValueTooLarge,
            "typed app-config store value exceeded its read limit",
            None,
        ),
    }
}

/// Walk `C::secret_fields()` and replace each `#[secret]` key NAME in `data`
/// with the resolved secret VALUE from the appropriate secret store.
///
/// `StoreRef` fields are skipped — their value is a store id, not a key.
async fn secret_walk<C>(
    ctx: &RequestContext,
    budget: &mut ConfigExtractionBudget,
    data: &mut serde_json::Value,
) -> Result<(), EdgeError>
where
    C: AppConfigMeta,
{
    for field in C::secret_fields() {
        // `StoreRef` holds a store id, not a secret key — skip it here (no
        // descent, no resolution). Its value is consumed by a sibling
        // `KeyInNamedStore` leaf, and it's validated at push time.
        if matches!(field.kind, SecretKind::StoreRef) {
            continue;
        }
        resolve_secret_field(ctx, budget, data, &field, &field.path, String::new()).await?;
    }
    Ok(())
}

/// Recursively descend `remaining` path segments from `node`, resolving the
/// secret leaf(s). `rendered` is the dotted path so far (with concrete `[n]`
/// indices) for error hints.
fn resolve_secret_field<'walk>(
    ctx: &'walk RequestContext,
    budget: &'walk mut ConfigExtractionBudget,
    node: &'walk mut serde_json::Value,
    field: &'walk SecretField,
    remaining: &'walk [SecretPathSegment],
    rendered: String,
) -> Pin<Box<dyn Future<Output = Result<(), EdgeError>> + 'walk>> {
    Box::pin(async move {
        match remaining.split_first() {
            // Leaf reached: `node` is the PARENT object; the last field is the key.
            Some((SecretPathSegment::Field(name), [])) => {
                resolve_leaf(ctx, budget, node, field, name.as_ref(), &rendered).await
            }
            Some((SecretPathSegment::OptionalField(name), [])) => {
                if node.as_object().is_some_and(|parent| {
                    matches!(
                        parent.get(name.as_ref()),
                        None | Some(serde_json::Value::Null)
                    )
                }) {
                    return Ok(());
                }
                resolve_leaf(ctx, budget, node, field, name.as_ref(), &rendered).await
            }
            // Required intermediates still reject stale blobs. Optional
            // intermediates are represented explicitly below rather than by the
            // leaf's `field.optional` flag.
            Some((SecretPathSegment::Field(name), rest)) => {
                let next_rendered = join_field(&rendered, name.as_ref());
                match node.get_mut(name.as_ref()) {
                    None | Some(serde_json::Value::Null) => Err(store_extraction_error(
                        StoreExtractionReason::SchemaMismatch,
                        format!("missing or null value at `{next_rendered}`"),
                        Some(next_rendered),
                    )),
                    Some(child) => {
                        resolve_secret_field(ctx, budget, child, field, rest, next_rendered).await
                    }
                }
            }
            Some((SecretPathSegment::OptionalField(name), rest)) => {
                let Some(parent) = node.as_object_mut() else {
                    return Err(store_extraction_error(
                        StoreExtractionReason::SchemaMismatch,
                        format!("expected an object at `{rendered}`"),
                        Some(rendered),
                    ));
                };
                let next_rendered = join_field(&rendered, name.as_ref());
                match parent.get_mut(name.as_ref()) {
                    None | Some(serde_json::Value::Null) => Ok(()),
                    Some(child) => {
                        resolve_secret_field(ctx, budget, child, field, rest, next_rendered).await
                    }
                }
            }
            // Iterate every array element. The array itself is a required
            // intermediate unless its containing field was optional above.
            Some((SecretPathSegment::ArrayEach, rest)) => {
                let Some(items) = node.as_array_mut() else {
                    return Err(store_extraction_error(
                        StoreExtractionReason::SchemaMismatch,
                        format!("expected an array at `{rendered}`"),
                        Some(rendered),
                    ));
                };
                for (idx, item) in items.iter_mut().enumerate() {
                    let indexed = format!("{rendered}[{idx}]");
                    resolve_secret_field(ctx, budget, item, field, rest, indexed).await?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    })
}

fn join_field(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Resolve one leaf: `parent` is the innermost containing object; `key` is the
/// secret field name; `store_ref_field` (for `KeyInNamedStore`) is a sibling
/// within `parent`.
async fn resolve_leaf(
    ctx: &RequestContext,
    budget: &mut ConfigExtractionBudget,
    parent: &mut serde_json::Value,
    field: &SecretField,
    key: &str,
    rendered_parent: &str,
) -> Result<(), EdgeError> {
    // `StoreRef` is filtered out in `secret_walk` before any descent, so it
    // never reaches here. Traversal handles optional intermediates; once a leaf
    // is reached, its present parent must be an object and only the leaf key
    // below honors `field.optional`.
    let leaf_path = join_field(rendered_parent, key);

    let Some(parent_obj) = parent.as_object_mut() else {
        return Err(store_extraction_error(
            StoreExtractionReason::SchemaMismatch,
            format!("expected an object containing `{key}` at `{rendered_parent}`"),
            Some(leaf_path),
        ));
    };

    let key_name = match parent_obj.get(key) {
        Some(serde_json::Value::String(name)) => name.clone(),
        // An optional secret is absent when the key is MISSING *or* serialized
        // as JSON `null`. serde emits `Option::None` as `null` (and `#[secret]`
        // bans `skip_serializing_if`, so the key is never omitted), so both
        // cases must skip — not just the missing-key case.
        None | Some(serde_json::Value::Null) if field.optional => return Ok(()),
        _ => {
            return Err(store_extraction_error(
                StoreExtractionReason::SchemaMismatch,
                format!("missing or non-string value at `{leaf_path}`"),
                Some(leaf_path),
            ));
        }
    };

    let (bound, resolved_store_id) = match field.kind {
        SecretKind::KeyInDefault => {
            let bound = ctx.secret_store_default().ok_or_else(|| {
                store_extraction_error(
                    StoreExtractionReason::MissingRegistry,
                    format!(
                        "secret field `{leaf_path}` has kind KeyInDefault but no default secret \
                         store is registered"
                    ),
                    Some(leaf_path.clone()),
                )
            })?;
            let id = bound.store_name().to_owned();
            (bound, id)
        }
        SecretKind::StoreRef => return Ok(()),
        SecretKind::KeyInNamedStore { store_ref_field } => {
            let store_id_str = parent_obj
                .get(store_ref_field)
                .and_then(|val| val.as_str())
                .ok_or_else(|| {
                    store_extraction_error(
                        StoreExtractionReason::SchemaMismatch,
                        format!(
                            "missing store_ref `{store_ref_field}` for secret field `{leaf_path}`"
                        ),
                        Some(leaf_path.clone()),
                    )
                })?
                .to_owned();
            let bound = ctx.secret_store(&store_id_str).ok_or_else(|| {
                // `store_id_str` is the blob's store_ref VALUE — config data that
                // may be sensitive — and this message reaches the HTTP body. Name
                // the field, not the stored id.
                store_extraction_error(
                    StoreExtractionReason::UnknownStore,
                    format!(
                        "secret field `{leaf_path}` names a store_ref that is not declared in \
                         [stores.secrets] (id redacted)"
                    ),
                    Some(leaf_path.clone()),
                )
            })?;
            (bound, store_id_str)
        }
    };

    let read = bound
        .get_bytes_bounded(
            &key_name,
            budget.deadline(),
            budget.remaining_backend_bytes(),
            budget.max_secret_bytes(),
        )
        .await
        .map_err(|err| map_secret_error(err, &leaf_path, &resolved_store_id, &key_name))?;
    budget.accept_read(
        read.backend_bytes,
        read.value.as_ref().map(bytes::Bytes::len),
        budget.max_secret_bytes(),
    )?;
    let secret_bytes = read.value.ok_or_else(|| {
        store_extraction_error(
            StoreExtractionReason::MissingSecret,
            format!(
                "the secret referenced by `{leaf_path}` was not found in its store (identifier redacted)"
            ),
            Some(leaf_path.clone()),
        )
    })?;
    let secret = String::from_utf8(secret_bytes.to_vec()).map_err(|_utf8_error| {
        store_extraction_error(
            StoreExtractionReason::InvalidSecretValue,
            format!("secret for field `{leaf_path}` is not valid UTF-8"),
            Some(leaf_path.clone()),
        )
    })?;
    parent_obj.insert(key.to_owned(), serde_json::Value::String(secret));
    Ok(())
}

fn map_secret_error(
    err: SecretError,
    field_name: &str,
    // The stored key name, the store id, and the provider's message/source are
    // deliberately UNUSED in the messages below: they are blob- or
    // provider-controlled strings that may reveal the secret or infrastructure,
    // and these messages reach the HTTP body / logs. Every branch names only the
    // offending FIELD (schema, safe). Kept in the signature so the redaction is
    // visible at the one place these values could have been formatted.
    _store_id: &str,
    _key_name: &str,
) -> EdgeError {
    match err {
        SecretError::DeadlineExceeded => EdgeError::store_extraction(
            StoreExtractionReason::DeadlineExceeded,
            format!("secret resolution for `{field_name}` exceeded its deadline"),
            Some(field_name.to_owned()),
        ),
        SecretError::Internal(_source) => EdgeError::store_extraction(
            StoreExtractionReason::BackendFailure,
            format!("secret resolution for `{field_name}` failed (details redacted)"),
            Some(field_name.to_owned()),
        ),
        SecretError::NotFound { .. } => EdgeError::store_extraction(
            StoreExtractionReason::MissingSecret,
            format!(
                "the secret referenced by `{field_name}` was not found in its store (identifier redacted)"
            ),
            Some(field_name.to_owned()),
        ),
        SecretError::Unavailable => EdgeError::store_extraction(
            StoreExtractionReason::SecretBackendUnavailable,
            format!("the secret store for `{field_name}` is unreachable"),
            Some(field_name.to_owned()),
        ),
        SecretError::Validation(_msg) => EdgeError::store_extraction(
            StoreExtractionReason::InvalidKey,
            format!(
                "the secret referenced by `{field_name}` was rejected by its store (details redacted)"
            ),
            Some(field_name.to_owned()),
        ),
        SecretError::ValueTooLarge => EdgeError::store_extraction(
            StoreExtractionReason::ValueTooLarge,
            format!("the secret referenced by `{field_name}` exceeds its configured byte limit"),
            Some(field_name.to_owned()),
        ),
    }
}

/// Walk `errors` recursively and return the first violating field's DOTTED
/// PATH (e.g. `"service.timeout_ms"` for a nested failure).
///
/// Keys are sorted for determinism across runs — required for the 6.3.1
/// contract test. An earlier draft only looked at top-level keys,
/// collapsing nested paths like `service.timeout_ms` to `"service"`.
fn first_violating_field(errors: &validator::ValidationErrors) -> Option<String> {
    fn walk(errors: &validator::ValidationErrors, prefix: &str, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        // Validator 0.20 stores keys as `Cow<'static, str>`.
        // `.as_ref()` gives `&str`; sort for determinism.
        let mut keys: Vec<&str> = errors.errors().keys().map(AsRef::as_ref).collect();
        keys.sort_unstable();
        for key in keys {
            let path = if prefix.is_empty() {
                key.to_owned()
            } else {
                format!("{prefix}.{key}")
            };
            match errors.errors().get(key) {
                Some(validator::ValidationErrorsKind::Field(_)) => {
                    *out = Some(path);
                    return;
                }
                Some(validator::ValidationErrorsKind::Struct(inner)) => {
                    walk(inner, &path, out);
                    if out.is_some() {
                        return;
                    }
                }
                Some(validator::ValidationErrorsKind::List(items)) => {
                    // BTreeMap<usize, Box<ValidationErrors>>; iterate
                    // in index order for determinism.
                    let mut indices: Vec<usize> = items.keys().copied().collect();
                    indices.sort_unstable();
                    for idx in indices {
                        if let Some(inner) = items.get(&idx) {
                            let indexed = format!("{path}[{idx}]");
                            walk(inner, &indexed, out);
                            if out.is_some() {
                                return;
                            }
                        }
                    }
                }
                None => {}
            }
        }
    }
    let mut out: Option<String> = None;
    walk(errors, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::{AppConfigMeta, SecretField, SecretKind, SecretPathSegment};
    use crate::blob_envelope::BlobEnvelope;
    use crate::body::Body;
    use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use crate::context::RequestContext;
    use crate::http::{HeaderValue, Method, StatusCode, request_builder};
    use crate::params::PathParams;
    use crate::secret_store::{InMemorySecretStore, NoopSecretStore, SecretHandle, SecretStore};
    use crate::store_registry::StoreRegistry;
    use futures::executor::block_on;
    use serde::{Deserialize, Serialize};
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::Arc;
    use validator::Validate;

    #[derive(Clone, Debug, PartialEq)]
    struct AppStateFixture {
        name: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct FormData {
        age: Option<u32>,
        username: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct PathPayload {
        id: String,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Payload {
        name: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct QueryParams {
        page: Option<u32>,
        #[serde(rename = "q")]
        query_term: Option<String>,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedFormData {
        #[validate(length(min = 3_u64))]
        username: String,
    }

    #[derive(Debug, Deserialize, Serialize, Validate)]
    struct ValidatedPayload {
        #[validate(length(min = 1_u64))]
        name: String,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedPathParams {
        #[validate(length(min = 1_u64, max = 10_u64))]
        id: String,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedQueryParams {
        #[validate(range(min = 1_u32, max = 100_u32))]
        page: u32,
    }

    // Fixture config type with no secret fields. Used by AppConfig<C> tests.
    #[derive(Debug, Deserialize, PartialEq, Serialize, Validate)]
    struct FixtureCfg {
        greeting: String,
        #[validate(range(min = 1_u32, max = 9999_u32))]
        timeout_ms: u32,
    }

    impl AppConfigMeta for FixtureCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![]
        }
    }

    // Fixture config type with one KeyInDefault secret field. Used by AppConfig<C> tests.
    // Fields are alphabetically ordered: api_token before greeting.
    #[derive(Debug, Deserialize, PartialEq, Validate)]
    struct SecretCfg {
        // Holds a key name pre-walk, resolved value post-walk.
        api_token: String,
        greeting: String,
    }

    impl AppConfigMeta for SecretCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                optional: false,
            }]
        }
    }

    // Array leaf: partners[*].api_key
    struct ArrayCfg;
    impl AppConfigMeta for ArrayCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(Cow::Borrowed("partners")),
                    SecretPathSegment::ArrayEach,
                    SecretPathSegment::Field(Cow::Borrowed("api_key")),
                ],
                optional: false,
            }]
        }
    }

    // Nested KeyInNamedStore: vaulted.token resolves against the store named by
    // its SIBLING vaulted.vault (the sibling-in-innermost-parent scoping rule).
    struct NamedStoreCfg;
    impl AppConfigMeta for NamedStoreCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInNamedStore {
                    store_ref_field: "vault",
                },
                path: vec![
                    SecretPathSegment::Field(Cow::Borrowed("vaulted")),
                    SecretPathSegment::Field(Cow::Borrowed("token")),
                ],
                optional: false,
            }]
        }
    }

    // Nested object leaf: integrations.datadome.server_side_key
    struct NestedCfg;
    impl AppConfigMeta for NestedCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(Cow::Borrowed("integrations")),
                    SecretPathSegment::Field(Cow::Borrowed("datadome")),
                    SecretPathSegment::Field(Cow::Borrowed("server_side_key")),
                ],
                optional: false,
            }]
        }
    }

    // Optional top-level leaf: maybe_key
    struct OptionalCfg;
    impl AppConfigMeta for OptionalCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![SecretPathSegment::Field(Cow::Borrowed("maybe_key"))],
                optional: true,
            }]
        }
    }

    // Optional leaf behind one required and one optional intermediate:
    // integrations.datadome.webhook_key
    struct OptionalNestedCfg;
    impl AppConfigMeta for OptionalNestedCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(Cow::Borrowed("integrations")),
                    SecretPathSegment::OptionalField(Cow::Borrowed("datadome")),
                    SecretPathSegment::Field(Cow::Borrowed("webhook_key")),
                ],
                optional: true,
            }]
        }
    }

    // Optional terminal segment supplied by hand-written metadata:
    // integrations.webhook_key
    struct TerminalOptionalCfg;
    impl AppConfigMeta for TerminalOptionalCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(Cow::Borrowed("integrations")),
                    SecretPathSegment::OptionalField(Cow::Borrowed("webhook_key")),
                ],
                optional: false,
            }]
        }
    }

    fn ctx(body: Body, params: PathParams) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri("/test")
            .body(body)
            .expect("request");
        RequestContext::new(request, params)
    }

    fn ctx_with_form(body: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri("/test")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_owned()))
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    fn ctx_with_query(query: &str) -> RequestContext {
        let uri = format!("/test?{query}");
        let request = request_builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    fn params(values: &[(&str, &str)]) -> PathParams {
        let map = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        PathParams::new(map)
    }

    #[test]
    fn json_extractor_parses_payload() {
        let body = Body::json(&Payload {
            name: "demo".into(),
        })
        .expect("json body");
        let ctx = ctx(body, PathParams::default());
        let payload = block_on(Json::<Payload>::from_request(&ctx)).expect("json");
        assert_eq!(payload.0.name, "demo");
    }

    #[test]
    fn json_extractor_propagates_errors() {
        let ctx = ctx(Body::from("not json"), PathParams::default());
        let err = block_on(Json::<Payload>::from_request(&ctx))
            .err()
            .expect("expected error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validated_json_rejects_invalid_payloads() {
        let body = Body::json(&ValidatedPayload {
            name: String::new(),
        })
        .expect("json");
        let ctx = ctx(body, PathParams::default());
        let err = block_on(ValidatedJson::<ValidatedPayload>::from_request(&ctx))
            .err()
            .expect("expected validation error");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn path_extractor_reads_params() {
        let ctx = ctx(Body::empty(), params(&[("id", "7")]));
        let payload = block_on(Path::<PathPayload>::from_request(&ctx)).expect("path");
        assert_eq!(payload.0.id, "7");
    }

    #[test]
    fn headers_extractor_clones_request_headers() {
        let mut ctx = ctx(Body::empty(), PathParams::default());
        ctx.request_mut()
            .headers_mut()
            .insert("x-test", HeaderValue::from_static("value"));
        let headers = block_on(Headers::from_request(&ctx)).expect("headers");
        assert_eq!(
            headers
                .get("x-test")
                .and_then(|value| value.to_str().ok())
                .unwrap(),
            "value"
        );
    }

    #[test]
    fn query_extractor_parses_params() {
        let ctx = ctx_with_query("page=5&q=hello");
        let query = block_on(Query::<QueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, Some(5));
        assert_eq!(query.query_term.as_deref(), Some("hello"));
    }

    #[test]
    fn query_extractor_handles_missing_optional_params() {
        let ctx = ctx_with_query("page=1");
        let query = block_on(Query::<QueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, Some(1));
        assert_eq!(query.query_term, None);
    }

    #[test]
    fn query_extractor_handles_empty_query() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let query = block_on(Query::<QueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, None);
        assert_eq!(query.query_term, None);
    }

    #[test]
    fn validated_query_accepts_valid_params() {
        let ctx = ctx_with_query("page=50");
        let query =
            block_on(ValidatedQuery::<ValidatedQueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, 50);
    }

    #[test]
    fn validated_query_rejects_invalid_params() {
        let ctx = ctx_with_query("page=200");
        let err = block_on(ValidatedQuery::<ValidatedQueryParams>::from_request(&ctx))
            .err()
            .expect("expected validation error");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn form_extractor_parses_urlencoded_body() {
        let ctx = ctx_with_form("username=alice&age=30");
        let form = block_on(Form::<FormData>::from_request(&ctx)).expect("form");
        assert_eq!(form.username, "alice");
        assert_eq!(form.age, Some(30));
    }

    #[test]
    fn form_extractor_handles_missing_optional_fields() {
        let ctx = ctx_with_form("username=bob");
        let form = block_on(Form::<FormData>::from_request(&ctx)).expect("form");
        assert_eq!(form.username, "bob");
        assert_eq!(form.age, None);
    }

    #[test]
    fn validated_form_accepts_valid_data() {
        let ctx = ctx_with_form("username=alice");
        let form = block_on(ValidatedForm::<ValidatedFormData>::from_request(&ctx)).expect("form");
        assert_eq!(form.username, "alice");
    }

    #[test]
    fn validated_form_rejects_invalid_data() {
        let ctx = ctx_with_form("username=ab");
        let err = block_on(ValidatedForm::<ValidatedFormData>::from_request(&ctx))
            .err()
            .expect("expected validation error");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn validated_path_accepts_valid_params() {
        let ctx = ctx(Body::empty(), params(&[("id", "abc123")]));
        let path =
            block_on(ValidatedPath::<ValidatedPathParams>::from_request(&ctx)).expect("path");
        assert_eq!(path.id, "abc123");
    }

    #[test]
    fn validated_path_rejects_invalid_params() {
        let ctx = ctx(Body::empty(), params(&[("id", "this-id-is-way-too-long")]));
        let err = block_on(ValidatedPath::<ValidatedPathParams>::from_request(&ctx))
            .err()
            .expect("expected validation error");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Deref/DerefMut and into_inner tests
    #[test]
    fn json_deref_and_into_inner() {
        let json = Json(Payload {
            name: "test".into(),
        });
        assert_eq!(json.name, "test"); // Deref
        let inner = json.into_inner();
        assert_eq!(inner.name, "test");
    }

    #[test]
    fn json_deref_mut() {
        let mut json = Json(Payload { name: "old".into() });
        json.name = "new".into(); // DerefMut
        assert_eq!(json.name, "new");
    }

    #[test]
    fn query_deref_and_into_inner() {
        let query = Query(QueryParams {
            page: Some(1),
            query_term: None,
        });
        assert_eq!(query.page, Some(1)); // Deref
        let inner = query.into_inner();
        assert_eq!(inner.page, Some(1));
    }

    #[test]
    fn query_deref_mut() {
        let mut query = Query(QueryParams {
            page: Some(1),
            query_term: None,
        });
        query.page = Some(2); // DerefMut
        assert_eq!(query.page, Some(2));
    }

    #[test]
    fn path_deref_and_into_inner() {
        let path = Path(PathPayload { id: "123".into() });
        assert_eq!(path.id, "123"); // Deref
        let inner = path.into_inner();
        assert_eq!(inner.id, "123");
    }

    #[test]
    fn path_deref_mut() {
        let mut path = Path(PathPayload { id: "old".into() });
        path.id = "new".into(); // DerefMut
        assert_eq!(path.id, "new");
    }

    #[test]
    fn form_deref_and_into_inner() {
        let form = Form(FormData {
            username: "alice".into(),
            age: Some(25),
        });
        assert_eq!(form.username, "alice"); // Deref
        let inner = form.into_inner();
        assert_eq!(inner.username, "alice");
    }

    #[test]
    fn form_deref_mut() {
        let mut form = Form(FormData {
            username: "alice".into(),
            age: None,
        });
        form.age = Some(30); // DerefMut
        assert_eq!(form.age, Some(30));
    }

    #[test]
    fn headers_deref_and_into_inner() {
        let mut map = HeaderMap::new();
        map.insert("x-custom", HeaderValue::from_static("value"));
        let headers = Headers(map);
        assert!(headers.get("x-custom").is_some()); // Deref
        let inner = headers.into_inner();
        assert!(inner.get("x-custom").is_some());
    }

    #[test]
    fn headers_deref_mut() {
        let mut headers = Headers(HeaderMap::new());
        headers.insert("x-new", HeaderValue::from_static("value")); // DerefMut
        assert!(headers.get("x-new").is_some());
    }

    #[test]
    fn validated_json_deref_and_into_inner() {
        let json = ValidatedJson(ValidatedPayload {
            name: "test".into(),
        });
        assert_eq!(json.name, "test"); // Deref
        let inner = json.into_inner();
        assert_eq!(inner.name, "test");
    }

    #[test]
    fn validated_json_deref_mut() {
        let mut json = ValidatedJson(ValidatedPayload { name: "old".into() });
        json.name = "new".into(); // DerefMut
        assert_eq!(json.name, "new");
    }

    #[test]
    fn validated_query_into_inner() {
        let query = ValidatedQuery(ValidatedQueryParams { page: 10 });
        assert_eq!(query.page, 10); // Deref
        let inner = query.into_inner();
        assert_eq!(inner.page, 10);
    }

    #[test]
    fn validated_query_deref_mut() {
        let mut query = ValidatedQuery(ValidatedQueryParams { page: 10 });
        query.page = 20; // DerefMut
        assert_eq!(query.page, 20);
    }

    #[test]
    fn validated_path_into_inner() {
        let path = ValidatedPath(ValidatedPathParams { id: "abc".into() });
        assert_eq!(path.id, "abc"); // Deref
        let inner = path.into_inner();
        assert_eq!(inner.id, "abc");
    }

    #[test]
    fn validated_path_deref_mut() {
        let mut path = ValidatedPath(ValidatedPathParams { id: "old".into() });
        path.id = "new".into(); // DerefMut
        assert_eq!(path.id, "new");
    }

    #[test]
    fn validated_form_into_inner() {
        let form = ValidatedForm(ValidatedFormData {
            username: "alice".into(),
        });
        assert_eq!(form.username, "alice"); // Deref
        let inner = form.into_inner();
        assert_eq!(inner.username, "alice");
    }

    #[test]
    fn validated_form_deref_mut() {
        let mut form = ValidatedForm(ValidatedFormData {
            username: "old".into(),
        });
        form.username = "new".into(); // DerefMut
        assert_eq!(form.username, "new");
    }

    // Host extractor tests
    #[test]
    fn host_extractor_uses_host_header() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("example.com"));
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(Host::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "example.com");
    }

    #[test]
    fn host_extractor_ignores_x_forwarded_host() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("internal.local"));
        request
            .headers_mut()
            .insert("x-forwarded-host", HeaderValue::from_static("example.com"));
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(Host::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "internal.local");
    }

    #[test]
    fn host_extractor_uses_default_when_no_headers() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(Host::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "localhost");
    }

    #[test]
    fn host_deref_and_into_inner() {
        let host = Host("example.com".to_owned());
        assert_eq!(&*host, "example.com"); // Deref
        let inner = host.into_inner();
        assert_eq!(inner, "example.com");
    }

    // ForwardedHost extractor tests
    #[test]
    fn forwarded_host_extractor_uses_x_forwarded_host_first() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("internal.local"));
        request
            .headers_mut()
            .insert("x-forwarded-host", HeaderValue::from_static("example.com"));
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(ForwardedHost::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "example.com");
    }

    #[test]
    fn forwarded_host_extractor_falls_back_to_host_header() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        request
            .headers_mut()
            .insert("host", HeaderValue::from_static("example.com"));
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(ForwardedHost::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "example.com");
    }

    #[test]
    fn forwarded_host_extractor_uses_default_when_no_headers() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let host = block_on(ForwardedHost::from_request(&ctx)).expect("host");
        assert_eq!(host.0, "localhost");
    }

    #[test]
    fn forwarded_host_deref_and_into_inner() {
        let host = ForwardedHost("example.com".to_owned());
        assert_eq!(&*host, "example.com"); // Deref
        let inner = host.into_inner();
        assert_eq!(inner, "example.com");
    }

    // -- Kv / Secrets / Config extractors (registry-aware) -----------------

    #[test]
    fn kv_extractor_errors_when_only_legacy_handle_wired() {
        // Hard-cutoff: the extractor used to synthesise
        // a one-id registry from a lone `ctx.kv_handle()` when no
        // `KvRegistry` was in extensions. That path silently
        // masked missing registry wiring, which violates the
        // spec's "no backward compatibility" promise for the
        // runtime store API. Adapter dispatchers (axum /
        // cloudflare / fastly / spin) now normalise legacy bare-
        // handle inputs to a single-id `KvRegistry` at the
        // dispatch boundary, so this code path only fires when a
        // test or callsite bypasses a dispatcher. In that case
        // the extractor must surface the wiring bug.
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(KvHandle::new(Arc::new(NoopKvStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Kv::from_request(&ctx))
            .expect_err("extractor must surface missing-registry as an error, not auto-upgrade");
        assert!(
            err.message().contains("no kv store configured"),
            "error names the wiring gap: {err:?}"
        );
    }

    #[test]
    fn kv_extractor_prefers_registry_over_legacy_handle() {
        use crate::key_value_store::{KvHandle, NoopKvStore};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let registry: KvRegistry = StoreRegistry::new(
            [
                ("sessions".to_owned(), KvHandle::new(Arc::new(NoopKvStore))),
                ("cache".to_owned(), KvHandle::new(Arc::new(NoopKvStore))),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "sessions".to_owned(),
        );

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let kv = block_on(Kv::from_request(&ctx)).expect("Kv extractor when registry present");
        assert!(kv.named("sessions").is_some());
        assert!(kv.named("cache").is_some());
        assert!(kv.named("unknown").is_none());
        assert_eq!(kv.registry().default_id(), "sessions");
    }

    #[test]
    fn kv_extractor_returns_error_when_not_configured() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Kv::from_request(&ctx)).expect_err("expected error");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message().contains("check [stores.kv]"));
    }

    #[test]
    fn secrets_extractor_errors_when_only_legacy_handle_wired() {
        // Hard-cutoff — same semantics as
        // `kv_extractor_errors_when_only_legacy_handle_wired`.
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use std::sync::Arc;

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(SecretHandle::new(Arc::new(NoopSecretStore)));
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Secrets::from_request(&ctx))
            .expect_err("extractor must surface missing-registry as an error");
        assert!(
            err.message().contains("no secret store configured"),
            "error names the wiring gap: {err:?}"
        );
    }

    #[test]
    fn secrets_extractor_preserves_registry_per_id_platform_name() {
        use crate::secret_store::{NoopSecretStore, SecretHandle};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let handle = SecretHandle::new(Arc::new(NoopSecretStore));
        let by_id: BTreeMap<String, BoundSecretStore> = [
            (
                "primary".to_owned(),
                BoundSecretStore::new(handle.clone(), "primary-vault".to_owned()),
            ),
            (
                "analytics".to_owned(),
                BoundSecretStore::new(handle, "analytics-vault".to_owned()),
            ),
        ]
        .into_iter()
        .collect();
        let registry: SecretRegistry = StoreRegistry::new(by_id, "primary".to_owned());

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        let ctx = RequestContext::new(request, PathParams::default());

        let secrets =
            block_on(Secrets::from_request(&ctx)).expect("Secrets extractor when registry present");
        // The per-id binding survives the extractor — each named store
        // resolves to its own platform name.
        assert_eq!(
            secrets.named("primary").expect("primary").store_name(),
            "primary-vault"
        );
        assert_eq!(
            secrets.named("analytics").expect("analytics").store_name(),
            "analytics-vault"
        );
        assert_eq!(
            secrets.default().expect("default").store_name(),
            "primary-vault"
        );
        assert!(secrets.named("missing").is_none());
    }

    #[test]
    fn secrets_extractor_errors_when_absent() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/secrets")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Secrets::from_request(&ctx)).unwrap_err();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_extractor_resolves_from_registry() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::ConfigStoreBinding;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct FixedStore(&'static str);
        #[async_trait(?Send)]
        impl ConfigStore for FixedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.to_owned()))
            }
        }

        let registry: ConfigRegistry = StoreRegistry::new(
            [
                (
                    "primary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(FixedStore("primary"))),
                        default_key: "primary".to_owned(),
                    },
                ),
                (
                    "analytics".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(FixedStore("analytics"))),
                        default_key: "analytics".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "primary".to_owned(),
        );

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let config =
            block_on(Config::from_request(&ctx)).expect("Config extractor when registry present");
        let analytics = config.named("analytics").expect("analytics handle");
        assert_eq!(
            block_on(analytics.get("any")).expect("config value"),
            Some("analytics".to_owned())
        );
        assert!(config.named("missing").is_none());
        assert!(config.default().is_some());
    }

    #[test]
    fn config_extractor_errors_when_only_legacy_handle_wired() {
        // Hard-cutoff — same semantics as
        // `kv_extractor_errors_when_only_legacy_handle_wired`.
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some("legacy".to_owned()))
            }
        }

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ConfigStoreHandle::new(Arc::new(AnyStore)));
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Config::from_request(&ctx))
            .expect_err("extractor must surface missing-registry as an error");
        assert!(
            err.message().contains("no config store configured"),
            "error names the wiring gap: {err:?}"
        );
    }

    #[test]
    fn config_extractor_errors_when_absent() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(Config::from_request(&ctx)).expect_err("expected error");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message().contains("check [stores.config]"));
    }

    // -- Config::default_binding / named_binding (B8) ----------------------

    #[test]
    fn config_default_binding_returns_resolved_key() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::ConfigStoreBinding;
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
            default_key: "app_config_staging".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("app_config".to_owned(), binding);

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let config =
            block_on(Config::from_request(&ctx)).expect("Config extractor when registry present");

        let def_binding = config.default_binding().expect("default binding");
        assert_eq!(def_binding.default_key, "app_config_staging");
    }

    #[test]
    fn config_default_binding_returns_none_when_not_configured() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        // extractor itself errors when no registry — confirm it does not panic
        let result = block_on(Config::from_request(&ctx));
        assert!(result.is_err(), "no registry -- extractor must error");
    }

    #[test]
    fn config_named_binding_returns_binding_for_declared_id() {
        use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
        use crate::store_registry::ConfigStoreBinding;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct AnyStore;
        #[async_trait(?Send)]
        impl ConfigStore for AnyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let registry: ConfigRegistry = StoreRegistry::new(
            [
                (
                    "primary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "primary_key".to_owned(),
                    },
                ),
                (
                    "secondary".to_owned(),
                    ConfigStoreBinding {
                        handle: ConfigStoreHandle::new(Arc::new(AnyStore)),
                        default_key: "secondary_key".to_owned(),
                    },
                ),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "primary".to_owned(),
        );

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/config")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);

        let ctx = RequestContext::new(request, PathParams::default());
        let config =
            block_on(Config::from_request(&ctx)).expect("Config extractor when registry present");

        let sec = config
            .named_binding("secondary")
            .expect("secondary binding");
        assert_eq!(sec.default_key, "secondary_key");

        assert!(
            config.named_binding("undeclared").is_none(),
            "unknown id must yield None"
        );
    }

    // -- AppConfig<C> extractor tests ----------------------------------------

    // Build a RequestContext with a ConfigRegistry wired to `store`.
    fn ctx_with_config_store<S: ConfigStore + 'static>(
        store: S,
        default_key: &str,
    ) -> RequestContext {
        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(store)),
            default_key: default_key.to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::single_id("default".to_owned(), binding);
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        RequestContext::new(request, PathParams::default())
    }

    // Build a RequestContext with a ConfigRegistry AND a SecretRegistry.
    fn ctx_with_config_and_secrets<CS: ConfigStore + 'static, SS: SecretStore + 'static>(
        config_store: CS,
        default_key: &str,
        secret_store: SS,
        secret_store_name: &str,
    ) -> RequestContext {
        let binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(config_store)),
            default_key: default_key.to_owned(),
        };
        let config_registry: ConfigRegistry =
            StoreRegistry::single_id("default".to_owned(), binding);
        let secret_handle = SecretHandle::new(Arc::new(secret_store));
        let bound_secret = BoundSecretStore::new(secret_handle, secret_store_name.to_owned());
        let secret_registry: SecretRegistry =
            StoreRegistry::single_id("default".to_owned(), bound_secret);
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(config_registry);
        request.extensions_mut().insert(secret_registry);
        RequestContext::new(request, PathParams::default())
    }

    // Helper: build a valid BlobEnvelope JSON string wrapping `data`.
    fn make_envelope(data: serde_json::Value) -> String {
        let envelope = BlobEnvelope::new(data, "2026-01-01T00:00:00Z".into());
        serde_json::to_string(&envelope).expect("serialise envelope")
    }

    async fn test_secret_walk<C>(
        ctx: &RequestContext,
        data: &mut serde_json::Value,
    ) -> Result<(), EdgeError>
    where
        C: AppConfigMeta,
    {
        let mut budget =
            ConfigExtractionBudget::start(ConfigExtractionLimits::default()).expect("test budget");
        secret_walk::<C>(ctx, &mut budget, data).await
    }

    #[test]
    fn app_config_extractor_happy_path() {
        struct FixedStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for FixedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        let data = serde_json::json!({ "greeting": "hello", "timeout_ms": 500_u32 });
        let blob = make_envelope(data);
        let ctx = ctx_with_config_store(FixedStore(blob), "the_key");
        let AppConfig(cfg) =
            block_on(AppConfig::<FixtureCfg>::from_request(&ctx)).expect("happy path");
        assert_eq!(cfg.greeting, "hello");
        assert_eq!(cfg.timeout_ms, 500);
    }

    #[test]
    fn app_config_extractor_uses_bounded_root_read() {
        use std::sync::Mutex;

        use crate::config_store::{BoundedStoreRead, ConfigExtractionLimits};
        use crate::time::Deadline;

        struct BoundedOnlyStore {
            blob: String,
            observed: Arc<Mutex<Option<(Deadline, u64, u64)>>>,
        }

        #[async_trait(?Send)]
        impl ConfigStore for BoundedOnlyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                panic!("typed extraction must not call the unbounded config-store API");
            }

            async fn get_bounded(
                &self,
                _key: &str,
                deadline: Deadline,
                max_backend_bytes: u64,
                max_value_bytes: u64,
            ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
                *self.observed.lock().expect("record bounded read") =
                    Some((deadline, max_backend_bytes, max_value_bytes));
                Ok(BoundedStoreRead {
                    backend_bytes: u64::try_from(self.blob.len()).expect("fixture length"),
                    value: Some(self.blob.clone()),
                })
            }
        }

        let observed = Arc::new(Mutex::new(None));
        let blob =
            make_envelope(serde_json::json!({ "greeting": "bounded", "timeout_ms": 500_u32 }));
        let ctx = ctx_with_config_store(
            BoundedOnlyStore {
                blob,
                observed: Arc::clone(&observed),
            },
            "the_key",
        );

        let AppConfig(cfg) =
            block_on(AppConfig::<FixtureCfg>::from_request(&ctx)).expect("bounded extraction");
        assert_eq!(cfg.greeting, "bounded");

        let (_, max_backend_bytes, max_value_bytes) = observed
            .lock()
            .expect("read observation")
            .expect("bounded read called");
        let limits = ConfigExtractionLimits::default();
        assert_eq!(max_backend_bytes, limits.max_backend_bytes);
        assert_eq!(max_value_bytes, limits.max_blob_bytes);
    }

    #[test]
    fn app_config_extractor_shares_deadline_and_budget_with_secret_reads() {
        use std::sync::Mutex;

        use crate::config_store::BoundedStoreRead;
        use crate::time::{Deadline, MonotonicInstant};
        use bytes::Bytes;

        type Observation = (MonotonicInstant, u64, u64);

        struct RecordingConfigStore {
            blob: String,
            observed: Arc<Mutex<Vec<Observation>>>,
        }

        #[async_trait(?Send)]
        impl ConfigStore for RecordingConfigStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                panic!("typed extraction must not call unbounded config reads");
            }

            async fn get_bounded(
                &self,
                _key: &str,
                deadline: Deadline,
                max_backend_bytes: u64,
                max_value_bytes: u64,
            ) -> Result<BoundedStoreRead<String>, ConfigStoreError> {
                self.observed.lock().expect("config observation").push((
                    deadline.instant(),
                    max_backend_bytes,
                    max_value_bytes,
                ));
                Ok(BoundedStoreRead {
                    backend_bytes: u64::try_from(self.blob.len()).expect("fixture length"),
                    value: Some(self.blob.clone()),
                })
            }
        }

        struct RecordingSecretStore {
            observed: Arc<Mutex<Vec<Observation>>>,
        }

        #[async_trait(?Send)]
        impl SecretStore for RecordingSecretStore {
            async fn get_bytes(
                &self,
                _store_name: &str,
                _key: &str,
            ) -> Result<Option<Bytes>, SecretError> {
                panic!("typed extraction must not call unbounded secret reads");
            }

            async fn get_bytes_bounded(
                &self,
                _store_name: &str,
                _key: &str,
                deadline: Deadline,
                max_backend_bytes: u64,
                max_value_bytes: u64,
            ) -> Result<BoundedStoreRead<Bytes>, SecretError> {
                self.observed.lock().expect("secret observation").push((
                    deadline.instant(),
                    max_backend_bytes,
                    max_value_bytes,
                ));
                Ok(BoundedStoreRead {
                    backend_bytes: 6,
                    value: Some(Bytes::from_static(b"secret")),
                })
            }
        }

        let observed = Arc::new(Mutex::new(Vec::new()));
        let blob =
            make_envelope(serde_json::json!({ "greeting": "bounded", "api_token": "token-key" }));
        let blob_bytes = u64::try_from(blob.len()).expect("fixture length");
        let ctx = ctx_with_config_and_secrets(
            RecordingConfigStore {
                blob,
                observed: Arc::clone(&observed),
            },
            "the_key",
            RecordingSecretStore {
                observed: Arc::clone(&observed),
            },
            "vault",
        );

        let AppConfig(cfg) =
            block_on(AppConfig::<SecretCfg>::from_request(&ctx)).expect("bounded extraction");
        assert_eq!(cfg.api_token, "secret");

        let observed = observed.lock().expect("observations");
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0, observed[1].0, "deadline must not reset");
        assert_eq!(
            observed[1].1,
            observed[0].1 - blob_bytes,
            "secret read receives the remaining backend allowance"
        );
        assert_eq!(
            observed[1].2,
            ConfigExtractionLimits::default().max_secret_bytes
        );
    }

    #[test]
    fn app_config_extractor_returns_typed_missing_blob() {
        struct EmptyStore;
        #[async_trait(?Send)]
        impl ConfigStore for EmptyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(None)
            }
        }

        let ctx = ctx_with_config_store(EmptyStore, "the_key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("missing blob must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::MissingBlob)
        );
        assert!(
            err.message().contains("missing typed app-config blob"),
            "message names the gap: {err:?}"
        );
        assert!(
            err.message().contains("run `<app-cli> config push`"),
            "message names the remediation: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_maps_config_store_unavailable() {
        struct DownStore;
        #[async_trait(?Send)]
        impl ConfigStore for DownStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Err(ConfigStoreError::unavailable("backend offline"))
            }
        }

        let ctx = ctx_with_config_store(DownStore, "the_key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("unavailable store must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::BackendUnavailable)
        );
    }

    #[test]
    fn app_config_extractor_maps_config_store_invalid_key() {
        struct BadKeyStore;
        #[async_trait(?Send)]
        impl ConfigStore for BadKeyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Err(ConfigStoreError::invalid_key("key is malformed"))
            }
        }

        let ctx = ctx_with_config_store(BadKeyStore, "the_key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("invalid key must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::InvalidKey)
        );
    }

    #[test]
    fn app_config_extractor_maps_config_store_internal() {
        struct BrokenStore;
        #[async_trait(?Send)]
        impl ConfigStore for BrokenStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Err(ConfigStoreError::internal(anyhow::anyhow!("disk on fire")))
            }
        }

        let ctx = ctx_with_config_store(BrokenStore, "the_key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("internal store error must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::BackendFailure)
        );
    }

    #[test]
    fn app_config_extractor_returns_integrity_mismatch_on_sha_mismatch() {
        const SENTINEL: &str = "SUPER_SECRET_STORED_HASH";
        struct TamperedStore;
        #[async_trait(?Send)]
        impl ConfigStore for TamperedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                // A valid envelope whose stored sha is a SENTINEL secret. The
                // stored hash is attacker-influenced (it comes from the config
                // store), and this error becomes the HTTP 500 body — so the
                // message must NOT echo it.
                let mut env = BlobEnvelope::new(
                    serde_json::json!({ "greeting": "hi", "timeout_ms": 100_u32 }),
                    "2026-01-01T00:00:00Z".into(),
                );
                env.sha256 = SENTINEL.to_owned();
                Ok(Some(serde_json::to_string(&env).unwrap()))
            }
        }

        let ctx = ctx_with_config_store(TamperedStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("sha mismatch must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::IntegrityMismatch)
        );
        // The client-facing message (the HTTP body) must not carry the stored
        // hash, only a redacted category.
        assert!(
            !err.message().contains(SENTINEL),
            "the stored hash must never reach the client-facing message: {err:?}"
        );
        assert!(
            err.message().contains("integrity check"),
            "message must still name the category: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_asks_to_redeploy_on_a_future_envelope_version() {
        // The store returns a NEWER envelope version VERBATIM (its contract). The
        // typed app-config layer -- NOT the store -- must give the upgrade/redeploy
        // remediation, distinct from the corruption "re-push" path, because a
        // build older than its config cannot be fixed by re-pushing the config.
        struct FutureVersionStore;
        #[async_trait(?Send)]
        impl ConfigStore for FutureVersionStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                // A v1-shaped envelope with the version bumped to 2.
                let env = BlobEnvelope::new(
                    serde_json::json!({ "greeting": "hi", "timeout_ms": 100_u32 }),
                    "2026-01-01T00:00:00Z".into(),
                );
                let mut value: serde_json::Value =
                    serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
                value["version"] = serde_json::json!(2_u32);
                Ok(Some(value.to_string()))
            }
        }

        let ctx = ctx_with_config_store(FutureVersionStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("a future envelope version must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::InvalidEnvelope)
        );
        let message = err.message().to_lowercase();
        assert!(
            message.contains("redeploy") && message.contains("version 2"),
            "must ask to redeploy an updated build and name the version: {err:?}"
        );
        assert!(
            !message.contains("re-run config push") && !message.contains("push to repair"),
            "must NOT tell the operator to re-push a future format: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_asks_to_redeploy_on_an_unknown_edgezero_kind() {
        // A v1-SHAPED envelope carrying an `edgezero_kind` discriminator. serde
        // ignores the unknown field, so without the reason check the extractor
        // would deserialize and APPLY it. It must instead ask to redeploy, matching
        // the generic push's refusal of the same value.
        struct KindTaggedStore;
        #[async_trait(?Send)]
        impl ConfigStore for KindTaggedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                let env = BlobEnvelope::new(
                    serde_json::json!({ "greeting": "hi", "timeout_ms": 100_u32 }),
                    "2026-01-01T00:00:00Z".into(),
                );
                let mut value: serde_json::Value =
                    serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
                value["edgezero_kind"] = serde_json::json!("new_format");
                Ok(Some(value.to_string()))
            }
        }

        let ctx = ctx_with_config_store(KindTaggedStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("an unknown edgezero_kind must error");
        let message = err.message().to_lowercase();
        assert!(
            err.store_extraction_reason() == Some(StoreExtractionReason::InvalidEnvelope)
                && message.contains("redeploy")
                && message.contains("edgezero_kind"),
            "an unknown discriminator must ask to redeploy: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_does_not_leak_data_value_in_deserialize_error() {
        const SENTINEL: &str = "SUPER_SECRET_FIELD_VALUE";
        struct TypeErrorStore;
        #[async_trait(?Send)]
        impl ConfigStore for TypeErrorStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                // A VALID envelope (verify passes) whose data has the wrong type
                // for `timeout_ms` — a string sentinel where u32 is expected.
                // The deserialize error names that value; it must not reach the
                // client-facing message.
                let env = BlobEnvelope::new(
                    serde_json::json!({ "greeting": "hi", "timeout_ms": SENTINEL }),
                    "2026-01-01T00:00:00Z".into(),
                );
                Ok(Some(serde_json::to_string(&env).unwrap()))
            }
        }

        let ctx = ctx_with_config_store(TypeErrorStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("a type mismatch in the typed config must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::SchemaMismatch)
        );
        assert!(
            !err.message().contains(SENTINEL),
            "the stored field value must never reach the client-facing message: {err:?}"
        );
        // The field path segments are redacted (a map key is indistinguishable
        // from a struct field and may be a secret); structure is kept.
        if let EdgeError::StoreExtraction { field_path, .. } = &err {
            assert!(
                field_path.as_deref().is_some_and(|path| {
                    !path.contains("timeout_ms") && path.contains("<redacted>")
                }),
                "the field path must be redacted: {field_path:?}"
            );
        }
    }

    #[test]
    fn app_config_extractor_returns_invalid_envelope_on_bad_json() {
        struct GarbageStore;
        #[async_trait(?Send)]
        impl ConfigStore for GarbageStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some("not-json-at-all".to_owned()))
            }
        }

        let ctx = ctx_with_config_store(GarbageStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("bad envelope JSON must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::InvalidEnvelope)
        );
        assert!(
            err.message().contains("not a valid envelope"),
            "message names the problem: {err:?}"
        );
        // The offending input is not echoed (a serde error would embed it, and a
        // stored value may hold secrets).
        assert!(
            !err.message().contains("not-json-at-all"),
            "the offending stored value must not reach the client message: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_returns_schema_mismatch_on_deserialise_failure() {
        use crate::config_store::{ConfigStore, ConfigStoreError};

        // Blob has wrong type for `timeout_ms` (string instead of u32).
        struct BadDataStore;
        #[async_trait(?Send)]
        impl ConfigStore for BadDataStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                let data = serde_json::json!({
                    "greeting": "hi",
                    "timeout_ms": "not-a-number",
                });
                Ok(Some(make_envelope(data)))
            }
        }

        let ctx = ctx_with_config_store(BadDataStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("type mismatch must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::SchemaMismatch)
        );
        // The field path segment is redacted (indistinguishable from a map key).
        if let EdgeError::StoreExtraction { field_path, .. } = &err {
            assert_eq!(
                field_path.as_deref(),
                Some("<redacted>"),
                "the field path must be redacted: {err:?}"
            );
        }
    }

    #[test]
    fn app_config_extractor_returns_schema_mismatch_on_validation_failure() {
        use crate::config_store::{ConfigStore, ConfigStoreError};

        // `timeout_ms = 0` violates `range(min = 1)`.
        struct ZeroTimeoutStore;
        #[async_trait(?Send)]
        impl ConfigStore for ZeroTimeoutStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                let data = serde_json::json!({ "greeting": "hi", "timeout_ms": 0_u32 });
                Ok(Some(make_envelope(data)))
            }
        }

        let ctx = ctx_with_config_store(ZeroTimeoutStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("validation failure must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::SchemaMismatch)
        );
        if let EdgeError::StoreExtraction { field_path, .. } = &err {
            assert_eq!(
                field_path.as_deref(),
                Some("timeout_ms"),
                "field_path names the violator: {err:?}"
            );
        }
    }

    #[test]
    fn app_config_secret_walk_resolves_key_in_default_store() {
        use crate::config_store::{ConfigStore, ConfigStoreError};
        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // Blob has `api_token = "my_key_name"` (a key name, not the secret).
        let data = serde_json::json!({ "greeting": "hi", "api_token": "my_key_name" });
        let blob = make_envelope(data);

        // Secret store has "my_key_name" → "s3cr3t".
        let secret_store =
            InMemorySecretStore::new([("vault/my_key_name", bytes::Bytes::from("s3cr3t"))]);

        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", secret_store, "vault");
        let AppConfig(cfg) =
            block_on(AppConfig::<SecretCfg>::from_request(&ctx)).expect("secret walk");
        assert_eq!(
            cfg.api_token, "s3cr3t",
            "api_token must hold the RESOLVED secret"
        );
    }

    #[test]
    fn app_config_secret_walk_missing_key_has_typed_reason() {
        use crate::config_store::{ConfigStore, ConfigStoreError};
        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // Blob references a key that doesn't exist in the secret store.
        let data = serde_json::json!({ "greeting": "hi", "api_token": "missing_key" });
        let blob = make_envelope(data);
        // NoopSecretStore returns None for everything.
        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", NoopSecretStore, "vault");
        let err = block_on(AppConfig::<SecretCfg>::from_request(&ctx))
            .expect_err("missing secret must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::MissingSecret)
        );
        if let EdgeError::StoreExtraction { field_path, .. } = &err {
            assert_eq!(
                field_path.as_deref(),
                Some("api_token"),
                "field_path names the secret field: {err:?}"
            );
        }
    }

    #[test]
    fn app_config_secret_walk_error_does_not_leak_the_stored_key_name() {
        use crate::config_store::{ConfigStore, ConfigStoreError};
        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // The blob's secret field holds the secret's KEY NAME — config data that
        // may be sensitive. When resolution fails, that name (and any store id /
        // provider message) must not reach the error, which becomes the HTTP body.
        const SENTINEL: &str = "SUPER_SECRET_KEY_NAME";
        let data = serde_json::json!({ "greeting": "hi", "api_token": SENTINEL });
        let blob = make_envelope(data);
        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", NoopSecretStore, "vault");
        let err = block_on(AppConfig::<SecretCfg>::from_request(&ctx))
            .expect_err("missing secret must error");
        assert!(
            !err.message().contains(SENTINEL),
            "the stored secret key name must never reach the error message: {err:?}"
        );
    }

    // Build a RequestContext whose default secret store maps `default/{key}` ->
    // `value`, for exercising `secret_walk` directly.
    fn ctx_with_default_secret_store(key: &str, value: &str) -> RequestContext {
        ctx_with_default_secret_store_map(&[(key, value)])
    }

    // Multi-entry variant of `ctx_with_default_secret_store`.
    fn ctx_with_default_secret_store_map(entries: &[(&str, &str)]) -> RequestContext {
        let store = InMemorySecretStore::new(entries.iter().map(|(key, value)| {
            (
                format!("default/{key}"),
                bytes::Bytes::from((*value).to_owned()),
            )
        }));
        let bound = BoundSecretStore::new(SecretHandle::new(Arc::new(store)), "default".to_owned());
        let registry: SecretRegistry = StoreRegistry::single_id("default".to_owned(), bound);
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        RequestContext::new(request, PathParams::default())
    }

    // Build a RequestContext whose secret store `store_id` maps
    // `{store_id}/{key}` -> `value`, resolvable via `ctx.secret_store(store_id)`.
    fn ctx_with_named_secret_store(store_id: &str, key: &str, value: &str) -> RequestContext {
        let store = InMemorySecretStore::new([(
            format!("{store_id}/{key}"),
            bytes::Bytes::from(value.to_owned()),
        )]);
        let bound = BoundSecretStore::new(SecretHandle::new(Arc::new(store)), store_id.to_owned());
        let registry: SecretRegistry = StoreRegistry::single_id(store_id.to_owned(), bound);
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        RequestContext::new(request, PathParams::default())
    }

    #[test]
    fn secret_walk_resolves_nested_object_leaf() {
        let ctx = ctx_with_default_secret_store("dd_key", "resolved-dd");
        let mut data = serde_json::json!({
            "integrations": { "datadome": { "server_side_key": "dd_key" } }
        });
        block_on(test_secret_walk::<NestedCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(
            data["integrations"]["datadome"]["server_side_key"],
            serde_json::json!("resolved-dd")
        );
    }

    #[test]
    fn secret_walk_resolves_each_array_element() {
        let ctx = ctx_with_default_secret_store_map(&[("k0", "v0"), ("k1", "v1")]);
        let mut data = serde_json::json!({
            "partners": [ { "api_key": "k0" }, { "api_key": "k1" } ]
        });
        block_on(test_secret_walk::<ArrayCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(data["partners"][0]["api_key"], serde_json::json!("v0"));
        assert_eq!(data["partners"][1]["api_key"], serde_json::json!("v1"));
    }

    #[test]
    fn secret_walk_resolves_nested_named_store_via_sibling_in_parent() {
        let ctx = ctx_with_named_secret_store("named", "tok_key", "TOK");
        let mut data = serde_json::json!({
            "vaulted": { "token": "tok_key", "vault": "named" }
        });
        block_on(test_secret_walk::<NamedStoreCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(data["vaulted"]["token"], serde_json::json!("TOK"));
        // The store_ref sibling is left intact (it names a store, not a secret).
        assert_eq!(data["vaulted"]["vault"], serde_json::json!("named"));
    }

    #[test]
    fn secret_walk_nested_named_store_missing_sibling_errors_with_dotted_path() {
        let ctx = ctx_with_named_secret_store("named", "tok_key", "TOK");
        let mut data = serde_json::json!({ "vaulted": { "token": "tok_key" } }); // no `vault`
        let err = block_on(test_secret_walk::<NamedStoreCfg>(&ctx, &mut data))
            .expect_err("missing store_ref sibling");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.to_string().contains("vaulted.token"));
    }

    #[test]
    fn secret_walk_skips_absent_optional_leaf() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "greeting": "hi" }); // no maybe_key
        block_on(test_secret_walk::<OptionalCfg>(&ctx, &mut data))
            .expect("absent optional is fine");
        assert!(data.get("maybe_key").is_none());
    }

    #[test]
    fn secret_walk_skips_null_optional_leaf() {
        // serde serializes `Option::None` as JSON `null` (the key is present,
        // not omitted). The walk must skip a null optional leaf, not error it.
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "maybe_key": null });
        block_on(test_secret_walk::<OptionalCfg>(&ctx, &mut data))
            .expect("null optional is skipped, not treated as non-string");
        assert_eq!(data["maybe_key"], serde_json::json!(null)); // left untouched
    }

    #[test]
    fn secret_walk_missing_required_nested_leaf_errors_with_dotted_path() {
        let ctx = ctx_with_default_secret_store("dd_key", "resolved-dd");
        let mut data = serde_json::json!({ "integrations": { "datadome": {} } });
        let err = block_on(test_secret_walk::<NestedCfg>(&ctx, &mut data))
            .expect_err("missing required nested leaf");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.to_string()
                .contains("integrations.datadome.server_side_key")
        );
    }

    #[test]
    fn secret_walk_missing_required_intermediate_errors_even_when_leaf_optional() {
        // `optional` reflects only the LEAF (`Option<String>`). A missing
        // INTERMEDIATE (the whole `integrations` subtree) is a stale blob and
        // must error with the dotted path — NOT pass silently and degrade to a
        // vaguer serde error downstream.
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "greeting": "hi" }); // no `integrations`
        let err = block_on(test_secret_walk::<OptionalNestedCfg>(&ctx, &mut data))
            .expect_err("missing required intermediate");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.to_string().contains("integrations"),
            "error names the missing intermediate: {err}"
        );
    }

    #[test]
    fn secret_walk_skips_absent_optional_intermediate() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "integrations": {} });
        block_on(test_secret_walk::<OptionalNestedCfg>(&ctx, &mut data))
            .expect("absent optional intermediate is fine");
    }

    #[test]
    fn secret_walk_skips_null_optional_intermediate() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "integrations": { "datadome": null } });
        block_on(test_secret_walk::<OptionalNestedCfg>(&ctx, &mut data))
            .expect("null optional intermediate is fine");
    }

    #[test]
    fn secret_walk_rejects_scalar_parent_of_optional_intermediate() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "integrations": "not-an-object" });
        let err = block_on(test_secret_walk::<OptionalNestedCfg>(&ctx, &mut data))
            .expect_err("a present optional intermediate must have an object parent");

        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.to_string().contains("integrations"),
            "error names the malformed parent: {err}"
        );
        let EdgeError::StoreExtraction {
            reason, field_path, ..
        } = &err
        else {
            panic!("malformed optional parent must be typed: {err:?}");
        };
        assert_eq!(*reason, StoreExtractionReason::SchemaMismatch);
        assert_eq!(field_path.as_deref(), Some("integrations"));
    }

    #[test]
    fn secret_walk_rejects_scalar_parent_of_terminal_optional_field() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "integrations": "not-an-object" });
        let err = block_on(test_secret_walk::<TerminalOptionalCfg>(&ctx, &mut data))
            .expect_err("a terminal optional field must have an object parent");

        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        let EdgeError::StoreExtraction {
            reason, field_path, ..
        } = &err
        else {
            panic!("malformed optional parent must be typed: {err:?}");
        };
        assert_eq!(*reason, StoreExtractionReason::SchemaMismatch);
        assert_eq!(field_path.as_deref(), Some("integrations.webhook_key"));
    }

    #[test]
    fn secret_walk_present_intermediate_absent_optional_leaf_is_ok() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "integrations": { "datadome": {} } });
        block_on(test_secret_walk::<OptionalNestedCfg>(&ctx, &mut data))
            .expect("absent optional leaf under present intermediates is fine");
    }

    #[test]
    fn app_config_named_reads_different_key() {
        struct KeyEchoStore;
        #[async_trait(?Send)]
        impl ConfigStore for KeyEchoStore {
            async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
                // Return a blob whose `greeting` equals the key being looked up.
                let data = serde_json::json!({ "greeting": key, "timeout_ms": 200_u32 });
                Ok(Some(make_envelope(data)))
            }
        }

        let ctx = ctx_with_config_store(KeyEchoStore, "default_key");
        // `named` should use "custom_key", not the binding's default_key.
        let cfg =
            block_on(AppConfig::<FixtureCfg>::named(&ctx, "custom_key")).expect("named succeeds");
        assert_eq!(cfg.greeting, "custom_key", "`named` used the explicit key");
    }

    #[test]
    fn app_config_from_store_reads_non_default_store() {
        use std::collections::BTreeMap;

        struct NamedStore(&'static str);
        #[async_trait(?Send)]
        impl ConfigStore for NamedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                let data = serde_json::json!({ "greeting": self.0, "timeout_ms": 300_u32 });
                Ok(Some(make_envelope(data)))
            }
        }

        // Wire two config stores: "primary" (default) and "secondary".
        let primary_binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(NamedStore("from-primary"))),
            default_key: "pk".to_owned(),
        };
        let secondary_binding = ConfigStoreBinding {
            handle: ConfigStoreHandle::new(Arc::new(NamedStore("from-secondary"))),
            default_key: "sk".to_owned(),
        };
        let registry: ConfigRegistry = StoreRegistry::new(
            [
                ("primary".to_owned(), primary_binding),
                ("secondary".to_owned(), secondary_binding),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "primary".to_owned(),
        );
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(registry);
        let ctx = RequestContext::new(request, PathParams::default());

        // from_store with store_id = "secondary", key = None (uses binding's default_key).
        let cfg = block_on(AppConfig::<FixtureCfg>::from_store(&ctx, "secondary", None))
            .expect("from_store secondary");
        assert_eq!(cfg.greeting, "from-secondary");
    }

    #[test]
    fn app_config_no_registry_returns_typed_error() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("no registry must error");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::MissingRegistry)
        );
        assert!(
            err.message().contains("no default config store registered"),
            "message names the gap: {err:?}"
        );
    }

    // -- Runtime validation of resolved secret values (spec 3.3.8) -----------

    /// Spec 3.3.8: RUNTIME runs `cfg.validate()` after `secret_walk` so that
    /// validators on secret fields run against the RESOLVED value, not the
    /// key name. A key name that is too short must still pass push (validator
    /// skipped), but a resolved secret that satisfies the rule must pass here.
    #[test]
    fn runtime_validates_resolved_secret_value_passes_when_long_enough() {
        use crate::config_store::{ConfigStore, ConfigStoreError};

        // A struct whose secret field has a `length(min = 10)` rule.
        #[derive(Debug, Deserialize, Validate)]
        struct SecretLen {
            #[validate(length(min = 10_u64))]
            api_token: String,
        }

        impl AppConfigMeta for SecretLen {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }

        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // Blob carries the key name "short" (5 chars) — push skipped the
        // validator. At runtime the secret store resolves it to a 32-char token.
        let data = serde_json::json!({ "api_token": "short" });
        let blob = make_envelope(data);
        let resolved_value = "a-real-secret-longer-than-thirty-two-chars".to_owned();
        let secret_store =
            InMemorySecretStore::new([("vault/short", bytes::Bytes::from(resolved_value.clone()))]);
        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", secret_store, "vault");
        let AppConfig(cfg) = block_on(AppConfig::<SecretLen>::from_request(&ctx))
            .expect("long resolved secret passes");
        assert_eq!(cfg.api_token, resolved_value);
    }

    /// Spec 3.3.8: a resolved secret that FAILS a validator on the secret
    /// field must produce a typed schema mismatch; runtime runs the full validator.
    #[test]
    fn runtime_rejects_resolved_secret_failing_validator() {
        use crate::config_store::{ConfigStore, ConfigStoreError};

        // Same struct: `length(min = 10)` on `api_token`.
        #[derive(Debug, Deserialize, Validate)]
        struct SecretLen {
            #[validate(length(min = 10_u64))]
            api_token: String,
        }

        impl AppConfigMeta for SecretLen {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }

        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // The secret store resolves the key to a 5-char value — too short.
        let data = serde_json::json!({ "api_token": "mykey" });
        let blob = make_envelope(data);
        let secret_store = InMemorySecretStore::new([("vault/mykey", bytes::Bytes::from("short"))]);
        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", secret_store, "vault");
        let err = block_on(AppConfig::<SecretLen>::from_request(&ctx))
            .expect_err("short resolved secret must fail validator");
        assert_eq!(
            err.store_extraction_reason(),
            Some(StoreExtractionReason::SchemaMismatch)
        );
        if let EdgeError::StoreExtraction { field_path, .. } = &err {
            assert_eq!(
                field_path.as_deref(),
                Some("api_token"),
                "field_path names the violating secret field: {err:?}"
            );
        }
    }

    /// SECURITY (P1): a resolved secret that FAILS a validator must NOT appear in
    /// the error message or the rendered HTTP response — `validator` echoes the
    /// rejected value in its params, and by this point the field holds the
    /// resolved secret. The field PATH must still be reported.
    #[test]
    fn runtime_validation_error_does_not_leak_resolved_secret() {
        use crate::config_store::{ConfigStore, ConfigStoreError};
        use crate::response::IntoResponse as _;

        #[derive(Debug, Deserialize, Validate)]
        struct SecretLen {
            #[validate(length(min = 100_u64))]
            api_token: String,
        }
        impl AppConfigMeta for SecretLen {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![SecretPathSegment::Field(Cow::Borrowed("api_token"))],
                    optional: false,
                }]
            }
        }
        struct BlobStore(String);
        #[async_trait(?Send)]
        impl ConfigStore for BlobStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                Ok(Some(self.0.clone()))
            }
        }

        // A distinctive resolved secret that fails the `length(min = 100)` rule.
        const SECRET: &str = "s3cr3t-DEADBEEF-do-not-leak";
        let blob = make_envelope(serde_json::json!({ "api_token": "mykey" }));
        let secret_store = InMemorySecretStore::new([("vault/mykey", bytes::Bytes::from(SECRET))]);
        let ctx = ctx_with_config_and_secrets(BlobStore(blob), "key", secret_store, "vault");
        let err = block_on(AppConfig::<SecretLen>::from_request(&ctx))
            .expect_err("short resolved secret must fail validator");

        // The error message names the field but NOT the secret.
        assert!(
            err.message().contains("api_token"),
            "names the field: {err:?}"
        );
        assert!(
            !err.message().contains(SECRET),
            "error message must not leak the resolved secret: {}",
            err.message()
        );
        // And neither does the rendered response body.
        let response = err.into_response().expect("response");
        let body = String::from_utf8_lossy(response.body().as_bytes().unwrap_or_default());
        assert!(
            !body.contains(SECRET),
            "rendered response must not leak the resolved secret: {body}"
        );
    }

    #[test]
    fn state_extractor_resolves_registered_value() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(Arc::new(AppStateFixture {
            name: "demo".to_owned(),
        }));
        let ctx = RequestContext::new(request, PathParams::default());

        let state =
            block_on(State::<Arc<AppStateFixture>>::from_request(&ctx)).expect("state present");

        // Deref: State<Arc<AppStateFixture>> -> Arc<AppStateFixture> -> AppStateFixture
        assert_eq!(state.name, "demo");
    }

    #[test]
    fn state_extractor_missing_registration_is_internal_error() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());

        // `.err().expect(..)` (not `expect_err`) so we don't require
        // `State<T>: Debug` — extractors here mirror Json/Path and omit it.
        let err = block_on(State::<Arc<AppStateFixture>>::from_request(&ctx))
            .err()
            .expect("missing state must surface as an error, not a default");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn state_extractor_deref_and_into_inner() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(AppStateFixture {
            name: "x".to_owned(),
        });
        let ctx = RequestContext::new(request, PathParams::default());

        let state = block_on(State::<AppStateFixture>::from_request(&ctx)).expect("state present");
        assert_eq!(
            *state,
            AppStateFixture {
                name: "x".to_owned()
            }
        ); // Deref
        assert_eq!(
            state.into_inner(),
            AppStateFixture {
                name: "x".to_owned()
            }
        );
    }
}
