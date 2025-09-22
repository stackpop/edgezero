//! Core primitives for building portable edge workloads across edge providers.

mod app;
mod body;
mod compression;
mod context;
mod error;
mod extractor;
mod handler;
mod middleware;
mod params;
mod proxy;
mod responder;
mod response;
mod router;

pub use anyedge_macros::action;
pub use app::{App, Hooks};
pub use body::Body;
pub use compression::{decode_brotli_stream, decode_gzip_stream};
pub use context::RequestContext;
pub use error::EdgeError;
pub use extractor::{
    Form, FromRequest, Headers, Json, Path, Query, ValidatedForm, ValidatedJson, ValidatedPath,
    ValidatedQuery,
};
pub use handler::{BoxHandler, DynHandler, IntoHandler};
pub use middleware::{middleware_fn, BoxMiddleware, FnMiddleware, Middleware, Next, RequestLogger};
pub use params::PathParams;
pub use proxy::{ProxyClient, ProxyRequest, ProxyResponse, ProxyService};
pub use responder::Responder;
pub use response::{IntoResponse, Text};
pub use router::{RouterBuilder, RouterService};

pub use http::header;
pub use http::request::Builder as RequestBuilder;
pub use http::response::Builder as ResponseBuilder;

pub type Method = http::Method;
pub type StatusCode = http::StatusCode;
pub type HeaderMap = http::HeaderMap;
pub type HeaderValue = http::HeaderValue;
pub type HeaderName = http::header::HeaderName;
pub type Uri = http::Uri;
pub type Version = http::Version;
pub type Extensions = http::Extensions;

/// Construct a request builder using the crate's HTTP backend.
pub fn request_builder() -> RequestBuilder {
    http::Request::builder()
}

/// Construct a response builder using the crate's HTTP backend.
pub fn response_builder() -> ResponseBuilder {
    http::Response::builder()
}

use std::future::Future;
use std::pin::Pin;

use http::Request as HttpRequest;
use http::Response as HttpResponse;

/// Convenience alias for incoming HTTP requests handled by the framework.
pub type Request = HttpRequest<Body>;

/// Convenience alias for HTTP responses emitted by handlers and middleware.
pub type Response = HttpResponse<Body>;

/// Boxed future returned from handlers and middleware chains.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, EdgeError>> + 'static>>;
