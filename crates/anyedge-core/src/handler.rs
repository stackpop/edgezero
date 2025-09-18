use crate::http::{Request, Response};
use std::future::Future;
use std::pin::Pin;

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

pub trait IntoHandlerFuture {
    fn into_handler_future(self) -> HandlerFuture<'static>;
}

impl IntoHandlerFuture for Response {
    fn into_handler_future(self) -> HandlerFuture<'static> {
        Box::pin(async move { self })
    }
}

impl<Fut> IntoHandlerFuture for Fut
where
    Fut: Future<Output = Response> + Send + 'static,
{
    fn into_handler_future(self) -> HandlerFuture<'static> {
        Box::pin(self)
    }
}

pub trait Handler: Send + Sync + 'static {
    fn call<'a>(&'a self, req: Request) -> HandlerFuture<'a>;
}

impl<F, O> Handler for F
where
    F: Fn(Request) -> O + Send + Sync + 'static,
    O: IntoHandlerFuture + 'static,
{
    fn call<'a>(&'a self, req: Request) -> HandlerFuture<'a> {
        (self)(req).into_handler_future()
    }
}

pub type BoxHandler = Box<dyn Handler + Send + Sync>;
