extern crate self as anyedge_controller;

pub mod extract;
pub mod format;
pub mod responder;

mod error;
mod handler;
mod hooks;
mod request;
mod route_set;

pub use anyedge_macros::action;

pub use error::ControllerError;
#[cfg(feature = "form")]
pub use extract::Form as RequestForm;
pub use extract::{FromRequest, Path, Query, RawRequest, State, ValidatedPath, ValidatedQuery};
#[cfg(feature = "json")]
pub use extract::{Json as RequestJson, ValidatedJson};
pub use format::*;
pub use handler::{controller_handler, ControllerHandler, IntoHandler};
pub use hooks::{AppRoutes, Hooks};
pub use request::RequestParts;
pub use route_set::{
    delete, get, head, options, patch, post, put, route_with, RouteSet, RouteSet as Routes,
    RouteSpec,
};

#[cfg(feature = "json")]
pub use responder::Json;
pub use responder::{respond, Responder, Text};
