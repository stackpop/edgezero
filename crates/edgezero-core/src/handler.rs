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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::{request_builder, Method, StatusCode};
    use crate::params::PathParams;
    use futures::executor::block_on;

    fn ctx() -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    #[test]
    fn into_handler_wraps_closure_and_call_runs_it() {
        async fn ok(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            Ok("hi")
        }
        let handler = ok.into_handler();
        let response = block_on(handler.call(ctx())).expect("ok response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn call_propagates_handler_error() {
        async fn boom(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            // `EdgeError::internal` takes `E: Into<anyhow::Error>`; a bare
            // `&str` does not satisfy that bound, so wrap with `anyhow!`.
            Err(EdgeError::internal(anyhow::anyhow!("boom")))
        }
        let handler = boom.into_handler();
        let Err(error) = block_on(handler.call(ctx())) else {
            panic!("expected error");
        };
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
