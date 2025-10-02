use std::future::Future;
use std::pin::Pin;

use crate::body::Body;
use crate::error::EdgeError;

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

pub fn request_builder() -> RequestBuilder {
    http::Request::builder()
}

pub fn response_builder() -> ResponseBuilder {
    http::Response::builder()
}

pub type Request = http::Request<Body>;
pub type Response = http::Response<Body>;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, EdgeError>> + 'static>>;
