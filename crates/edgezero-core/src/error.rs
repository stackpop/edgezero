use anyhow::Error as AnyError;
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

use crate::body::Body;
use crate::config_store::ConfigStoreError;
use crate::http::{header::CONTENT_TYPE, HeaderValue, Method, Response, StatusCode};
use crate::response::{response_with_body, IntoResponse};

/// Application-level error that carries an HTTP status code.
#[derive(Debug, Error)]
pub enum EdgeError {
    #[error("{message}")]
    BadRequest { message: String },
    #[error("no route matched path: {path}")]
    NotFound { path: String },
    #[error("method {method} not allowed; allowed: {allowed}")]
    MethodNotAllowed { method: Method, allowed: String },
    #[error("validation error: {message}")]
    Validation { message: String },
    #[error("{message}")]
    ServiceUnavailable { message: String },
    #[error("internal error: {source}")]
    Internal {
        #[from]
        source: AnyError,
    },
}

impl EdgeError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        EdgeError::BadRequest {
            message: message.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        EdgeError::Validation {
            message: message.into(),
        }
    }

    pub fn not_found(path: impl Into<String>) -> Self {
        EdgeError::NotFound { path: path.into() }
    }

    pub fn method_not_allowed(method: &Method, allowed: &[Method]) -> Self {
        let mut names = allowed
            .iter()
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>();
        names.sort();
        let allowed_list = if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        };
        EdgeError::MethodNotAllowed {
            method: method.clone(),
            allowed: allowed_list,
        }
    }

    pub fn internal<E>(error: E) -> Self
    where
        E: Into<AnyError>,
    {
        EdgeError::Internal {
            source: error.into(),
        }
    }

    pub fn service_unavailable(message: impl Into<String>) -> Self {
        EdgeError::ServiceUnavailable {
            message: message.into(),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            EdgeError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            EdgeError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            EdgeError::NotFound { .. } => StatusCode::NOT_FOUND,
            EdgeError::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            EdgeError::ServiceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            EdgeError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            EdgeError::BadRequest { message } => message.clone(),
            EdgeError::Validation { message } => message.clone(),
            EdgeError::NotFound { path } => format!("no route matched path: {path}"),
            EdgeError::MethodNotAllowed { method, allowed } => {
                format!("method {} not allowed; allowed: {}", method, allowed)
            }
            EdgeError::ServiceUnavailable { message } => message.clone(),
            EdgeError::Internal { source } => format!("internal error: {}", source),
        }
    }

    pub fn source(&self) -> Option<&AnyError> {
        match self {
            EdgeError::Internal { source } => Some(source),
            _ => None,
        }
    }
}

impl From<ConfigStoreError> for EdgeError {
    fn from(err: ConfigStoreError) -> Self {
        match err {
            ConfigStoreError::InvalidKey { message } => EdgeError::bad_request(message),
            ConfigStoreError::Unavailable { message } => EdgeError::service_unavailable(message),
            ConfigStoreError::Internal { source } => EdgeError::internal(source),
        }
    }
}

fn json_or_text<T: Serialize>(payload: &T) -> Body {
    Body::json(payload).unwrap_or_else(|_| Body::text("internal error"))
}

impl IntoResponse for EdgeError {
    fn into_response(self) -> Response {
        let payload = json!({
            "error": {
                "status": self.status().as_u16(),
                "message": self.message(),
            }
        });

        let body = json_or_text(&payload);
        let mut response = response_with_body(self.status(), body);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use serde::ser;

    #[test]
    fn bad_request_sets_status_and_message() {
        let err = EdgeError::bad_request("oops");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "oops");
    }

    #[test]
    fn method_not_allowed_lists_methods_sorted() {
        let err = EdgeError::method_not_allowed(&Method::POST, &[Method::GET, Method::DELETE]);
        assert_eq!(err.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(err.message().contains("allowed: DELETE, GET"));
    }

    #[test]
    fn internal_wraps_source_error() {
        let err = EdgeError::internal(anyhow::anyhow!("boom"));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message().contains("internal error: boom"));
        assert!(err.source().is_some());
    }

    #[test]
    fn not_found_sets_status_and_message() {
        let err = EdgeError::not_found("/missing");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert!(err.message().contains("/missing"));
    }

    #[test]
    fn validation_sets_status_and_message() {
        let err = EdgeError::validation("invalid input");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.message(), "invalid input");
        assert!(err.source().is_none());
    }

    #[test]
    fn method_not_allowed_handles_empty_allowed_list() {
        let err = EdgeError::method_not_allowed(&Method::GET, &[]);
        assert_eq!(err.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(err.message().contains("(none)"));
    }

    #[test]
    fn service_unavailable_sets_status_and_message() {
        let err = EdgeError::service_unavailable("config store unavailable");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "config store unavailable");
    }

    #[test]
    fn config_store_error_unavailable_maps_to_service_unavailable() {
        let err = EdgeError::from(ConfigStoreError::unavailable("backend offline"));
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "backend offline");
    }

    #[test]
    fn config_store_error_invalid_key_maps_to_bad_request() {
        let err = EdgeError::from(ConfigStoreError::invalid_key("invalid config key"));
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "invalid config key");
    }

    #[test]
    fn json_or_text_falls_back_on_serialization_error() {
        struct FailingSerialize;

        impl Serialize for FailingSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(ser::Error::custom("boom"))
            }
        }

        let body = json_or_text(&FailingSerialize);
        assert_eq!(body.as_bytes(), b"internal error");
    }

    #[test]
    fn into_response_sets_json_payload() {
        let response = EdgeError::bad_request("invalid").into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .expect("content-type header");
        assert_eq!(content_type, HeaderValue::from_static("application/json"));

        let body = response.into_body().into_bytes();
        assert!(std::str::from_utf8(body.as_ref())
            .unwrap()
            .contains("invalid"));
    }
}
