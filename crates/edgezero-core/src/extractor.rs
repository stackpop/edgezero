use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use http::header;
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::HeaderMap;

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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.json().map(Json)
    }
}

impl<T> Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Json<T> {
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

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedJson<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedJson<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

pub struct Headers(pub HeaderMap);

#[async_trait(?Send)]
impl FromRequest for Headers {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        Ok(Headers(ctx.request().headers().clone()))
    }
}

impl Deref for Headers {
    type Target = HeaderMap;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Headers {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Headers {
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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let headers = ctx.request().headers();
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost")
            .to_string();
        Ok(Host(host))
    }
}

impl Deref for Host {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Host {
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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let headers = ctx.request().headers();
        let host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost")
            .to_string();
        Ok(ForwardedHost(host))
    }
}

impl Deref for ForwardedHost {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ForwardedHost {
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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.query().map(Query)
    }
}

impl<T> Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Query<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Query<T> {
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

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedQuery<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedQuery<T> {
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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.path().map(Path)
    }
}

impl<T> Deref for Path<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Path<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Path<T> {
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

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedPath<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedPath<T> {
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
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.form().map(Form)
    }
}

impl<T> Deref for Form<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Form<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Form<T> {
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

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for ValidatedForm<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> ValidatedForm<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// Extracts the [`KvHandle`] from the request context.
///
/// Returns `EdgeError::Internal` if no KV store was configured for this request.
///
/// # Example
/// ```ignore
/// #[action]
/// pub async fn handler(Kv(store): Kv) -> Result<Response, EdgeError> {
///     let count: i32 = store.get_or("visits", 0).await?;
///     store.put("visits", &(count + 1)).await?;
///     Ok(Response::ok(format!("visits: {}", count + 1)))
/// }
/// ```
#[derive(Debug)]
pub struct Kv(pub crate::kv::KvHandle);

#[async_trait(?Send)]
impl FromRequest for Kv {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.kv_handle()
            .map(Kv)
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no kv store configured")))
    }
}

impl std::ops::Deref for Kv {
    type Target = crate::kv::KvHandle;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Kv {
    #[must_use]
    pub fn into_inner(self) -> crate::kv::KvHandle {
        self.0
    }
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

    fn ctx(body: Body, params: PathParams) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri("/test")
            .body(body)
            .expect("request");
        RequestContext::new(request, params)
    }

    fn params(values: &[(&str, &str)]) -> PathParams {
        let map = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>();
        PathParams::new(map)
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Payload {
        name: String,
    }

    #[derive(Debug, Deserialize, Serialize, Validate)]
    struct ValidatedPayload {
        #[validate(length(min = 1))]
        name: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct PathPayload {
        id: String,
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
        let body = Body::json(&ValidatedPayload { name: "".into() }).expect("json");
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
            headers.get("x-test").and_then(|v| v.to_str().ok()).unwrap(),
            "value"
        );
    }

    // Query extractor tests
    #[derive(Debug, Deserialize, PartialEq)]
    struct QueryParams {
        page: Option<u32>,
        q: Option<String>,
    }

    fn ctx_with_query(query: &str) -> RequestContext {
        let uri = format!("/test?{}", query);
        let request = request_builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    #[test]
    fn query_extractor_parses_params() {
        let ctx = ctx_with_query("page=5&q=hello");
        let query = block_on(Query::<QueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, Some(5));
        assert_eq!(query.q.as_deref(), Some("hello"));
    }

    #[test]
    fn query_extractor_handles_missing_optional_params() {
        let ctx = ctx_with_query("page=1");
        let query = block_on(Query::<QueryParams>::from_request(&ctx)).expect("query");
        assert_eq!(query.page, Some(1));
        assert_eq!(query.q, None);
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
        assert_eq!(query.q, None);
    }

    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedQueryParams {
        #[validate(range(min = 1, max = 100))]
        page: u32,
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

    // Form extractor tests
    fn ctx_with_form(body: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri("/test")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct FormData {
        username: String,
        age: Option<u32>,
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

    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedFormData {
        #[validate(length(min = 3))]
        username: String,
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

    // ValidatedPath tests
    #[derive(Debug, Deserialize, Validate)]
    struct ValidatedPathParams {
        #[validate(length(min = 1, max = 10))]
        id: String,
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
            q: None,
        });
        assert_eq!(query.page, Some(1)); // Deref
        let inner = query.into_inner();
        assert_eq!(inner.page, Some(1));
    }

    #[test]
    fn query_deref_mut() {
        let mut query = Query(QueryParams {
            page: Some(1),
            q: None,
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
        let host = Host("example.com".to_string());
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
        let host = ForwardedHost("example.com".to_string());
        assert_eq!(&*host, "example.com"); // Deref
        let inner = host.into_inner();
        assert_eq!(inner, "example.com");
    }

    // -- Kv extractor -------------------------------------------------------

    #[test]
    fn kv_extractor_returns_handle_when_configured() {
        use crate::kv::{KvHandle, KvStore};
        use std::sync::Arc;

        struct NoopStore;

        #[async_trait(?Send)]
        impl KvStore for NoopStore {
            async fn get_bytes(
                &self,
                _key: &str,
            ) -> Result<Option<bytes::Bytes>, crate::kv::KvError> {
                Ok(None)
            }
            async fn put_bytes(
                &self,
                _key: &str,
                _value: bytes::Bytes,
            ) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn put_bytes_with_ttl(
                &self,
                _key: &str,
                _value: bytes::Bytes,
                _ttl: std::time::Duration,
            ) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn delete(&self, _key: &str) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn list_keys(&self, _prefix: &str) -> Result<Vec<String>, crate::kv::KvError> {
                Ok(vec![])
            }
        }

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/kv")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(KvHandle::new(Arc::new(NoopStore)));

        let ctx = RequestContext::new(request, PathParams::default());
        let kv = block_on(Kv::from_request(&ctx));
        assert!(kv.is_ok());
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
        assert!(err.message().contains("no kv store configured"));
    }

    #[test]
    fn kv_deref_and_into_inner() {
        use crate::kv::{KvHandle, KvStore};
        use std::sync::Arc;

        struct NoopStore;

        #[async_trait(?Send)]
        impl KvStore for NoopStore {
            async fn get_bytes(
                &self,
                _key: &str,
            ) -> Result<Option<bytes::Bytes>, crate::kv::KvError> {
                Ok(None)
            }
            async fn put_bytes(
                &self,
                _key: &str,
                _value: bytes::Bytes,
            ) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn put_bytes_with_ttl(
                &self,
                _key: &str,
                _value: bytes::Bytes,
                _ttl: std::time::Duration,
            ) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn delete(&self, _key: &str) -> Result<(), crate::kv::KvError> {
                Ok(())
            }
            async fn list_keys(&self, _prefix: &str) -> Result<Vec<String>, crate::kv::KvError> {
                Ok(vec![])
            }
        }

        let handle = KvHandle::new(Arc::new(NoopStore));
        let kv = Kv(handle);

        // Debug works
        let debug = format!("{:?}", kv);
        assert!(debug.contains("Kv"));

        // Deref works
        let _: &KvHandle = &*kv;

        // into_inner works
        let _inner: KvHandle = kv.into_inner();
    }
}
