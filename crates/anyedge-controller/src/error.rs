use anyedge_core::{header, http::StatusCode, Response};
use thiserror::Error;

use crate::responder::Responder;

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("request body already extracted")]
    BodyAlreadyExtracted,
    #[error("request body missing")]
    BodyMissing,
    #[error("failed to deserialize JSON: {0}")]
    JsonError(String),
    #[cfg(feature = "form")]
    #[error("failed to deserialize form: {0}")]
    FormError(String),
    #[error("failed to deserialize query: {0}")]
    QueryError(String),
    #[error("failed to deserialize path parameters: {0}")]
    PathError(String),
    #[error("missing required parameter: {0}")]
    MissingParameter(String),
    #[error("application state for `{0}` not found")]
    StateNotFound(&'static str),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("bad request: {0}")]
    BadRequest(String),
}

impl ControllerError {
    #[allow(dead_code)]
    pub fn with_message(status: StatusCode, message: impl Into<String>) -> ControllerResponse {
        ControllerResponse {
            status,
            body: message.into(),
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            ControllerError::BodyAlreadyExtracted => StatusCode::BAD_REQUEST,
            ControllerError::BodyMissing => StatusCode::BAD_REQUEST,
            ControllerError::JsonError(_) => StatusCode::BAD_REQUEST,
            #[cfg(feature = "form")]
            ControllerError::FormError(_) => StatusCode::BAD_REQUEST,
            ControllerError::QueryError(_) => StatusCode::BAD_REQUEST,
            ControllerError::PathError(_) => StatusCode::BAD_REQUEST,
            ControllerError::MissingParameter(_) => StatusCode::BAD_REQUEST,
            ControllerError::StateNotFound(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ControllerError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ControllerError::BadRequest(_) => StatusCode::BAD_REQUEST,
        }
    }

    fn message(&self) -> String {
        match self {
            ControllerError::BodyAlreadyExtracted => "request body already consumed".to_string(),
            ControllerError::BodyMissing => "request body missing".to_string(),
            ControllerError::JsonError(msg) => msg.clone(),
            #[cfg(feature = "form")]
            ControllerError::FormError(msg) => msg.clone(),
            ControllerError::QueryError(msg) => msg.clone(),
            ControllerError::PathError(msg) => msg.clone(),
            ControllerError::MissingParameter(name) => format!("missing parameter: {name}"),
            ControllerError::StateNotFound(name) => {
                format!("application state `{name}` not registered")
            }
            ControllerError::Validation(msg) => msg.clone(),
            ControllerError::BadRequest(msg) => msg.clone(),
        }
    }
}

impl Responder for ControllerError {
    fn into_response(self) -> Response {
        #[cfg(feature = "json")]
        {
            return match self {
                ControllerError::JsonError(message) => {
                    let body = serde_json::json!({
                        "error": "invalid_json",
                        "message": message,
                    })
                    .to_string();
                    Response::new(StatusCode::BAD_REQUEST.as_u16())
                        .with_header(header::CONTENT_TYPE, "application/json")
                        .with_body(body)
                }
                other => ControllerResponse {
                    status: other.status(),
                    body: other.message(),
                }
                .into_response(),
            };
        }

        #[cfg(not(feature = "json"))]
        {
            ControllerResponse {
                status: self.status(),
                body: self.message(),
            }
            .into_response()
        }
    }
}

impl From<ControllerError> for Response {
    fn from(err: ControllerError) -> Self {
        err.into_response()
    }
}

#[derive(Debug)]
pub struct ControllerResponse {
    status: StatusCode,
    body: String,
}

impl Responder for ControllerResponse {
    fn into_response(self) -> Response {
        Response::new(self.status.as_u16()).text(self.body)
    }
}
