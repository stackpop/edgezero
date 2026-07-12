use std::future::Future;
use std::sync::Arc;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::HandlerFuture;
use crate::response::IntoResponse;

/// Which introspection payloads a route's handler needs injected at dispatch.
///
/// Reported per handler via [`DynHandler::introspection_needs`]. Handlers written
/// with `#[action(manifest)]` / `#[action(routes)]` set the matching field(s);
/// every other handler reports the default (all-false).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntrospectionNeeds {
    pub manifest: bool,
    pub routes: bool,
}

impl IntrospectionNeeds {
    /// Whether this handler needs any introspection payload injected.
    #[must_use]
    #[inline]
    pub fn any(self) -> bool {
        self.manifest || self.routes
    }
}

pub trait DynHandler: Send + Sync {
    fn call(&self, ctx: RequestContext) -> HandlerFuture;

    /// Introspection payloads a route bound to this handler needs injected into
    /// the request at dispatch. Defaults to none; `#[action(manifest)]` /
    /// `#[action(routes)]` handlers override it.
    #[inline]
    fn introspection_needs(&self) -> IntrospectionNeeds {
        IntrospectionNeeds::default()
    }
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

    // `missing_trait_methods` (deny) forbids relying on the trait default here;
    // spell out the same all-false result that fn/closure handlers report.
    #[inline]
    fn introspection_needs(&self) -> IntrospectionNeeds {
        IntrospectionNeeds::default()
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
        // Prove the closure's return value actually flowed through
        // `into_response` — not just that *some* default-OK response came
        // back. A bridge that dropped the body would still be status 200.
        assert_eq!(response.body().as_bytes(), Some(&b"hi"[..]));
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

    #[test]
    fn fn_handler_reports_default_introspection_needs() {
        // A plain closure handler uses the blanket `DynHandler` impl, which must
        // report no introspection needs. (`&str: IntoResponse` satisfies the bound.)
        let handler = |_ctx: RequestContext| async { Ok::<&'static str, EdgeError>("ok") };
        assert_eq!(
            DynHandler::introspection_needs(&handler),
            IntrospectionNeeds::default()
        );
        assert!(!IntrospectionNeeds::default().any());
    }
}
