use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use http::header;
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::app_config::{AppConfigMeta, SecretKind};
use crate::blob_envelope::BlobEnvelope;
use crate::config_store::ConfigStoreHandle;
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::HeaderMap;
use crate::secret_store::SecretError;
use crate::store_registry::{
    BoundConfigStore, BoundKvStore, BoundSecretStore, ConfigRegistry, ConfigStoreBinding,
    KvRegistry, SecretRegistry,
};
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
        // Spec hard-cutoff (§ intro): no backward compatibility for
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
// AppConfig<C> — typed app-config extractor (spec §3.3, §3.3.3, §4.3)
// ---------------------------------------------------------------------------

/// Typed app-config extractor. See spec §3.3.3 + §4.3.
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
            EdgeError::internal(anyhow::anyhow!(
                "no default config store registered \u{2014} check [stores.config] in edgezero.toml"
            ))
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
    /// Returns the inner `C` directly per spec §6.2.1.
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
            EdgeError::internal(anyhow::anyhow!(
                "no config store registered for id `{store_id}`"
            ))
        })?;
        let resolved_key = key.unwrap_or(&binding.default_key).to_owned();
        extract_from_handle::<C>(ctx, &binding.handle, &resolved_key).await
    }

    /// Read the typed config from the default store under an
    /// EXPLICIT key (instead of the binding's `default_key`).
    /// Returns the inner `C` directly per spec §6.2 — handlers
    /// usually destructure the `FromRequest` extractor; the inherent
    /// methods exist for call sites that need a different key or
    /// store and prefer the bare `C` over wrapping/unwrapping.
    ///
    /// # Errors
    /// See `extract_from_handle`.
    #[inline]
    pub async fn named(ctx: &RequestContext, key: &str) -> Result<C, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no default config store registered \u{2014} check [stores.config] in edgezero.toml"
            ))
        })?;
        extract_from_handle::<C>(ctx, &binding.handle, key).await
    }
}

/// Shared body: fetch + envelope + sha + secret walk + deserialise + validate.
///
/// The `FromRequest` impl and the `named`/`from_store` inherent methods all
/// delegate here so there is one implementation path.
///
/// # Errors
///
/// - [`EdgeError::ConfigOutOfDate`] — missing blob, missing secret key,
///   deserialise failure, or validation failure on a non-secret field.
/// - [`EdgeError::Internal`] — envelope parse failure or SHA mismatch
///   (envelope integrity failures indicate a corrupt/tampered store entry,
///   not a stale config — they surface as 500 Internal).
/// - [`EdgeError::ServiceUnavailable`] — config-store backend temporarily down
///   (`ConfigStoreError::Unavailable`).
/// - [`EdgeError::BadRequest`] — malformed key (`ConfigStoreError::InvalidKey`).
async fn extract_from_handle<C>(
    ctx: &RequestContext,
    handle: &ConfigStoreHandle,
    key: &str,
) -> Result<C, EdgeError>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    // ConfigStoreError → EdgeError uses the existing `impl
    // From<ConfigStoreError> for EdgeError` at
    // `crates/edgezero-core/src/error.rs`, which maps:
    //   Unavailable  → ServiceUnavailable (503)
    //   InvalidKey   → BadRequest (400)
    //   Internal     → Internal (500)
    // NEVER `map_err(EdgeError::internal)` here — that collapses
    // backpressure / bad-key signals into 500s.
    let raw = handle
        .get(key)
        .await
        .map_err(EdgeError::from)?
        .ok_or_else(|| {
            EdgeError::config_out_of_date(
                format!(
                    "missing typed app-config blob at key `{key}` — \
                     run `<app-cli> config push` for this deploy"
                ),
                String::new(),
            )
        })?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {err}")))?;
    envelope.verify().map_err(|err| {
        EdgeError::internal(anyhow::anyhow!("envelope verification failed: {err}"))
    })?;
    let mut data = envelope.into_data();
    // Secret walk per spec §3.3.3.
    secret_walk::<C>(ctx, &mut data).await?;
    // Deserialise via serde_path_to_error so failures carry a dotted
    // field path for ConfigOutOfDate per spec §4.3.
    let cfg: C = serde_path_to_error::deserialize(data.into_deserializer())
        .map_err(|err| EdgeError::config_out_of_date_from_serde(&err))?;
    // RUNTIME uses cfg.validate(): after secret_walk the fields hold
    // RESOLVED values, so every validator rule — including those on
    // secret fields (e.g. length/regex on the actual token value) —
    // MUST run. Spec §3.3.8 split: PUSH skips secret-field validators
    // (via validate_excluding_secrets) because the value at push time
    // is a key NAME; RUNTIME runs cfg.validate() because the value is
    // now the resolved secret.
    cfg.validate().map_err(|err| {
        EdgeError::config_out_of_date(
            err.to_string(),
            first_violating_field(&err).unwrap_or_default(),
        )
    })?;
    Ok(cfg)
}

/// Walk `C::SECRET_FIELDS` and replace each `#[secret]` key NAME in `data`
/// with the resolved secret VALUE from the appropriate secret store.
///
/// `StoreRef` fields are skipped — their value is a store id, not a key.
async fn secret_walk<C>(ctx: &RequestContext, data: &mut serde_json::Value) -> Result<(), EdgeError>
where
    C: AppConfigMeta,
{
    let data_obj = data
        .as_object_mut()
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("blob `data` is not a JSON object")))?;
    for field in C::SECRET_FIELDS {
        let key_name = data_obj
            .get(field.name)
            .and_then(|val| val.as_str())
            .ok_or_else(|| {
                EdgeError::config_out_of_date(
                    format!("missing or non-string value at `{}`", field.name),
                    field.name.to_owned(),
                )
            })?
            .to_owned();
        let (bound, resolved_store_id) = match field.kind {
            SecretKind::KeyInDefault => {
                let bound = ctx.secret_store_default().ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!(
                            "secret field `{}` has kind KeyInDefault but no default secret \
                             store is registered",
                            field.name,
                        ),
                        field.name.to_owned(),
                    )
                })?;
                let id = bound.store_name().to_owned();
                (bound, id)
            }
            SecretKind::StoreRef => continue,
            SecretKind::KeyInNamedStore { store_ref_field } => {
                let store_id_str = data_obj
                    .get(store_ref_field)
                    .and_then(|val| val.as_str())
                    .ok_or_else(|| {
                        EdgeError::config_out_of_date(
                            format!(
                                "missing store_ref `{store_ref_field}` for secret field `{}`",
                                field.name
                            ),
                            field.name.to_owned(),
                        )
                    })?
                    .to_owned();
                let bound = ctx.secret_store(&store_id_str).ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!(
                            "blob declared store_ref `{store_id_str}` but \
                             [stores.secrets] has no such id"
                        ),
                        field.name.to_owned(),
                    )
                })?;
                (bound, store_id_str)
            }
        };
        let secret = bound
            .require_str(&key_name)
            .await
            .map_err(|err| map_secret_error(err, field.name, &resolved_store_id, &key_name))?;
        data_obj.insert(field.name.to_owned(), serde_json::Value::String(secret));
    }
    Ok(())
}

fn map_secret_error(
    err: SecretError,
    field_name: &str,
    store_id: &str,
    key_name: &str,
) -> EdgeError {
    match err {
        SecretError::NotFound { name } => EdgeError::config_out_of_date(
            format!("secret `{name}` in store `{store_id}` not found"),
            field_name.to_owned(),
        ),
        SecretError::Validation(msg) => EdgeError::config_out_of_date(
            format!("secret `{key_name}` in store `{store_id}` rejected: {msg}"),
            field_name.to_owned(),
        ),
        SecretError::Unavailable => {
            EdgeError::service_unavailable(format!("secret store `{store_id}` unreachable"))
        }
        SecretError::Internal(source) => EdgeError::internal(anyhow::anyhow!(
            "secret `{key_name}` in store `{store_id}` produced unexpected store error: {source}"
        )),
    }
}

/// Walk `errors` recursively and return the first violating field's DOTTED
/// PATH (e.g. `"service.timeout_ms"` for a nested failure).
///
/// Keys are sorted for determinism across runs — required for the §6.3.1
/// contract test. Round-34 M-1: earlier draft only looked at top-level keys,
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
    use crate::app_config::{AppConfigMeta, SecretField, SecretKind};
    use crate::blob_envelope::BlobEnvelope;
    use crate::body::Body;
    use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use crate::context::RequestContext;
    use crate::http::{request_builder, HeaderValue, Method, StatusCode};
    use crate::params::PathParams;
    use crate::secret_store::{InMemorySecretStore, NoopSecretStore, SecretHandle, SecretStore};
    use crate::store_registry::StoreRegistry;
    use futures::executor::block_on;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::sync::Arc;
    use validator::Validate;

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
        const SECRET_FIELDS: &'static [SecretField] = &[];
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
        const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
            name: "api_token",
            kind: SecretKind::KeyInDefault,
        }];
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
    fn app_config_extractor_returns_config_out_of_date_on_missing_blob() {
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
        assert!(
            matches!(err, EdgeError::ConfigOutOfDate { .. }),
            "expected ConfigOutOfDate, got {err:?}"
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
    fn app_config_extractor_maps_config_store_unavailable_to_service_unavailable() {
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
        assert!(
            matches!(err, EdgeError::ServiceUnavailable { .. }),
            "ConfigStoreError::Unavailable must map to ServiceUnavailable (not Internal): {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_maps_config_store_invalid_key_to_bad_request() {
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
        assert!(
            matches!(err, EdgeError::BadRequest { .. }),
            "ConfigStoreError::InvalidKey must map to BadRequest (not Internal): {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_maps_config_store_internal_to_internal() {
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
        assert!(
            matches!(err, EdgeError::Internal { .. }),
            "ConfigStoreError::Internal must map to Internal: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_returns_internal_on_sha_mismatch() {
        struct TamperedStore;
        #[async_trait(?Send)]
        impl ConfigStore for TamperedStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
                // Build a valid envelope then corrupt its sha.
                let mut env = BlobEnvelope::new(
                    serde_json::json!({ "greeting": "hi", "timeout_ms": 100_u32 }),
                    "2026-01-01T00:00:00Z".into(),
                );
                env.sha256 = "ff".repeat(32);
                Ok(Some(serde_json::to_string(&env).unwrap()))
            }
        }

        let ctx = ctx_with_config_store(TamperedStore, "key");
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("sha mismatch must error");
        // SHA mismatch → internal error (envelope integrity failure).
        assert!(
            matches!(err, EdgeError::Internal { .. }),
            "SHA mismatch must surface as Internal: {err:?}"
        );
        assert!(
            err.message().contains("envelope verification failed"),
            "message names the problem: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_returns_internal_on_bad_envelope_json() {
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
        assert!(
            matches!(err, EdgeError::Internal { .. }),
            "Envelope parse failure must be Internal: {err:?}"
        );
        assert!(
            err.message().contains("envelope parse failed"),
            "message names the problem: {err:?}"
        );
    }

    #[test]
    fn app_config_extractor_returns_config_out_of_date_on_deserialise_failure() {
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
        assert!(
            matches!(err, EdgeError::ConfigOutOfDate { .. }),
            "deserialise failure must be ConfigOutOfDate: {err:?}"
        );
        // serde_path_to_error should give us the field path.
        if let EdgeError::ConfigOutOfDate { field_path, .. } = &err {
            assert_eq!(
                field_path, "timeout_ms",
                "serde_path_to_error must supply the field name: {err:?}"
            );
        }
    }

    #[test]
    fn app_config_extractor_returns_config_out_of_date_on_validation_failure() {
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
        assert!(
            matches!(err, EdgeError::ConfigOutOfDate { .. }),
            "validation failure must be ConfigOutOfDate: {err:?}"
        );
        if let EdgeError::ConfigOutOfDate { field_path, .. } = &err {
            assert_eq!(
                field_path, "timeout_ms",
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
    fn app_config_secret_walk_missing_key_in_default_store_is_config_out_of_date() {
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
        assert!(
            matches!(err, EdgeError::ConfigOutOfDate { .. }),
            "missing secret must be ConfigOutOfDate: {err:?}"
        );
        if let EdgeError::ConfigOutOfDate { field_path, .. } = &err {
            assert_eq!(
                field_path, "api_token",
                "field_path names the secret field: {err:?}"
            );
        }
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
    fn app_config_no_registry_returns_internal_error() {
        // No ConfigRegistry in extensions → Internal error.
        let request = request_builder()
            .method(Method::GET)
            .uri("/cfg")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let err = block_on(AppConfig::<FixtureCfg>::from_request(&ctx))
            .expect_err("no registry must error");
        assert!(
            matches!(err, EdgeError::Internal { .. }),
            "no registry must surface as Internal: {err:?}"
        );
        assert!(
            err.message().contains("no default config store registered"),
            "message names the gap: {err:?}"
        );
    }

    // -- Runtime validation of resolved secret values (spec §3.3.8) -----------

    /// Spec §3.3.8: RUNTIME runs `cfg.validate()` after `secret_walk` so that
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
            const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
                name: "api_token",
                kind: SecretKind::KeyInDefault,
            }];
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

    /// Spec §3.3.8: a resolved secret that FAILS a validator on the secret
    /// field must produce `ConfigOutOfDate` — the runtime runs the full validator.
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
            const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
                name: "api_token",
                kind: SecretKind::KeyInDefault,
            }];
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
        assert!(
            matches!(err, EdgeError::ConfigOutOfDate { .. }),
            "validator failure on resolved secret must be ConfigOutOfDate: {err:?}"
        );
        if let EdgeError::ConfigOutOfDate { field_path, .. } = &err {
            assert_eq!(
                field_path, "api_token",
                "field_path names the violating secret field: {err:?}"
            );
        }
    }
}
