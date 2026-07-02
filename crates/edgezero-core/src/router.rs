use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::{Context, Poll};

use matchit::Router as PathRouter;
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::{BoxHandler, IntoHandler};
use crate::http::{HandlerFuture, Method, Request, Response};
use crate::middleware::{BoxMiddleware, Middleware, Next};
use crate::params::PathParams;
use crate::response::IntoResponse as _;

struct RouteEntry {
    handler: BoxHandler,
}

impl Clone for RouteEntry {
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.handler = Arc::clone(&source.handler);
    }
}

#[derive(Clone, Debug)]
pub struct RouteInfo {
    method: Method,
    path: String,
}

impl RouteInfo {
    #[must_use]
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    #[inline]
    pub fn new<S: Into<String>>(method: Method, path: S) -> Self {
        Self {
            method,
            path: path.into(),
        }
    }

    #[must_use]
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Per-request introspection payload injected by [`RouterInner::dispatch`].
#[derive(Clone)]
pub struct IntrospectionData {
    /// The app manifest serialized to JSON at compile time by `app!`.
    pub manifest_json: Option<Arc<str>>,
    /// Every registered route, in registration order.
    pub routes: Arc<[RouteInfo]>,
}

enum RouteMatch<'route> {
    Found(&'route RouteEntry, PathParams),
    MethodNotAllowed(Vec<Method>),
    NotFound,
}

#[derive(Default)]
pub struct RouterBuilder {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_info: Vec<RouteInfo>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
}

impl RouterBuilder {
    #[expect(
        clippy::panic,
        reason = "duplicate route is a build-time programmer error, not a runtime condition"
    )]
    fn add_route<H>(&mut self, path: &str, method: Method, handler: H)
    where
        H: IntoHandler,
    {
        let router = self.routes.entry(method.clone()).or_default();

        router
            .insert(
                path,
                RouteEntry {
                    handler: handler.into_handler(),
                },
            )
            .unwrap_or_else(|err| panic!("duplicate route definition for {path}: {err}"));

        self.route_info
            .push(RouteInfo::new(method, path.to_owned()));
    }

    #[must_use]
    #[inline]
    pub fn build(self) -> RouterService {
        let route_index: Arc<[RouteInfo]> = Arc::from(self.route_info);

        RouterService::new(
            self.routes,
            self.middlewares,
            route_index,
            self.manifest_json,
        )
    }

    #[must_use]
    #[inline]
    pub fn delete<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::DELETE, handler)
    }

    #[must_use]
    #[inline]
    pub fn get<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::GET, handler)
    }

    #[must_use]
    #[inline]
    pub fn middleware<M>(mut self, middleware: M) -> Self
    where
        M: Middleware,
    {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    #[must_use]
    #[inline]
    pub fn middleware_arc(mut self, middleware: BoxMiddleware) -> Self {
        self.middlewares.push(middleware);
        self
    }

    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    #[inline]
    pub fn post<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::POST, handler)
    }

    #[must_use]
    #[inline]
    pub fn put<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::PUT, handler)
    }

    #[must_use]
    #[inline]
    pub fn route<H>(mut self, path: &str, method: Method, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.add_route(path, method, handler);
        self
    }

    #[must_use]
    #[inline]
    pub fn with_manifest_json<S: Into<Arc<str>>>(mut self, json: S) -> Self {
        self.manifest_json = Some(json.into());
        self
    }
}

struct RouterInner {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_index: Arc<[RouteInfo]>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
}

impl RouterInner {
    async fn dispatch(&self, mut request: Request) -> Result<Response, EdgeError> {
        request.extensions_mut().insert(IntrospectionData {
            manifest_json: self.manifest_json.clone(),
            routes: Arc::clone(&self.route_index),
        });

        let method = request.method().clone();
        let path = request.uri().path().to_owned();

        match self.find_route(&method, &path) {
            RouteMatch::Found(entry, params) => {
                let ctx = RequestContext::new(request, params);
                let next = Next::new(&self.middlewares, entry.handler.as_ref());
                next.run(ctx).await
            }
            RouteMatch::MethodNotAllowed(mut allowed) => {
                allowed.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                Err(EdgeError::method_not_allowed(&method, &allowed))
            }
            RouteMatch::NotFound => Err(EdgeError::not_found(path)),
        }
    }

    fn find_route(&self, method: &Method, path: &str) -> RouteMatch<'_> {
        if let Some(router) = self.routes.get(method) {
            if let Ok(matched) = router.at(path) {
                let params = PathParams::new(
                    matched
                        .params
                        .iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .collect(),
                );
                return RouteMatch::Found(matched.value, params);
            }
        }

        let allowed: HashSet<Method> = self
            .routes
            .iter()
            .filter(|(_, router)| router.at(path).is_ok())
            .map(|(candidate_method, _)| candidate_method.clone())
            .collect();

        if allowed.is_empty() {
            RouteMatch::NotFound
        } else {
            RouteMatch::MethodNotAllowed(allowed.into_iter().collect())
        }
    }
}

#[derive(Clone)]
pub struct RouterService {
    inner: Arc<RouterInner>,
}

impl Service<Request> for RouterService {
    type Error = EdgeError;
    type Future = HandlerFuture;
    type Response = Response;

    #[inline]
    fn call(&mut self, req: Request) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.dispatch(req).await })
    }

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl RouterService {
    #[must_use]
    #[inline]
    pub fn builder() -> RouterBuilder {
        RouterBuilder::new()
    }

    fn new(
        routes: HashMap<Method, PathRouter<RouteEntry>>,
        middlewares: Vec<BoxMiddleware>,
        route_index: Arc<[RouteInfo]>,
        manifest_json: Option<Arc<str>>,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                manifest_json,
                middlewares,
                route_index,
                routes,
            }),
        }
    }

    /// # Errors
    /// Returns [`EdgeError`] if the dispatched handler errors AND the error
    /// itself fails to render as a response.
    #[inline]
    pub async fn oneshot(&self, request: Request) -> Result<Response, EdgeError> {
        let mut service = self.clone();
        match service.call(request).await {
            Ok(response) => Ok(response),
            Err(err) => err.into_response(),
        }
    }

    #[must_use]
    #[inline]
    pub fn routes(&self) -> Vec<RouteInfo> {
        self.inner.route_index.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{request_builder, Method, Request, Response, StatusCode};
    use crate::params::PathParams;
    use crate::response::response_with_body;
    use futures::executor::block_on;
    use futures::task::noop_waker_ref;
    use serde::Deserialize;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    async fn ok_handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
        response_with_body(StatusCode::OK, Body::empty())
    }

    #[test]
    fn builder_accepts_middleware_and_middleware_arc() {
        struct RecordingMiddleware {
            log: Arc<Mutex<Vec<&'static str>>>,
            name: &'static str,
        }

        #[async_trait::async_trait(?Send)]
        impl Middleware for RecordingMiddleware {
            async fn handle(
                &self,
                ctx: RequestContext,
                next: Next<'_>,
            ) -> Result<Response, EdgeError> {
                self.log.lock().unwrap().push(self.name);
                next.run(ctx).await
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let first = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "first",
        };
        let second = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "second",
        };

        let service = RouterService::builder()
            .middleware(first)
            .middleware_arc({
                let arc: BoxMiddleware = Arc::new(second);
                arc
            })
            .get("/test", ok_handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        let response = block_on(service.clone().call(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let entries = log.lock().unwrap().clone();
        assert_eq!(entries, vec!["first", "second"]);
    }

    #[test]
    fn builder_supports_put_and_delete_routes() {
        let service = RouterService::builder()
            .put("/items", ok_handler)
            .delete("/items", ok_handler)
            .build();

        let put_request = request_builder()
            .method(Method::PUT)
            .uri("/items")
            .body(Body::empty())
            .expect("request");
        let put_response = block_on(service.clone().call(put_request)).expect("response");
        assert_eq!(put_response.status(), StatusCode::OK);

        let delete_request = request_builder()
            .method(Method::DELETE)
            .uri("/items")
            .body(Body::empty())
            .expect("request");
        let delete_response = block_on(service.clone().call(delete_request)).expect("response");
        assert_eq!(delete_response.status(), StatusCode::OK);
    }

    #[test]
    #[should_panic(expected = "duplicate route definition")]
    fn duplicate_route_definition_panics() {
        let _service = RouterService::builder()
            .get("/dup", ok_handler)
            .get("/dup", ok_handler)
            .build();
    }

    #[test]
    fn handler_returns_bad_request_for_invalid_path_params() {
        #[derive(Deserialize)]
        struct Params {
            id: String,
        }

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let params: Params = ctx.path()?;
            let id = params
                .id
                .parse::<u32>()
                .map_err(|_e| EdgeError::bad_request("invalid id"))?;
            Ok(format!("hello {id}"))
        }

        let service = RouterService::builder().get("/items/{id}", handler).build();
        let ok_request = request_builder()
            .method(Method::GET)
            .uri("/items/42")
            .body(Body::empty())
            .expect("request");
        let ok_response = block_on(service.clone().call(ok_request)).expect("response");
        assert_eq!(ok_response.status(), StatusCode::OK);
        assert_eq!(
            ok_response.body().as_bytes().expect("buffered"),
            b"hello 42"
        );

        let request = request_builder()
            .method(Method::GET)
            .uri("/items/abc")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn oneshot_returns_error_response() {
        let service = RouterService::builder().build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/missing")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn oneshot_returns_success_response() {
        let service = RouterService::builder().get("/ok", ok_handler).build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/ok")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn returns_method_not_allowed() {
        let service = RouterService::builder().post("/submit", ok_handler).build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/submit")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn returns_method_not_allowed_with_multiple_methods() {
        let service = RouterService::builder()
            .get("/submit", ok_handler)
            .post("/submit", ok_handler)
            .build();

        let request = request_builder()
            .method(Method::PUT)
            .uri("/submit")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn returns_not_found() {
        let service = RouterService::builder().get("/known", ok_handler).build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/missing")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn route_entry_clone_copies_handler() {
        let entry = RouteEntry {
            handler: ok_handler.into_handler(),
        };
        let cloned = entry.clone();

        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());
        let response = block_on(cloned.handler.call(ctx)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn route_matches_path_params() {
        #[derive(Deserialize)]
        struct Params {
            id: String,
        }

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let params: Params = ctx.path()?;
            Ok(format!("hello {}", params.id))
        }

        let service = RouterService::builder().get("/hello/{id}", handler).build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/hello/world")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.clone().call(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.body().as_bytes().expect("buffered"),
            b"hello world"
        );
    }

    #[test]
    fn service_poll_ready_reports_ready() {
        let mut service = RouterService::builder().build();
        let waker = noop_waker_ref();
        let mut cx = Context::from_waker(waker);
        let ready = Service::<Request>::poll_ready(&mut service, &mut cx);
        assert!(matches!(ready, Poll::Ready(Ok(()))));
    }

    #[test]
    fn dispatch_injects_introspection_data() {
        let seen: Arc<Mutex<Option<(bool, usize)>>> = Arc::new(Mutex::new(None));
        let seen_capture = Arc::clone(&seen);

        let handler = move |ctx: RequestContext| {
            let seen_inner = Arc::clone(&seen_capture);
            async move {
                let data = ctx.introspection().expect("introspection data present");
                *seen_inner.lock().unwrap() =
                    Some((data.manifest_json.is_some(), data.routes.len()));
                Ok::<_, EdgeError>("ok")
            }
        };

        let router = RouterService::builder()
            .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
            .get("/", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        block_on(router.oneshot(request)).unwrap();

        let (had_manifest, route_count) = seen.lock().unwrap().expect("handler ran");
        assert!(had_manifest, "manifest_json should be injected");
        assert_eq!(route_count, 1);
    }

    #[test]
    fn middleware_sees_introspection_data() {
        struct Probe(Arc<Mutex<Option<(bool, usize)>>>);
        #[async_trait::async_trait(?Send)]
        impl Middleware for Probe {
            async fn handle(
                &self,
                ctx: RequestContext,
                next: Next<'_>,
            ) -> Result<Response, EdgeError> {
                *self.0.lock().unwrap() = ctx
                    .introspection()
                    .map(|data| (data.manifest_json.is_some(), data.routes.len()));
                next.run(ctx).await
            }
        }

        let saw: Arc<Mutex<Option<(bool, usize)>>> = Arc::new(Mutex::new(None));
        let router = RouterService::builder()
            .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
            .middleware(Probe(Arc::clone(&saw)))
            .get("/", |_ctx: RequestContext| async {
                Ok::<_, EdgeError>("ok")
            })
            .build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        block_on(router.oneshot(request)).unwrap();
        let (had_manifest, route_count) = saw.lock().unwrap().expect("middleware ran");
        assert!(had_manifest, "middleware should see manifest_json");
        assert!(
            route_count > 0,
            "middleware should see non-empty route list"
        );
    }

    #[test]
    fn streams_body_through_router() {
        use bytes::Bytes;
        use futures_util::stream;
        use futures_util::StreamExt as _;

        async fn handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
            let chunks = stream::iter(vec![
                Bytes::from_static(b"chunk-one\n"),
                Bytes::from_static(b"chunk-two\n"),
            ]);

            (StatusCode::OK, Body::stream(chunks)).into_response()
        }

        let service = RouterService::builder().get("/stream", handler).build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/stream")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.clone().call(request)).expect("response");
        let mut stream = response.into_body().into_stream().expect("stream body");
        let collected = block_on(async {
            let mut acc = Vec::new();
            while let Some(result) = stream.next().await {
                let chunk = result.expect("chunk");
                acc.extend_from_slice(&chunk);
            }
            acc
        });
        assert_eq!(collected, b"chunk-one\nchunk-two\n");
    }
}
