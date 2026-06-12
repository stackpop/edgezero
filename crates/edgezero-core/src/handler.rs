use std::future::Future;
use std::sync::Arc;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::HandlerFuture;
use crate::response::IntoResponse;

pub trait DynHandler: Send + Sync {
    fn call(&self, ctx: RequestContext) -> HandlerFuture;
}

impl<F, Fut, Res> DynHandler for F
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Res, EdgeError>> + 'static,
    Res: IntoResponse,
{
    #[inline]
    fn call(&self, ctx: RequestContext) -> HandlerFuture {
        let fut = (self)(ctx);
        Box::pin(async move { fut.await?.into_response() })
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
    #[inline]
    fn into_handler(self) -> BoxHandler {
        Arc::new(self)
    }
}
