use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use http::header;
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::HeaderMap;
use crate::store_registry::{
    BoundConfigStore, BoundKvStore, BoundSecretStore, ConfigRegistry, KvRegistry, SecretRegistry,
    StoreRegistry,
};

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

/// Extractor that yields the per-request [`KvRegistry`] (§6.9).
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
        if let Some(registry) = ctx.request().extensions().get::<KvRegistry>().cloned() {
            return Ok(Kv(registry));
        }
        // Legacy fallback: synthesize a single-id registry from the lone handle
        // so adapters that have not yet wired registries keep working.
        if let Some(handle) = ctx.kv_handle() {
            return Ok(Kv(single_id_registry(handle)));
        }
        Err(EdgeError::internal(anyhow::anyhow!(
            "no kv store configured -- check [stores.kv] in edgezero.toml and platform bindings"
        )))
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

/// Extractor that yields the per-request [`SecretRegistry`] (§6.9).
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
        if let Some(registry) = ctx.request().extensions().get::<SecretRegistry>().cloned() {
            return Ok(Secrets(registry));
        }
        if let Some(handle) = ctx.secret_handle() {
            // Legacy fallback: wrap the lone `SecretHandle` into a one-id
            // registry under the conventional `"default"` platform name.
            // Adapters that haven't yet wired a real `SecretRegistry` keep
            // working through this path.
            let bound = BoundSecretStore::new(handle, "default".to_owned());
            return Ok(Secrets(single_id_registry(bound)));
        }
        Err(EdgeError::internal(anyhow::anyhow!(
            "no secret store configured -- check [stores.secrets] in edgezero.toml and platform bindings"
        )))
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

/// Extractor that yields the per-request [`ConfigRegistry`] (§6.9).
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
        if let Some(registry) = ctx.request().extensions().get::<ConfigRegistry>().cloned() {
            return Ok(Config(registry));
        }
        if let Some(handle) = ctx.config_handle() {
            return Ok(Config(single_id_registry(handle)));
        }
        Err(EdgeError::internal(anyhow::anyhow!(
            "no config store configured -- check [stores.config] in edgezero.toml and platform bindings"
        )))
    }
}

impl Config {
    /// Resolve the default [`BoundConfigStore`].
    #[must_use]
    #[inline]
    pub fn default(&self) -> Option<BoundConfigStore> {
        self.0.default()
    }

    /// Resolve the [`BoundConfigStore`] for `id`. Strict lookup — unknown ids
    /// yield `None`.
    #[must_use]
    #[inline]
    pub fn named(&self, id: &str) -> Option<BoundConfigStore> {
        self.0.named(id)
    }

    /// Access the underlying registry directly.
    #[must_use]
    #[inline]
    pub fn registry(&self) -> &ConfigRegistry {
        &self.0
    }
}

/// Wrap a legacy single handle into a one-id registry under the conventional
/// `"default"` id. Used by the extractor fallback path while not every adapter
/// wires a real registry.
fn single_id_registry<H: Clone>(handle: H) -> StoreRegistry<H> {
    let mut by_id: BTreeMap<String, H> = BTreeMap::new();
    by_id.insert("default".to_owned(), handle);
    StoreRegistry::new(by_id, "default".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::http::{request_builder, HeaderValue, Method, StatusCode};
    use crate::params::PathParams;
    use futures::executor::block_on;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
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
    fn kv_extractor_falls_back_to_legacy_handle() {
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
        let kv = block_on(Kv::from_request(&ctx)).expect("Kv extractor when handle present");
        // No registry wired → synthetic single-id registry under "default".
        assert!(kv.default().is_some());
        assert!(kv.named("default").is_some());
        assert!(kv.named("other").is_none());
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
    fn secrets_extractor_falls_back_to_legacy_handle() {
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
        let secrets =
            block_on(Secrets::from_request(&ctx)).expect("Secrets extractor when handle present");
        let bound = secrets
            .default()
            .expect("legacy fallback yields a bound store");
        assert_eq!(bound.store_name(), "default");
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
                    ConfigStoreHandle::new(Arc::new(FixedStore("primary"))),
                ),
                (
                    "analytics".to_owned(),
                    ConfigStoreHandle::new(Arc::new(FixedStore("analytics"))),
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
    fn config_extractor_falls_back_to_legacy_handle() {
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
        let config =
            block_on(Config::from_request(&ctx)).expect("Config extractor when handle present");
        assert!(config.default().is_some());
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
}
