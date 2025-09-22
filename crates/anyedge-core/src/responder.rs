use crate::{EdgeError, IntoResponse, Response};

pub trait Responder: Sized {
    fn respond(self) -> Result<Response, EdgeError>;
}

impl<T> Responder for T
where
    T: IntoResponse,
{
    fn respond(self) -> Result<Response, EdgeError> {
        Ok(self.into_response())
    }
}

impl<T> Responder for Result<T, EdgeError>
where
    T: IntoResponse,
{
    fn respond(self) -> Result<Response, EdgeError> {
        self.map(IntoResponse::into_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, StatusCode};

    #[test]
    fn responder_for_into_response_types() {
        let response = "hello".respond().expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes(), b"hello");
    }

    #[test]
    fn responder_for_result_propagates_error() {
        let err = EdgeError::bad_request("nope");
        let response = Result::<Body, _>::Err(err).respond().unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.message(), "nope");
    }
}
