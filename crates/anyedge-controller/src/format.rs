use crate::responder::{Json, Responder, Text};
use anyedge_core::Response;

#[cfg(feature = "json")]
pub fn json<T>(value: T) -> Response
where
    Json<T>: Responder,
{
    Json::new(value).into_response()
}

pub fn text(value: impl Into<String>) -> Response {
    Text::new(value).into_response()
}

pub fn empty_json() -> Response {
    Response::ok()
        .with_header(
            anyedge_core::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .with_body(b"{}".to_vec())
}

pub fn unauthorized(msg: impl Into<String>) -> Response {
    Response::new(401).text(msg)
}

pub fn bad_request(msg: impl Into<String>) -> Response {
    Response::new(400).text(msg)
}
