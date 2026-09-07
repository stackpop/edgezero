use anyhow::Error as AnyError;
use serde::Serialize;
use serde_json::json;
use serde_path_to_error::Error as SerdePathError;
use thiserror::Error;

use crate::body::Body;
use crate::config_store::ConfigStoreError;
use crate::http::{
    HeaderValue, Method, Response, StatusCode,
    header::{CONTENT_TYPE, RETRY_AFTER},
};
use crate::response::{IntoResponse, response_with_body};

/// Stable classification for upstream failures that map to HTTP 502.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadGatewayReason {
    Decode,
    Protocol,
    Transport,
    Unspecified,
}

/// Configured budget input that selected an effective deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetSource {
    BatchDeadline,
    Default,
    PerCallTimeout,
    Unspecified,
}

/// Application-level error that carries an HTTP status code.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EdgeError {
    /// Upstream or transport failure. HTTP 502.
    #[error("{message}")]
    BadGateway {
        message: String,
        reason: BadGatewayReason,
    },
    #[error("{message}")]
    BadRequest { message: String },
    /// The blob's `data` shape disagrees with the deployed `C`
    /// type. Re-running `<app-cli> config push` for the deployed
    /// code revision fixes it. HTTP 503, kind
    /// `"config_out_of_date"`, carries `Retry-After: 60`.
    #[error("config out of date: {message}")]
    ConfigOutOfDate { message: String, field_path: String },
    /// A wall-clock deadline or per-request timeout fired. HTTP 504.
    #[error("{message}")]
    GatewayTimeout {
        message: String,
        cause: BudgetSource,
    },
    #[error("internal error: {source}")]
    Internal {
        #[from]
        source: AnyError,
    },
    #[error("method {method} not allowed; allowed: {allowed}")]
    MethodNotAllowed { method: Method, allowed: String },
    #[error("no route matched path: {path}")]
    NotFound { path: String },
    #[error("not implemented: {message}")]
    NotImplemented { message: String },
    #[error("service unavailable: {message}")]
    ServiceUnavailable { message: String },
    #[error("validation error: {message}")]
    Validation { message: String },
}

impl EdgeError {
    #[inline]
    pub fn bad_gateway<S: Into<String>>(message: S) -> Self {
        EdgeError::BadGateway {
            message: message.into(),
            reason: BadGatewayReason::Unspecified,
        }
    }

    #[inline]
    pub fn bad_gateway_with_reason<S: Into<String>>(message: S, reason: BadGatewayReason) -> Self {
        EdgeError::BadGateway {
            message: message.into(),
            reason,
        }
    }

    #[inline]
    pub fn bad_request<S: Into<String>>(message: S) -> Self {
        EdgeError::BadRequest {
            message: message.into(),
        }
    }

    /// Construct from an explicit `(message, field_path)` pair.
    /// Used by the secret walk and validator paths. `field_path`
    /// SHOULD be a dotted path naming the offending field; pass
    /// `String::new()` when no specific field is anchored.
    #[must_use]
    #[inline]
    pub fn config_out_of_date<Msg: Into<String>, Path: Into<String>>(
        message: Msg,
        field_path: Path,
    ) -> Self {
        Self::ConfigOutOfDate {
            message: message.into(),
            field_path: field_path.into(),
        }
    }

    /// Construct from a `serde_path_to_error` error returned by
    /// the deserialise wrapper around the blob's `data` field.
    #[must_use]
    #[inline]
    pub fn config_out_of_date_from_serde(serde_err: &SerdePathError<serde_json::Error>) -> Self {
        // The serde message embeds the offending stored VALUE (e.g. `invalid
        // type: string "hunter2", expected u32`), and this message is serialised
        // into the HTTP error body. The config blob may hold secrets, so the
        // VALUE must not escape — report only the category.
        //
        // The `field_path` STRING segments are redacted (structure kept): a map
        // key is indistinguishable from a struct field here and may be a secret,
        // and the redaction invariant forbids a stored string on any path. The
        // exact path is available from a local `config validate`. See
        // `redact_serde_path`.
        use serde_json::error::Category;
        let category = match serde_err.inner().classify() {
            Category::Data => "wrong type or invalid value",
            Category::Syntax => "malformed JSON",
            Category::Eof => "unexpected end of input",
            Category::Io => "i/o error while reading",
        };
        Self::ConfigOutOfDate {
            message: format!(
                "typed app-config is out of date ({category}; value redacted) — \
                 run `<app-cli> config push` for this deploy"
            ),
            field_path: redact_serde_path(serde_err.path()),
        }
    }

    #[inline]
    pub fn gateway_timeout<S: Into<String>>(message: S) -> Self {
        EdgeError::GatewayTimeout {
            message: message.into(),
            cause: BudgetSource::Unspecified,
        }
    }

    #[inline]
    pub fn gateway_timeout_caused<S: Into<String>>(message: S, cause: BudgetSource) -> Self {
        EdgeError::GatewayTimeout {
            message: message.into(),
            cause,
        }
    }

    /// Typed access to the wrapped [`AnyError`] for `EdgeError::Internal`.
    ///
    /// Renamed away from `source` to avoid shadowing
    /// [`std::error::Error::source`] (auto-derived by `thiserror`). The
    /// trait method returns a `&dyn Error`; this one returns the concrete
    /// `&anyhow::Error` so callers can downcast.
    #[must_use]
    #[inline]
    pub fn inner(&self) -> Option<&AnyError> {
        match self {
            EdgeError::Internal { source } => Some(source),
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::ConfigOutOfDate { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::Validation { .. }
            | EdgeError::ServiceUnavailable { .. } => None,
        }
    }

    #[inline]
    pub fn internal<E>(error: E) -> Self
    where
        E: Into<AnyError>,
    {
        EdgeError::Internal {
            source: error.into(),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            EdgeError::BadGateway { .. } => "bad_gateway",
            EdgeError::BadRequest { .. } => "bad_request",
            EdgeError::ConfigOutOfDate { .. } => "config_out_of_date",
            EdgeError::GatewayTimeout { .. } => "gateway_timeout",
            EdgeError::Internal { .. } => "internal",
            EdgeError::MethodNotAllowed { .. } => "method_not_allowed",
            EdgeError::NotFound { .. } => "not_found",
            EdgeError::NotImplemented { .. } => "not_implemented",
            EdgeError::ServiceUnavailable { .. } => "service_unavailable",
            EdgeError::Validation { .. } => "validation",
        }
    }

    #[must_use]
    #[inline]
    pub fn message(&self) -> String {
        match self {
            EdgeError::BadGateway { message, .. }
            | EdgeError::BadRequest { message }
            | EdgeError::ConfigOutOfDate { message, .. }
            | EdgeError::GatewayTimeout { message, .. }
            | EdgeError::Validation { message }
            | EdgeError::NotImplemented { message }
            | EdgeError::ServiceUnavailable { message } => message.clone(),
            EdgeError::NotFound { path } => format!("no route matched path: {path}"),
            EdgeError::MethodNotAllowed { method, allowed } => {
                format!("method {method} not allowed; allowed: {allowed}")
            }
            EdgeError::Internal { source } => format!("internal error: {source}"),
        }
    }

    #[must_use]
    #[inline]
    pub fn method_not_allowed(method: &Method, allowed: &[Method]) -> Self {
        let mut names = allowed
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        let allowed_list = if names.is_empty() {
            "(none)".to_owned()
        } else {
            names.join(", ")
        };
        EdgeError::MethodNotAllowed {
            method: method.clone(),
            allowed: allowed_list,
        }
    }

    #[inline]
    pub fn not_found<S: Into<String>>(path: S) -> Self {
        EdgeError::NotFound { path: path.into() }
    }

    #[inline]
    pub fn not_implemented<S: Into<String>>(message: S) -> Self {
        EdgeError::NotImplemented {
            message: message.into(),
        }
    }

    #[inline]
    pub fn service_unavailable<S: Into<String>>(message: S) -> Self {
        EdgeError::ServiceUnavailable {
            message: message.into(),
        }
    }

    #[must_use]
    #[inline]
    pub fn status(&self) -> StatusCode {
        match self {
            EdgeError::BadGateway { .. } => StatusCode::BAD_GATEWAY,
            EdgeError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            EdgeError::ConfigOutOfDate { .. } | EdgeError::ServiceUnavailable { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            EdgeError::GatewayTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            EdgeError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            EdgeError::NotFound { .. } => StatusCode::NOT_FOUND,
            EdgeError::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            EdgeError::NotImplemented { .. } => StatusCode::NOT_IMPLEMENTED,
            EdgeError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    #[inline]
    pub fn validation<S: Into<String>>(message: S) -> Self {
        EdgeError::Validation {
            message: message.into(),
        }
    }
}

impl From<ConfigStoreError> for EdgeError {
    #[inline]
    fn from(err: ConfigStoreError) -> Self {
        match err {
            ConfigStoreError::InvalidKey { message } => EdgeError::bad_request(message),
            ConfigStoreError::Unavailable { message } => EdgeError::service_unavailable(message),
            ConfigStoreError::Internal { source } => EdgeError::internal(source),
        }
    }
}

impl IntoResponse for EdgeError {
    #[inline]
    fn into_response(self) -> Result<Response, EdgeError> {
        let kind = self.kind_str();
        let is_config_out_of_date = matches!(self, EdgeError::ConfigOutOfDate { .. });
        // `ConfigOutOfDate { field_path: String::new(), .. }` (the missing-blob
        // path) must OMIT the `field_path` JSON key entirely, not emit
        // `"field_path": ""`. Per spec 6.3.1.
        let field_path_opt: Option<&str> = match &self {
            EdgeError::ConfigOutOfDate { field_path, .. } if !field_path.is_empty() => {
                Some(field_path.as_str())
            }
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::ConfigOutOfDate { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::Internal { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::Validation { .. } => None,
        };
        let status = self.status();
        let message = self.message();

        let mut error_obj = serde_json::Map::new();
        error_obj.insert("status".into(), serde_json::Value::from(status.as_u16()));
        error_obj.insert("kind".into(), serde_json::Value::from(kind));
        error_obj.insert("message".into(), serde_json::Value::from(message));
        if let Some(field_path) = field_path_opt {
            error_obj.insert("field_path".into(), serde_json::Value::from(field_path));
        }
        let payload = json!({ "error": serde_json::Value::Object(error_obj) });

        let body = json_or_text(&payload);
        let mut response = response_with_body(status, body)?;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if is_config_out_of_date {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("60"));
        }
        Ok(response)
    }
}

/// Render a `serde_path_to_error` path with its STRING segments redacted while
/// preserving structure (dots and sequence indices).
///
/// A struct field and a map KEY are the same `Segment::Map` in this API
/// (verified empirically), so they cannot be told apart. A map key is stored
/// config DATA and may be a secret, and this path is serialised into the HTTP
/// error body — which the redaction invariant forbids for any stored string. So
/// every string segment becomes `<redacted>`; sequence indices (positions, not
/// values) are kept. The operator recovers the EXACT path by running
/// `config validate` locally, which deserializes the same blob without an HTTP
/// boundary. See the `field_path` note in spec 2026-06-16.
fn redact_serde_path(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Seq { index } => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
            Segment::Map { .. } | Segment::Enum { .. } => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str("<redacted>");
            }
            Segment::Unknown => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push('?');
            }
        }
    }
    // No segments => a root-level error; keep serde's "." sentinel (a marker,
    // not data).
    if out.is_empty() {
        return path.to_string();
    }
    out
}

fn json_or_text<T: Serialize>(payload: &T) -> Body {
    Body::json(payload).unwrap_or_else(|_| Body::text("internal error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use serde::ser;
    use std::str;

    #[test]
    fn bad_gateway_and_gateway_timeout_surface() {
        for (err, code, msg) in [
            (
                EdgeError::bad_gateway("upstream refused"),
                StatusCode::BAD_GATEWAY,
                "upstream refused",
            ),
            (
                EdgeError::gateway_timeout("deadline expired"),
                StatusCode::GATEWAY_TIMEOUT,
                "deadline expired",
            ),
        ] {
            assert_eq!(err.status(), code);
            assert_eq!(err.message(), msg);
            assert!(err.inner().is_none());
            assert!(err.to_string().contains(msg));
        }
    }

    #[test]
    fn bad_gateway_and_gateway_timeout_json_shape() {
        for (err, code, kind, msg) in [
            (
                EdgeError::bad_gateway("nope"),
                502_u16,
                "bad_gateway",
                "nope",
            ),
            (
                EdgeError::bad_gateway_with_reason("nope", BadGatewayReason::Decode),
                502_u16,
                "bad_gateway",
                "nope",
            ),
            (
                EdgeError::bad_gateway_with_reason("nope", BadGatewayReason::Protocol),
                502_u16,
                "bad_gateway",
                "nope",
            ),
            (
                EdgeError::bad_gateway_with_reason("nope", BadGatewayReason::Transport),
                502_u16,
                "bad_gateway",
                "nope",
            ),
            (
                EdgeError::gateway_timeout("late"),
                504_u16,
                "gateway_timeout",
                "late",
            ),
            (
                EdgeError::gateway_timeout_caused("late", BudgetSource::PerCallTimeout),
                504_u16,
                "gateway_timeout",
                "late",
            ),
            (
                EdgeError::gateway_timeout_caused("late", BudgetSource::BatchDeadline),
                504_u16,
                "gateway_timeout",
                "late",
            ),
            (
                EdgeError::gateway_timeout_caused("late", BudgetSource::Default),
                504_u16,
                "gateway_timeout",
                "late",
            ),
        ] {
            let response = err.into_response().expect("response");
            assert_eq!(response.status().as_u16(), code);
            let body_json = parse_body(response);
            assert_eq!(body_json["error"]["status"], code);
            assert_eq!(body_json["error"]["kind"], serde_json::Value::from(kind));
            assert_eq!(body_json["error"]["message"], serde_json::Value::from(msg));
            assert!(
                body_json["error"].get("field_path").is_none(),
                "502/504 carry no field_path"
            );
            assert!(
                body_json["error"].get("reason").is_none(),
                "reason is not part of the wire shape"
            );
            assert!(
                body_json["error"].get("cause").is_none(),
                "cause is not part of the wire shape"
            );
        }
    }

    #[test]
    fn bad_gateway_reason_is_typed() {
        let EdgeError::BadGateway {
            reason: default_reason,
            ..
        } = EdgeError::bad_gateway("x")
        else {
            panic!("expected BadGateway");
        };
        assert_eq!(default_reason, BadGatewayReason::Unspecified);

        for expected in [
            BadGatewayReason::Decode,
            BadGatewayReason::Protocol,
            BadGatewayReason::Transport,
            BadGatewayReason::Unspecified,
        ] {
            let EdgeError::BadGateway { reason, .. } =
                EdgeError::bad_gateway_with_reason("x", expected)
            else {
                panic!("expected BadGateway");
            };
            assert_eq!(reason, expected);
        }
    }

    #[test]
    fn bad_request_sets_status_and_message() {
        let err = EdgeError::bad_request("oops");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "oops");
    }

    #[test]
    fn bare_gateway_timeout_is_unspecified() {
        let EdgeError::GatewayTimeout { cause, .. } = EdgeError::gateway_timeout("x") else {
            panic!("expected GatewayTimeout");
        };
        assert_eq!(cause, BudgetSource::Unspecified);
    }

    #[test]
    fn config_out_of_date_constructor_round_trips() {
        let err = EdgeError::config_out_of_date("missing field", "feature.new_checkout");
        match err {
            EdgeError::ConfigOutOfDate {
                message,
                field_path,
            } => {
                assert_eq!(message, "missing field");
                assert_eq!(field_path, "feature.new_checkout");
            }
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::Internal { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::Validation { .. } => panic!("expected ConfigOutOfDate"),
        }
    }

    #[test]
    fn config_out_of_date_sets_status_and_message() {
        let err = EdgeError::config_out_of_date("schema mismatch", "service.timeout_ms");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "schema mismatch");
        assert!(err.inner().is_none());
    }

    #[test]
    fn config_out_of_date_from_serde_extracts_path_and_message() {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        struct Outer {
            #[expect(dead_code, reason = "only used to drive deserialization")]
            service: Inner,
        }

        #[derive(Debug, Deserialize)]
        struct Inner {
            #[expect(dead_code, reason = "only used to drive deserialization")]
            timeout_ms: u32,
        }

        // Feed JSON that puts a string where u32 is expected to force a
        // type-mismatch error at path `service.timeout_ms`.
        let json = r#"{"service": {"timeout_ms": "not-a-number"}}"#;
        let de = &mut serde_json::Deserializer::from_str(json);
        let result: Result<Outer, _> = serde_path_to_error::deserialize(de);
        let serde_err = result.expect_err("expected deserialization error");

        let err = EdgeError::config_out_of_date_from_serde(&serde_err);

        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!err.message().is_empty());
        match err {
            EdgeError::ConfigOutOfDate { field_path, .. } => {
                // String segments redacted, structure preserved.
                assert_eq!(field_path, "<redacted>.<redacted>");
            }
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::Internal { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::Validation { .. } => panic!("expected ConfigOutOfDate"),
        }
    }

    #[test]
    fn config_out_of_date_from_serde_redacts_map_key_from_path_and_message() {
        use serde::Deserialize;
        use std::collections::BTreeMap;

        #[derive(Debug, Deserialize)]
        struct Outer {
            #[expect(dead_code, reason = "only used to drive deserialization")]
            items: BTreeMap<String, Inner>,
        }
        #[derive(Debug, Deserialize)]
        struct Inner {
            #[expect(dead_code, reason = "only used to drive deserialization")]
            port: u32,
        }

        // A MAP KEY holding a secret. The type error is under that key, so the
        // serde path is `items.SECRET_MAP_KEY.port`. It must reach NEITHER the
        // message NOR the field_path: a map key is indistinguishable from a struct
        // field in the serde path, so every string segment is redacted (structure
        // kept).
        const SENTINEL: &str = "SUPER_SECRET_MAP_KEY";
        let json = format!(r#"{{"items": {{"{SENTINEL}": {{"port": "nope"}}}}}}"#);
        let de = &mut serde_json::Deserializer::from_str(&json);
        let result: Result<Outer, _> = serde_path_to_error::deserialize(de);
        let serde_err = result.expect_err("expected deserialization error");

        let err = EdgeError::config_out_of_date_from_serde(&serde_err);
        match err {
            EdgeError::ConfigOutOfDate {
                field_path,
                message,
            } => {
                assert!(
                    !message.contains(SENTINEL),
                    "a stored map key must never reach the message: {message}"
                );
                assert!(
                    !field_path.contains(SENTINEL),
                    "a stored map key must never reach the field_path: {field_path}"
                );
                // Structure is preserved (items.<key>.port -> redacted, dotted).
                assert_eq!(field_path, "<redacted>.<redacted>.<redacted>");
            }
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::Internal { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::Validation { .. } => panic!("expected ConfigOutOfDate"),
        }
    }

    #[test]
    fn config_out_of_date_from_serde_root_error_passes_through_sentinel() {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        struct Root {
            #[expect(dead_code, reason = "only used to drive deserialization")]
            value: u32,
        }

        // A completely invalid JSON causes a root-level error. serde_path_to_error
        // returns "." as the path sentinel in this case.
        let json = r#""not-an-object""#;
        let de = &mut serde_json::Deserializer::from_str(json);
        let result: Result<Root, _> = serde_path_to_error::deserialize(de);
        let serde_err = result.expect_err("expected deserialization error");

        let expected_path = serde_err.path().to_string();
        let err = EdgeError::config_out_of_date_from_serde(&serde_err);
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        match err {
            EdgeError::ConfigOutOfDate { field_path, .. } => {
                // The from_serde constructor passes the library's path through
                // verbatim; for root-level errors that is ".".
                assert_eq!(
                    field_path, expected_path,
                    "field_path should match serde_path_to_error sentinel"
                );
            }
            EdgeError::BadGateway { .. }
            | EdgeError::BadRequest { .. }
            | EdgeError::GatewayTimeout { .. }
            | EdgeError::Internal { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::Validation { .. } => panic!("expected ConfigOutOfDate"),
        }
    }

    #[test]
    fn config_store_error_internal_maps_to_internal_server_error() {
        let err = EdgeError::from(ConfigStoreError::internal(anyhow::anyhow!("boom")));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message().contains("boom"));
    }

    #[test]
    fn config_store_error_invalid_key_maps_to_bad_request() {
        let err = EdgeError::from(ConfigStoreError::invalid_key("invalid config key"));
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "invalid config key");
    }

    #[test]
    fn config_store_error_unavailable_maps_to_service_unavailable() {
        let err = EdgeError::from(ConfigStoreError::unavailable("backend offline"));
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "backend offline");
    }

    #[test]
    fn gateway_timeout_caused_preserves_cause() {
        for expected in [
            BudgetSource::BatchDeadline,
            BudgetSource::Default,
            BudgetSource::PerCallTimeout,
            BudgetSource::Unspecified,
        ] {
            let EdgeError::GatewayTimeout { cause, .. } =
                EdgeError::gateway_timeout_caused("x", expected)
            else {
                panic!("expected GatewayTimeout");
            };
            assert_eq!(cause, expected);
        }
    }

    #[test]
    fn internal_wraps_source_error() {
        let err = EdgeError::internal(anyhow::anyhow!("boom"));
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(err.message().contains("internal error: boom"));
        assert!(err.inner().is_some());
    }

    #[test]
    fn into_response_sets_json_payload() {
        let response = EdgeError::bad_request("invalid")
            .into_response()
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .expect("content-type header");
        assert_eq!(content_type, HeaderValue::from_static("application/json"));

        let body = response.into_body().into_bytes().expect("buffered");
        let body_str = str::from_utf8(body.as_ref()).unwrap();
        assert!(body_str.contains("invalid"), "body should contain message");
        assert!(
            body_str.contains("\"kind\""),
            "body should contain kind field"
        );
        assert!(
            body_str.contains("\"bad_request\""),
            "kind should be bad_request"
        );
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
        assert_eq!(body.as_bytes().expect("buffered"), b"internal error");
    }

    #[test]
    fn method_not_allowed_handles_empty_allowed_list() {
        let err = EdgeError::method_not_allowed(&Method::GET, &[]);
        assert_eq!(err.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(err.message().contains("(none)"));
    }

    #[test]
    fn method_not_allowed_lists_methods_sorted() {
        let err = EdgeError::method_not_allowed(&Method::POST, &[Method::GET, Method::DELETE]);
        assert_eq!(err.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(err.message().contains("allowed: DELETE, GET"));
    }

    #[test]
    fn not_found_sets_status_and_message() {
        let err = EdgeError::not_found("/missing");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert!(err.message().contains("/missing"));
    }

    #[test]
    fn service_unavailable_sets_status_and_message() {
        let err = EdgeError::service_unavailable("config store unavailable");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "config store unavailable");
    }

    #[test]
    fn validation_sets_status_and_message() {
        let err = EdgeError::validation("invalid input");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.message(), "invalid input");
        assert!(err.inner().is_none());
    }

    fn parse_body(response: Response) -> serde_json::Value {
        use std::str;
        let bytes = response.into_body().into_bytes().expect("buffered body");
        let text = str::from_utf8(bytes.as_ref()).expect("utf-8 body");
        serde_json::from_str(text).expect("json body")
    }

    #[test]
    fn kind_strings_per_variant() {
        macro_rules! assert_kind {
            ($err:expr, $expected_kind:literal, $expected_status:literal) => {{
                let response = $err.into_response().expect("response");
                assert_eq!(
                    response.status().as_u16(),
                    $expected_status,
                    "status mismatch for kind {}",
                    $expected_kind
                );
                let body = parse_body(response);
                assert_eq!(
                    body["error"]["kind"],
                    serde_json::Value::from($expected_kind),
                    "kind mismatch"
                );
            }};
        }

        assert_kind!(EdgeError::bad_gateway("x"), "bad_gateway", 502_u16);
        assert_kind!(EdgeError::bad_request("x"), "bad_request", 400_u16);
        assert_kind!(
            EdgeError::config_out_of_date("x", "f"),
            "config_out_of_date",
            503_u16
        );
        assert_kind!(
            EdgeError::internal(anyhow::anyhow!("x")),
            "internal",
            500_u16
        );
        assert_kind!(
            EdgeError::method_not_allowed(&Method::GET, &[]),
            "method_not_allowed",
            405_u16
        );
        assert_kind!(EdgeError::not_found("/x"), "not_found", 404_u16);
        assert_kind!(EdgeError::not_implemented("x"), "not_implemented", 501_u16);
        assert_kind!(EdgeError::gateway_timeout("x"), "gateway_timeout", 504_u16);
        assert_kind!(
            EdgeError::service_unavailable("x"),
            "service_unavailable",
            503_u16
        );
        assert_kind!(EdgeError::validation("x"), "validation", 422_u16);
    }

    #[test]
    fn retry_after_only_on_config_out_of_date() {
        macro_rules! assert_retry_after {
            ($err:expr, $expected:literal) => {{
                let response = $err.into_response().expect("response");
                let header = response.headers().get(RETRY_AFTER);
                if $expected {
                    assert_eq!(header.expect("Retry-After header").to_str().unwrap(), "60");
                } else {
                    assert!(header.is_none(), "unexpected Retry-After header on variant");
                }
            }};
        }

        assert_retry_after!(EdgeError::bad_gateway("x"), false);
        assert_retry_after!(EdgeError::bad_request("x"), false);
        assert_retry_after!(EdgeError::gateway_timeout("x"), false);
        assert_retry_after!(EdgeError::internal(anyhow::anyhow!("x")), false);
        // ServiceUnavailable is also 503 but must NOT carry Retry-After
        assert_retry_after!(EdgeError::service_unavailable("x"), false);
        assert_retry_after!(EdgeError::config_out_of_date("x", "f"), true);
    }

    #[test]
    fn field_path_only_on_config_out_of_date() {
        for err in [EdgeError::bad_gateway("x"), EdgeError::gateway_timeout("x")] {
            let body = parse_body(err.into_response().expect("response"));
            assert!(
                body["error"].get("field_path").is_none(),
                "field_path should be absent for gateway errors"
            );
        }

        let bad_req_err = EdgeError::bad_request("x");
        let bad_req_body = parse_body(bad_req_err.into_response().expect("response"));
        assert!(
            bad_req_body["error"].get("field_path").is_none(),
            "field_path should be absent for BadRequest"
        );

        let cod_err = EdgeError::config_out_of_date("x", "feature.new_checkout");
        let cod_body = parse_body(cod_err.into_response().expect("response"));
        assert_eq!(
            cod_body["error"]["field_path"],
            serde_json::Value::from("feature.new_checkout")
        );

        // Phase-B-review Important: empty `field_path` must OMIT the
        // key entirely, not emit `"field_path": ""`. The missing-blob path
        // constructs `ConfigOutOfDate { field_path: String::new(), .. }`
        // and the spec 6.3.1 response shape says omit when no field
        // is anchored.
        let empty_cod_err = EdgeError::config_out_of_date("no remote blob", "");
        let empty_cod_body = parse_body(empty_cod_err.into_response().expect("response"));
        assert!(
            empty_cod_body["error"].get("field_path").is_none(),
            "field_path key must be absent (not empty string) when ConfigOutOfDate has no anchor: {empty_cod_body}",
        );
        assert_eq!(empty_cod_body["error"]["kind"], "config_out_of_date");
    }
}
