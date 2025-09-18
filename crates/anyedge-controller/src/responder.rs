use anyedge_core::{header, Response};
#[cfg(feature = "json")]
use serde::Serialize;

pub trait Responder {
    fn into_response(self) -> Response;
}

impl Responder for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl<T, E> Responder for Result<T, E>
where
    T: Responder,
    E: Responder,
{
    fn into_response(self) -> Response {
        match self {
            Ok(value) => value.into_response(),
            Err(err) => err.into_response(),
        }
    }
}

impl Responder for () {
    fn into_response(self) -> Response {
        Response::new(204)
    }
}

#[cfg(feature = "json")]
pub struct Json<T>(pub T);

#[cfg(feature = "json")]
impl<T> Json<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

#[cfg(feature = "json")]
impl<T> Responder for Json<T>
where
    T: Serialize,
{
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.0).unwrap_or_else(|err| {
            serde_json::to_vec(&serde_json::json!({ "error": err.to_string() })).unwrap_or_default()
        });
        Response::ok()
            .with_header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .with_body(body)
    }
}

pub struct Text<T>(pub T);

impl<T> Text<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> Responder for Text<T>
where
    T: Into<String>,
{
    fn into_response(self) -> Response {
        Response::ok()
            .with_header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .with_body(self.0.into())
    }
}

pub fn respond(value: impl Responder) -> Response {
    value.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControllerError;

    #[test]
    fn text_responder_sets_content_type() {
        let response = Text::new("hello").into_response();
        assert_eq!(response.status.as_u16(), 200);
        let header = response.headers.get("content-type").unwrap();
        assert_eq!(header.to_str().unwrap(), "text/plain; charset=utf-8");
        assert_eq!(String::from_utf8(response.body.clone()).unwrap(), "hello");
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_responder_serializes_body() {
        let response = Json::new(serde_json::json!({"ok": true})).into_response();
        assert_eq!(response.status.as_u16(), 200);
        let header = response.headers.get("content-type").unwrap();
        assert_eq!(header.to_str().unwrap(), "application/json; charset=utf-8");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn respond_handles_result_ok() {
        let response = respond(Ok::<_, ControllerError>(Response::ok().text("ok")));
        assert_eq!(response.status.as_u16(), 200);
    }

    #[test]
    fn respond_handles_result_err() {
        let err = ControllerError::BadRequest("bad".into());
        let response = respond(Err::<Response, _>(err));
        assert_eq!(response.status.as_u16(), 400);
    }
}
