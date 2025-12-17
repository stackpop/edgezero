use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
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
        let err = match block_on(Json::<Payload>::from_request(&ctx)) {
            Ok(_) => panic!("expected error"),
            Err(err) => err,
        };
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validated_json_rejects_invalid_payloads() {
        let body = Body::json(&ValidatedPayload { name: "".into() }).expect("json");
        let ctx = ctx(body, PathParams::default());
        let err = match block_on(ValidatedJson::<ValidatedPayload>::from_request(&ctx)) {
            Ok(_) => panic!("expected validation error"),
            Err(err) => err,
        };
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
}
