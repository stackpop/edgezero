use std::future::Future;
use std::pin::Pin;

use http::request::Builder as HttpRequestBuilder;
use http::response::Builder as HttpResponseBuilder;

use crate::body::Body;
use crate::error::EdgeError;

// CLAUDE.md mandates that application code never imports from the `http`
// crate directly — every HTTP type must come through `edgezero_core::http`.
// `Builder` types are exposed via `pub type` aliases (not `pub use`) so
// only the `header` re-export remains, scoped to its own child module.
pub type RequestBuilder = HttpRequestBuilder;
pub type ResponseBuilder = HttpResponseBuilder;

/// Re-exports of [`http::header`] used by adapters and handlers.
pub mod header {
    #![expect(
        clippy::pub_use,
        reason = "header constants/types must be re-exported through this module to satisfy the \
                  CLAUDE.md `edgezero_core::http` facade rule; downstream code must not depend on \
                  the `http` crate directly"
    )]
    pub use http::header::*;
}

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
