use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::DynHandler;
use crate::http::Response;

pub type BoxMiddleware = Arc<dyn Middleware>;

#[async_trait(?Send)]
pub trait Middleware: Send + Sync + 'static {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError>;
}

pub struct Next<'a> {
    middlewares: &'a [BoxMiddleware],
    handler: &'a dyn DynHandler,
}

impl<'a> Next<'a> {
    pub fn new(middlewares: &'a [BoxMiddleware], handler: &'a dyn DynHandler) -> Self {
        Self {
            middlewares,
            handler,
        }
    }

    pub async fn run(self, ctx: RequestContext) -> Result<Response, EdgeError> {
        if let Some((head, tail)) = self.middlewares.split_first() {
            head.handle(ctx, Next::new(tail, self.handler)).await
        } else {
            self.handler.call(ctx).await
        }
    }
}

pub struct RequestLogger;

#[async_trait(?Send)]
impl Middleware for RequestLogger {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        let method = ctx.request().method().clone();
        let path = ctx.request().uri().path().to_string();
        let start = Instant::now();

        match next.run(ctx).await {
            Ok(response) => {
                let status = response.status();
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(
                    "request method={} path={} status={} elapsed_ms={:.2}",
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
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                tracing::error!(
                    "request method={} path={} status={} error={} elapsed_ms={:.2}",
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

pub struct FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    f: F,
}

impl<F> FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait(?Send)]
impl<F, Fut> Middleware for FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        (self.f)(ctx, next).await
    }
}

pub fn middleware_fn<F, Fut>(f: F) -> FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    FnMiddleware::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::handler::IntoHandler;
    use crate::http::{request_builder, Method, Response, StatusCode};
    use crate::params::PathParams;
    use crate::response::response_with_body;
    use futures::executor::block_on;
    use std::sync::{Arc, Mutex};

    struct RecordingMiddleware {
        log: Arc<Mutex<Vec<String>>>,
        name: &'static str,
    }

    #[async_trait(?Send)]
    impl Middleware for RecordingMiddleware {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            {
                let mut entries = self.log.lock().unwrap();
                entries.push(self.name.to_string());
            }
            next.run(ctx).await
        }
    }

    struct ShortCircuit;

    #[async_trait(?Send)]
    impl Middleware for ShortCircuit {
        async fn handle(
            &self,
            _ctx: RequestContext,
            _next: Next<'_>,
        ) -> Result<Response, EdgeError> {
            Ok(response_with_body(StatusCode::UNAUTHORIZED, Body::empty()))
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
        Ok(response_with_body(StatusCode::OK, Body::empty()))
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
            Ok::<Response, EdgeError>(response_with_body(StatusCode::OK, Body::empty()))
        })
        .into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![
            Arc::new(first) as BoxMiddleware,
            Arc::new(second) as BoxMiddleware,
        ];

        let result = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(result.status(), StatusCode::OK);

        let calls = log.lock().unwrap().clone();
        assert_eq!(calls, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn middleware_can_short_circuit() {
        let handler = ok_handler.into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(ShortCircuit) as BoxMiddleware];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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

    #[test]
    fn middleware_fn_executes_closure() {
        let called = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&called);
        let middleware = middleware_fn(move |_ctx, _next| {
            let flag = Arc::clone(&flag);
            async move {
                *flag.lock().unwrap() = true;
                Ok(response_with_body(StatusCode::OK, Body::empty()))
            }
        });

        let handler = ok_handler.into_handler();
        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(middleware) as BoxMiddleware];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(*called.lock().unwrap());
    }
}
