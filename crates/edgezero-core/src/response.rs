use crate::body::Body;
use crate::error::EdgeError;
use crate::http::{
    HeaderValue, Response, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE},
};

/// Convert common return types into `Response`.
///
/// **Breaking change (pre-1.0):** this trait now returns `Result<Response,
/// EdgeError>`. Callers must propagate response-building failures (typically
/// invalid headers) instead of letting them panic at the `http::Builder`
/// boundary.
pub trait IntoResponse {
    /// # Errors
    /// Returns [`EdgeError::internal`] if the underlying HTTP response cannot
    /// be assembled — propagated so the request can fail cleanly instead of
    /// crashing the worker.
    fn into_response(self) -> Result<Response, EdgeError>;
}

impl IntoResponse for Response {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        Ok(self)
    }
}

impl IntoResponse for Body {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, self)
    }
}

impl IntoResponse for &str {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, Body::text(self))
    }
}

impl IntoResponse for String {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, Body::text(self))
    }
}

pub struct Text<T>(T);

impl<T> Text<T> {
    #[inline]
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> IntoResponse for Text<T>
where
    T: Into<String>,
{
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, Body::text(self.0.into()))
    }
}

impl IntoResponse for () {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::NO_CONTENT, Body::empty())
    }
}

impl<T> IntoResponse for (StatusCode, T)
where
    T: IntoResponse,
{
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        let (status, inner) = self;
        let mut response = inner.into_response()?;
        *response.status_mut() = status;
        Ok(response)
    }
}

/// # Errors
/// Returns [`EdgeError::internal`] if the underlying [`http::response::Builder`]
/// rejects the supplied status, headers, or body.
#[inline]
pub fn response_with_body(status: StatusCode, body: Body) -> Result<Response, EdgeError> {
    use crate::http::response_builder;

    let mut builder = response_builder().status(status);

    if let Body::Once(bytes) = &body
        && !bytes.is_empty()
    {
        builder = builder
            .header(CONTENT_LENGTH, bytes.len().to_string())
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
    }

    builder.body(body).map_err(EdgeError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_with_body_sets_length_and_type() {
        let response = response_with_body(StatusCode::OK, Body::from("hello")).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .unwrap(),
            "5"
        );
        assert_eq!(
            headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn empty_body_does_not_set_length() {
        let response = response_with_body(StatusCode::OK, Body::empty()).expect("response");
        assert!(response.headers().get(CONTENT_LENGTH).is_none());
    }

    #[test]
    fn text_wrapper_builds_response() {
        let response = Text::new("hello").into_response().expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"hello");
    }

    #[test]
    fn unit_type_sets_no_content() {
        let response = ().into_response().expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.body().as_bytes().expect("buffered").is_empty());
    }

    #[test]
    fn status_code_tuple_overrides_status() {
        let response = (StatusCode::CREATED, "created")
            .into_response()
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"created");
    }
}
