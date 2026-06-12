use crate::error::EdgeError;
use crate::http::Response;
use crate::response::IntoResponse;

pub trait Responder: Sized {
    /// # Errors
    /// Returns [`EdgeError`] if the value cannot be turned into a response (e.g., a `Result`'s `Err` variant).
    fn respond(self) -> Result<Response, EdgeError>;
}

impl<T> Responder for T
where
    T: IntoResponse,
{
    #[inline]
    fn respond(self) -> Result<Response, EdgeError> {
        self.into_response()
    }
}

impl<T> Responder for Result<T, EdgeError>
where
    T: IntoResponse,
{
    #[inline]
    fn respond(self) -> Result<Response, EdgeError> {
        self.and_then(IntoResponse::into_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::StatusCode;

    #[test]
    fn responder_for_into_response_types() {
        let response = "hello".respond().expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"hello");
    }

    #[test]
    fn responder_for_result_propagates_error() {
        let err = EdgeError::bad_request("nope");
        let response = Result::<Body, _>::Err(err).respond().unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.message(), "nope");
    }
}
