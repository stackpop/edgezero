use std::collections::HashMap;
use std::sync::Arc;

use anyedge_core::{HeaderMap, Method, Request};
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::{error::ControllerError, request::RequestParts};

pub trait FromRequest: Sized {
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError>;
}

impl FromRequest for Method {
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        Ok(parts.request().method.clone())
    }
}

impl FromRequest for HeaderMap {
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        Ok(parts.request().headers.clone())
    }
}

impl FromRequest for Request {
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        Ok(parts.take_request())
    }
}

pub struct Path<T>(pub T);

impl<T> Path<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> FromRequest for Path<T>
where
    T: DeserializeOwned,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        deserialize_map(parts.request().params.clone())
            .map(Path)
            .map_err(ControllerError::PathError)
    }
}

pub struct ValidatedPath<T>(pub T);

impl<T> ValidatedPath<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> FromRequest for ValidatedPath<T>
where
    T: DeserializeOwned + Validate,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        let Path(value) = Path::<T>::from_request(parts)?;
        value
            .validate()
            .map(|_| ValidatedPath(value))
            .map_err(|err| ControllerError::Validation(err.to_string()))
    }
}

pub struct Query<T>(pub T);

impl<T> Query<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> FromRequest for Query<T>
where
    T: DeserializeOwned,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        deserialize_map(parts.request().query_params.clone())
            .map(Query)
            .map_err(ControllerError::QueryError)
    }
}

pub struct ValidatedQuery<T>(pub T);

impl<T> ValidatedQuery<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> FromRequest for ValidatedQuery<T>
where
    T: DeserializeOwned + Validate,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        let Query(value) = Query::<T>::from_request(parts)?;
        value
            .validate()
            .map(|_| ValidatedQuery(value))
            .map_err(|err| ControllerError::Validation(err.to_string()))
    }
}

pub struct State<T>(pub Arc<T>);

impl<T> State<T> {
    pub fn into_inner(self) -> Arc<T> {
        self.0
    }
}

impl<T> FromRequest for State<T>
where
    T: Send + Sync + 'static,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        if let Some(value) = parts.request().extensions.get::<Arc<T>>() {
            return Ok(State(value.clone()));
        }
        Err(ControllerError::StateNotFound(std::any::type_name::<T>()))
    }
}

#[cfg(feature = "json")]
pub struct Json<T>(pub T);

#[cfg(feature = "json")]
impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

#[cfg(feature = "json")]
impl<T> FromRequest for Json<T>
where
    T: DeserializeOwned,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        if parts.body_taken() {
            return Err(ControllerError::BodyAlreadyExtracted);
        }
        let body = parts.take_body();
        if body.is_empty() {
            return Err(ControllerError::BodyMissing);
        }
        serde_json::from_slice(&body)
            .map(Json)
            .map_err(|err| ControllerError::JsonError(err.to_string()))
    }
}

#[cfg(feature = "json")]
pub struct ValidatedJson<T>(pub T);

#[cfg(feature = "json")]
impl<T> ValidatedJson<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

#[cfg(feature = "json")]
impl<T> FromRequest for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        let Json(value) = Json::<T>::from_request(parts)?;
        value
            .validate()
            .map(|_| ValidatedJson(value))
            .map_err(|err| ControllerError::Validation(err.to_string()))
    }
}

#[cfg(feature = "form")]
pub struct Form<T>(pub T);

#[cfg(feature = "form")]
impl<T> Form<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

#[cfg(feature = "form")]
impl<T> FromRequest for Form<T>
where
    T: DeserializeOwned,
{
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        if parts.body_taken() {
            return Err(ControllerError::BodyAlreadyExtracted);
        }
        let body = parts.take_body();
        if body.is_empty() {
            return Err(ControllerError::BodyMissing);
        }
        serde_urlencoded::from_bytes(&body)
            .map(Form)
            .map_err(|err| ControllerError::FormError(err.to_string()))
    }
}

pub struct RawRequest(pub Request);

impl FromRequest for RawRequest {
    fn from_request(parts: &mut RequestParts) -> Result<Self, ControllerError> {
        Ok(RawRequest(parts.take_request()))
    }
}

fn deserialize_map<T>(map: HashMap<String, String>) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let mut value = serde_json::Map::new();
    for (key, val) in map {
        let json_val = serde_json::from_str(&val).unwrap_or(serde_json::Value::String(val));
        value.insert(key, json_val);
    }
    serde_json::from_value(serde_json::Value::Object(value)).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{Method, Request};
    use validator::Validate;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Params {
        slug: String,
    }

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Filters {
        q: String,
        page: i32,
    }

    fn build_request(method: Method, path: &str) -> Request {
        Request::new(method, path)
    }

    #[test]
    fn path_extractor_deserializes_struct() {
        let mut req = build_request(Method::GET, "/notes/demo");
        req.params.insert("slug".into(), "demo".into());

        let mut parts = RequestParts::new(req);
        let Path(params): Path<Params> = Path::from_request(&mut parts).expect("path extractor");
        assert_eq!(params.slug, "demo");
    }

    #[test]
    fn query_extractor_deserializes_struct() {
        let mut req = build_request(Method::GET, "/search");
        req.query_params.insert("q".into(), "rust".into());
        req.query_params.insert("page".into(), "2".into());

        let mut parts = RequestParts::new(req);
        let Query(filters): Query<Filters> =
            Query::from_request(&mut parts).expect("query extractor");
        assert_eq!(filters.q, "rust");
        assert_eq!(filters.page, 2);
    }

    #[derive(serde::Deserialize, Validate, Debug)]
    struct SlugParams {
        #[validate(length(min = 2))]
        slug: String,
    }

    #[test]
    fn validated_path_runs_validation() {
        let mut req = build_request(Method::GET, "/notes/demo");
        req.params.insert("slug".into(), "demo".into());

        let mut parts = RequestParts::new(req);
        let ValidatedPath(params): ValidatedPath<SlugParams> =
            ValidatedPath::from_request(&mut parts).expect("validated path extractor");
        assert_eq!(params.slug, "demo");
    }

    #[test]
    fn validated_path_returns_error_when_invalid() {
        let mut req = build_request(Method::GET, "/notes/x");
        req.params.insert("slug".into(), "x".into());

        let mut parts = RequestParts::new(req);
        let result = ValidatedPath::<SlugParams>::from_request(&mut parts);
        assert!(matches!(result, Err(ControllerError::Validation(_))));
    }

    #[derive(serde::Deserialize, Validate, Debug)]
    struct QueryParams {
        #[validate(range(min = 1, max = 5))]
        page: i32,
    }

    #[test]
    fn validated_query_runs_validation() {
        let mut req = build_request(Method::GET, "/search");
        req.query_params.insert("page".into(), "3".into());

        let mut parts = RequestParts::new(req);
        let ValidatedQuery(params): ValidatedQuery<QueryParams> =
            ValidatedQuery::from_request(&mut parts).expect("validated query extractor");
        assert_eq!(params.page, 3);
    }

    #[test]
    fn validated_query_returns_error_when_invalid() {
        let mut req = build_request(Method::GET, "/search");
        req.query_params.insert("page".into(), "0".into());

        let mut parts = RequestParts::new(req);
        let result = ValidatedQuery::<QueryParams>::from_request(&mut parts);
        assert!(matches!(result, Err(ControllerError::Validation(_))));
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_extractor_deserializes_body() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Payload {
            title: String,
        }

        let mut req = build_request(Method::POST, "/json");
        req.body = serde_json::to_vec(&serde_json::json!({"title": "Edge"})).unwrap();

        let mut parts = RequestParts::new(req);
        let Json(payload): Json<Payload> = Json::from_request(&mut parts).expect("json extractor");
        assert_eq!(payload.title, "Edge");
    }

    #[cfg(feature = "form")]
    #[test]
    fn form_extractor_deserializes_body() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Payload {
            name: String,
        }

        let mut req = build_request(Method::POST, "/form");
        req.body = b"name=Edge".to_vec();

        let mut parts = RequestParts::new(req);
        let Form(payload): Form<Payload> = Form::from_request(&mut parts).expect("form extractor");
        assert_eq!(payload.name, "Edge");
    }

    #[test]
    fn raw_request_returns_original() {
        let req = build_request(Method::GET, "/raw");
        let mut parts = RequestParts::new(req.clone());
        let RawRequest(original) = RawRequest::from_request(&mut parts).expect("raw request");
        assert_eq!(original.path, req.path);
    }

    #[test]
    fn state_extractor_reads_arc() {
        let mut req = build_request(Method::GET, "/state");
        req.extensions.insert(std::sync::Arc::new(42usize));
        let mut parts = RequestParts::new(req);
        let State(value): State<usize> = State::from_request(&mut parts).expect("state");
        assert_eq!(*value, 42);
    }
}
