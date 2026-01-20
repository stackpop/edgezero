use crate::body::Body;
use crate::http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    HeaderValue, Response, StatusCode,
};

/// Convert common return types into `Response`.
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for Body {
    fn into_response(self) -> Response {
        response_with_body(StatusCode::OK, self)
    }
}

impl IntoResponse for &str {
    fn into_response(self) -> Response {
        response_with_body(StatusCode::OK, Body::text(self))
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        response_with_body(StatusCode::OK, Body::text(self))
    }
}

pub struct Text<T>(T);

impl<T> Text<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> IntoResponse for Text<T>
where
    T: Into<String>,
{
    fn into_response(self) -> Response {
        response_with_body(StatusCode::OK, Body::text(self.0.into()))
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        response_with_body(StatusCode::NO_CONTENT, Body::empty())
    }
}

impl<T> IntoResponse for (StatusCode, T)
where
    T: IntoResponse,
{
    fn into_response(self) -> Response {
        let (status, inner) = self;
        let mut response = inner.into_response();
        *response.status_mut() = status;
        response
    }
}

pub fn response_with_body(status: StatusCode, body: Body) -> Response {
    use crate::http::response_builder;

    let mut builder = response_builder().status(status);

    if let Body::Once(ref bytes) = body {
        if !bytes.is_empty() {
            builder = builder
                .header(CONTENT_LENGTH, bytes.len().to_string())
                .header(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
        }
    }

    builder
        .body(body)
        .expect("static response builder should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_with_body_sets_length_and_type() {
        let response = response_with_body(StatusCode::OK, Body::from("hello"));
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .unwrap(),
            "5"
        );
        assert_eq!(
            headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn empty_body_does_not_set_length() {
        let response = response_with_body(StatusCode::OK, Body::empty());
        assert!(response.headers().get(CONTENT_LENGTH).is_none());
    }

    #[test]
    fn text_wrapper_builds_response() {
        let response = Text::new("hello").into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes(), b"hello");
    }

    #[test]
    fn unit_type_sets_no_content() {
        let response = ().into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.body().as_bytes().is_empty());
    }

    #[test]
    fn status_code_tuple_overrides_status() {
        let response = (StatusCode::CREATED, "created").into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.body().as_bytes(), b"created");
    }
}
