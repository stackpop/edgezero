use std::future::Future;
use std::pin::Pin;

use crate::body::Body;
use crate::error::EdgeError;

// CLAUDE.md mandates that application code never imports from the `http`
// crate directly — every HTTP type must come through `edgezero_core::http`.
// That contract is what these re-exports exist for.
pub use http::header;
pub use http::request::Builder as RequestBuilder;
pub use http::response::Builder as ResponseBuilder;

pub type Method = http::Method;
pub type StatusCode = http::StatusCode;
pub type HeaderMap = http::HeaderMap;
pub type HeaderValue = http::HeaderValue;
pub type HeaderName = header::HeaderName;
pub type Uri = http::Uri;
pub type Version = http::Version;
pub type Extensions = http::Extensions;

#[must_use]
pub fn request_builder() -> RequestBuilder {
    http::Request::builder()
}

#[must_use]
pub fn response_builder() -> ResponseBuilder {
    http::Response::builder()
}

pub type Request = http::Request<Body>;
pub type Response = http::Response<Body>;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, EdgeError>> + 'static>>;
