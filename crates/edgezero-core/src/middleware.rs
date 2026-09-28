use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use web_time::Instant;

use async_trait::async_trait;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::DynHandler;
use crate::http::Response;
use crate::request_timing::RequestTimings;

pub type BoxMiddleware = Arc<dyn Middleware>;

pub struct FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    func: F,
}

impl<F> FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    #[inline]
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

#[async_trait(?Send)]
impl<F, Fut> Middleware for FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    #[inline]
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        (self.func)(ctx, next).await
    }
}

#[async_trait(?Send)]
pub trait Middleware: Send + Sync + 'static {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError>;
}

pub struct Next<'mw> {
    handler: &'mw dyn DynHandler,
    middlewares: &'mw [BoxMiddleware],
}

impl<'mw> Next<'mw> {
    #[inline]
    pub fn new(middlewares: &'mw [BoxMiddleware], handler: &'mw dyn DynHandler) -> Self {
        Self {
            handler,
            middlewares,
        }
    }

    /// # Errors
    /// Returns whatever error the next middleware or the final handler produces.
    #[inline]
    pub async fn run(self, ctx: RequestContext) -> Result<Response, EdgeError> {
        if let Some((head, tail)) = self.middlewares.split_first() {
            head.handle(ctx, Next::new(tail, self.handler)).await
        } else {
            self.handler.call(ctx).await
        }
    }
}

/// Opt-in, insert-if-absent attachment of a request-local timing handle.
///
/// Register before timing consumers. This never finalizes timings or renders
/// headers. Routes are matched before middleware, so unmatched 404/405 requests
/// bypass attachment. Do not install collectors with `RouterBuilder::with_state`.
pub struct RequestTimingMiddleware<const N: usize, D = ()> {
    data: PhantomData<fn() -> D>,
    excluded_paths: &'static [&'static str],
}

impl<const N: usize, D> Default for RequestTimingMiddleware<N, D> {
    #[inline]
    fn default() -> Self {
        Self {
            data: PhantomData,
            excluded_paths: &[],
        }
    }
}

impl<const N: usize, D> RequestTimingMiddleware<N, D> {
    /// Excludes exact URI paths for every method, ignoring query strings.
    /// Existing handles are preserved even on excluded paths.
    #[must_use]
    #[inline]
    pub fn with_excluded_paths(mut self, paths: &'static [&'static str]) -> Self {
        self.excluded_paths = paths;
        self
    }
}

#[async_trait(?Send)]
impl<const N: usize, D: Default + Send + 'static> Middleware for RequestTimingMiddleware<N, D> {
    #[inline]
    async fn handle(&self, mut ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        if ctx
            .request()
            .extensions()
            .get::<RequestTimings<N, D>>()
            .is_none()
            && !self.excluded_paths.contains(&ctx.request().uri().path())
        {
            ctx.request_mut()
                .extensions_mut()
                .insert(RequestTimings::<N, D>::new());
        }
        next.run(ctx).await
    }
}

pub struct RequestLogger;

#[async_trait(?Send)]
impl Middleware for RequestLogger {
    #[inline]
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        let method = ctx.request().method().clone();
        let path = ctx.request().uri().path().to_owned();
        let start = Instant::now();

        match next.run(ctx).await {
            Ok(response) => {
                let status = response.status();
                let elapsed = start.elapsed().as_millis();
                tracing::info!(
                    "request method={} path={} status={} elapsed_ms={}",
                    method,
                    path,
                    status.as_u16(),
                    elapsed
                );
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let message = err.message();
                let elapsed = start.elapsed().as_millis();
                tracing::error!(
                    "request method={} path={} status={} error={} elapsed_ms={}",
                    method,
                    path,
                    status.as_u16(),
                    message,
                    elapsed
                );
                Err(err)
            }
        }
    }
}

#[inline]
pub fn middleware_fn<F, Fut>(func: F) -> FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    FnMiddleware::new(func)
}

#[cfg(test)]
mod request_timing_tests {
    use super::*;
    use crate::body::Body;
    use crate::handler::IntoHandler as _;
    use crate::http::{Method, Request, StatusCode, request_builder};
    use crate::params::PathParams;
    use crate::response::response_with_body;
    use crate::router::RouterService;
    use futures::executor::block_on;
    use std::sync::Mutex;
    use std::time::Duration;
    use tower_service::Service as _;

    type Timings = RequestTimings<1>;

    struct BeforeAttachment;
    #[async_trait(?Send)]
    impl Middleware for BeforeAttachment {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            assert!(ctx.request().extensions().get::<Timings>().is_none());
            next.run(ctx).await
        }
    }

    struct Capture {
        handles: Arc<Mutex<Vec<Timings>>>,
        short_circuit: bool,
    }

    #[async_trait(?Send)]
    impl Middleware for Capture {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            let timings = ctx.request().extensions().get::<Timings>().unwrap().clone();
            self.handles.lock().unwrap().push(timings);
            if self.short_circuit {
                response_with_body(StatusCode::UNAUTHORIZED, Body::text("stopped"))
            } else {
                next.run(ctx).await
            }
        }
    }

    #[crate::action]
    async fn presence(ctx: RequestContext) -> Result<Response, EdgeError> {
        let installed = ctx.request().extensions().get::<Timings>();
        if let Some(timings) = installed {
            timings.record(0, Duration::from_millis(3)).unwrap();
        }
        response_with_body(
            StatusCode::OK,
            Body::text(if installed.is_some() { "yes" } else { "no" }),
        )
    }

    #[crate::action]
    async fn failure(_ctx: RequestContext) -> Result<Response, EdgeError> {
        Err(EdgeError::bad_request("unchanged timing error"))
    }

    fn request(method: Method, uri: &str) -> Request {
        request_builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn request_timing_exclusions_are_exact_paths_for_every_method_and_query() {
        let router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default().with_excluded_paths(&["/health"]))
            .get("/health", presence)
            .post("/health", presence)
            .get("/health/", presence)
            .get("/healthy", presence)
            .build();
        for method in [Method::GET, Method::POST] {
            for uri in ["/health", "/health?check=1"] {
                let response = block_on(router.oneshot(request(method.clone(), uri))).unwrap();
                assert_eq!(response.body().as_bytes().unwrap(), b"no");
                let timings = Timings::new();
                timings.record(0, Duration::from_millis(8)).unwrap();
                let mut preinstalled = request(method.clone(), uri);
                preinstalled.extensions_mut().insert(timings.clone());
                let preserved = block_on(router.oneshot(preinstalled)).unwrap();
                assert_eq!(preserved.body().as_bytes().unwrap(), b"yes");
                assert_eq!(
                    timings.snapshot(|view| view.phases).unwrap(),
                    [Some(Duration::from_millis(11))]
                );
            }
        }
        for uri in ["/health/", "/healthy"] {
            let response = block_on(router.oneshot(request(Method::GET, uri))).unwrap();
            assert_eq!(response.body().as_bytes().unwrap(), b"yes");
        }
        let default_router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .get("/health", presence)
            .build();
        let response = block_on(default_router.oneshot(request(Method::GET, "/health"))).unwrap();
        assert_eq!(response.body().as_bytes().unwrap(), b"yes");
    }

    #[test]
    fn request_timing_duplicate_installation_preserves_state_and_requests_are_fresh() {
        let handles = Arc::new(Mutex::new(Vec::<Timings>::new()));
        let router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .middleware(Capture {
                handles: Arc::clone(&handles),
                short_circuit: false,
            })
            .middleware(RequestTimingMiddleware::<1>::default())
            .get("/timed", presence)
            .build();
        let timings = Timings::new();
        timings.record(0, Duration::from_millis(8)).unwrap();
        let mut preinstalled = request(Method::GET, "/timed");
        preinstalled.extensions_mut().insert(timings.clone());
        block_on(router.oneshot(preinstalled)).unwrap();
        assert_eq!(
            timings.snapshot(|view| view.phases).unwrap(),
            [Some(Duration::from_millis(11))]
        );
        for _ in 0_u8..2 {
            block_on(router.oneshot(request(Method::GET, "/timed"))).unwrap();
        }
        let saved = handles.lock().unwrap();
        assert_eq!(saved.len(), 3);
        saved[1].record(0, Duration::from_millis(5)).unwrap();
        assert_eq!(
            saved[1].snapshot(|view| view.phases).unwrap(),
            [Some(Duration::from_millis(8))]
        );
        assert_eq!(
            saved[2].snapshot(|view| view.phases).unwrap(),
            [Some(Duration::from_millis(3))]
        );
        for handle in saved.iter() {
            assert_eq!(
                handle
                    .snapshot(|view| (view.headers_ready_total, view.request_elapsed))
                    .unwrap(),
                (None, None)
            );
        }
    }

    #[test]
    fn request_timing_preserves_short_circuits_and_errors_without_finalization() {
        let handles = Arc::new(Mutex::new(Vec::<Timings>::new()));
        let short_router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .middleware(Capture {
                handles: Arc::clone(&handles),
                short_circuit: true,
            })
            .get("/timed", presence)
            .build();
        let response = block_on(short_router.oneshot(request(Method::GET, "/timed"))).unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.body().as_bytes().unwrap(), b"stopped");
        assert_eq!(
            handles.lock().unwrap()[0]
                .snapshot(|view| (view.phases, view.request_elapsed))
                .unwrap(),
            ([None], None)
        );

        let mut router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .middleware(Capture {
                handles: Arc::clone(&handles),
                short_circuit: false,
            })
            .get("/error", failure)
            .build();
        let error = block_on(router.call(request(Method::GET, "/error"))).unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.message(), "unchanged timing error");
        let rendered = block_on(router.oneshot(request(Method::GET, "/error"))).unwrap();
        assert_eq!(rendered.status(), StatusCode::BAD_REQUEST);
        assert!(!rendered.headers().contains_key("server-timing"));
        let saved = handles.lock().unwrap();
        assert_eq!(saved.len(), 3);
        for handle in saved.iter() {
            assert_eq!(
                handle
                    .snapshot(|view| (view.headers_ready_total, view.request_elapsed))
                    .unwrap(),
                (None, None)
            );
        }
    }

    #[test]
    fn request_timing_runs_after_prior_middleware_and_unmatched_routes_bypass_it() {
        let handles = Arc::new(Mutex::new(Vec::<Timings>::new()));
        let router = RouterService::builder()
            .middleware(RequestTimingMiddleware::<1>::default())
            .middleware(Capture {
                handles: Arc::clone(&handles),
                short_circuit: false,
            })
            .get("/timed", presence)
            .build();
        for (method, uri, status) in [
            (Method::GET, "/missing", StatusCode::NOT_FOUND),
            (Method::POST, "/timed", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let response = block_on(router.oneshot(request(method, uri))).unwrap();
            assert_eq!(response.status(), status);
        }
        assert!(handles.lock().unwrap().is_empty());

        let ordered = RouterService::builder()
            .middleware(BeforeAttachment)
            .middleware(RequestTimingMiddleware::<1>::default())
            .get("/timed", presence)
            .build();
        assert_eq!(
            block_on(ordered.oneshot(request(Method::GET, "/timed")))
                .unwrap()
                .body()
                .as_bytes()
                .unwrap(),
            b"yes"
        );
    }

    #[test]
    fn request_timing_next_preserves_handler_error() {
        let handler = failure.into_handler();
        let ctx = RequestContext::new(request(Method::GET, "/error"), PathParams::default());
        let error = block_on(
            RequestTimingMiddleware::<1>::default().handle(ctx, Next::new(&[], handler.as_ref())),
        )
        .unwrap_err();
        assert_eq!(error.message(), "unchanged timing error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::handler::IntoHandler as _;
    use crate::http::{Method, Response, StatusCode, request_builder};
    use crate::params::PathParams;
    use crate::response::response_with_body;
    use futures::executor::block_on;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct RecordingMiddleware {
        log: Arc<Mutex<Vec<String>>>,
        name: &'static str,
    }

    struct ShortCircuit;

    #[async_trait(?Send)]
    impl Middleware for RecordingMiddleware {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            self.log.lock().unwrap().push(self.name.to_owned());
            next.run(ctx).await
        }
    }

    #[async_trait(?Send)]
    impl Middleware for ShortCircuit {
        async fn handle(
            &self,
            _ctx: RequestContext,
            _next: Next<'_>,
        ) -> Result<Response, EdgeError> {
            response_with_body(StatusCode::UNAUTHORIZED, Body::empty())
        }
    }

    fn empty_context() -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    async fn ok_handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, Body::empty())
    }

    #[test]
    fn middleware_can_short_circuit() {
        let handler = ok_handler.into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(ShortCircuit)];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn middleware_chain_runs_in_order() {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let first = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "first",
        };
        let second = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "second",
        };

        let handler = (|_ctx: RequestContext| async move {
            response_with_body(StatusCode::OK, Body::empty())
        })
        .into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(first), Arc::new(second)];

        let result = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(result.status(), StatusCode::OK);

        let calls = log.lock().unwrap().clone();
        assert_eq!(calls, vec!["first".to_owned(), "second".to_owned()]);
    }

    #[test]
    fn middleware_fn_executes_closure() {
        let called = Arc::new(AtomicBool::new(false));
        let outer_flag = Arc::clone(&called);
        let middleware = middleware_fn(move |_ctx, _next| {
            let inner_flag = Arc::clone(&outer_flag);
            async move {
                inner_flag.store(true, Ordering::SeqCst);
                response_with_body(StatusCode::OK, Body::empty())
            }
        });

        let handler = ok_handler.into_handler();
        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(middleware)];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn next_runs_handler_without_middlewares() {
        let handler = ok_handler.into_handler();
        let response =
            block_on(Next::new(&[], handler.as_ref()).run(empty_context())).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn request_logger_passes_through_success() {
        let handler = ok_handler.into_handler();
        let response =
            block_on(RequestLogger.handle(empty_context(), Next::new(&[], handler.as_ref())))
                .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn request_logger_propagates_error() {
        let handler = (|_ctx: RequestContext| async move {
            Err::<Response, EdgeError>(EdgeError::bad_request("boom"))
        })
        .into_handler();
        let err = block_on(RequestLogger.handle(empty_context(), Next::new(&[], handler.as_ref())))
            .expect_err("error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
