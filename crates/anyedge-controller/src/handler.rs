use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyedge_core::{
    handler::{Handler, HandlerFuture, IntoHandlerFuture},
    Request, Response,
};

type ControllerFuture = Pin<Box<dyn Future<Output = Response> + Send + 'static>>;
type ControllerFn = dyn Fn(Request) -> ControllerFuture + Send + Sync + 'static;

#[derive(Clone)]
pub struct ControllerHandler {
    inner: Arc<ControllerFn>,
}

impl ControllerHandler {
    pub fn new<F, O>(f: F) -> Self
    where
        F: Fn(Request) -> O + Send + Sync + 'static,
        O: IntoHandlerFuture + 'static,
    {
        Self {
            inner: Arc::new(move |req| (f(req)).into_handler_future()),
        }
    }

    pub fn from_fn<F, O>(f: F) -> Self
    where
        F: Fn(Request) -> O + Send + Sync + 'static,
        O: IntoHandlerFuture + 'static,
    {
        Self::new(f)
    }

    pub fn call(&self, req: Request) -> ControllerFuture {
        (self.inner)(req)
    }
}

impl Handler for ControllerHandler {
    fn call<'a>(&'a self, req: Request) -> HandlerFuture<'a> {
        (self.inner)(req)
    }
}

pub trait IntoHandler {
    fn into_handler(self) -> ControllerHandler;
}

impl IntoHandler for ControllerHandler {
    fn into_handler(self) -> ControllerHandler {
        self
    }
}

impl<F, O> IntoHandler for F
where
    F: Fn(Request) -> O + Send + Sync + 'static,
    O: IntoHandlerFuture + 'static,
{
    fn into_handler(self) -> ControllerHandler {
        ControllerHandler::from_fn(self)
    }
}

pub fn controller_handler<H>(handler: H) -> ControllerHandler
where
    H: IntoHandler,
{
    handler.into_handler()
}
