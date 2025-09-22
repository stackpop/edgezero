use std::future::Future;
use std::sync::Arc;

use crate::{EdgeError, HandlerFuture, IntoResponse, RequestContext};

pub trait DynHandler: Send + Sync {
    fn call(&self, ctx: RequestContext) -> HandlerFuture;
}

impl<F, Fut, Res> DynHandler for F
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Res, EdgeError>> + 'static,
    Res: IntoResponse,
{
    fn call(&self, ctx: RequestContext) -> HandlerFuture {
        let fut = (self)(ctx);
        Box::pin(async move {
            let response = fut.await?.into_response();
            Ok(response)
        })
    }
}

pub type BoxHandler = Arc<dyn DynHandler>;

pub trait IntoHandler {
    fn into_handler(self) -> BoxHandler;
}

impl<H> IntoHandler for H
where
    H: DynHandler + Sized + 'static,
{
    fn into_handler(self) -> BoxHandler {
        Arc::new(self)
    }
}
