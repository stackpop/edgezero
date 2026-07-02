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
