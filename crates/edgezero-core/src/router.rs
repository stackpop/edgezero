use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::{Context, Poll};

use matchit::Router as PathRouter;
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::{BoxHandler, IntoHandler, IntrospectionNeeds};
use crate::http::{Extensions, HandlerFuture, Method, Request, Response};
use crate::introspection::{ManifestJson, RouteTable};
use crate::middleware::{BoxMiddleware, Middleware, Next};
use crate::params::PathParams;
use crate::response::IntoResponse as _;

struct RouteEntry {
    handler: BoxHandler,
    introspection_needs: IntrospectionNeeds,
}

impl Clone for RouteEntry {
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
            introspection_needs: self.introspection_needs,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.handler = Arc::clone(&source.handler);
        self.introspection_needs = source.introspection_needs;
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
    /// App state registered via [`RouterBuilder::with_state`], keyed by type.
    /// Cloned into every request's extensions at dispatch.
    state_extensions: Extensions,
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

        // The handler reports which introspection payloads its route needs; the
        // flag is read once here and consulted per request in `dispatch`.
        let boxed = handler.into_handler();
        let introspection_needs = boxed.introspection_needs();

        router
            .insert(
                path,
                RouteEntry {
                    handler: boxed,
                    introspection_needs,
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
            self.state_extensions,
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

    /// Register a value cloned into every request's extensions before
    /// dispatch, making it available to the [`State<T>`] extractor and to
    /// `RequestContext`-based handlers.
    ///
    /// Typically `T = Arc<AppState>`. Registering the same `T` twice is
    /// last-write-wins. Cost is one `T::clone` (an `Arc` bump for
    /// `Arc<AppState>`) per registered state per request.
    ///
    /// [`State<T>`]: crate::extractor::State
    #[must_use]
    #[inline]
    pub fn with_state<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.state_extensions.insert(value);
        self
    }
}

struct RouterInner {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_index: Arc<[RouteInfo]>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    state_extensions: Extensions,
}

impl RouterInner {
    async fn dispatch(&self, mut request: Request) -> Result<Response, EdgeError> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();

        match self.find_route(&method, &path) {
            RouteMatch::Found(entry, params) => {
                // Inject only the introspection payloads this route asked for —
                // nothing for the vast majority of routes that need none.
                let needs = entry.introspection_needs;
                if needs.manifest
                    && let Some(json) = &self.manifest_json
                {
                    request
                        .extensions_mut()
                        .insert(ManifestJson(Arc::clone(json)));
                }
                if needs.routes {
                    request
                        .extensions_mut()
                        .insert(RouteTable(Arc::clone(&self.route_index)));
                }
                // App-owned state registered via RouterBuilder::with_state.
                // Runs after introspection inserts; `extend` overwrites by
                // TypeId, so app state wins last-write on any collision.
                request
                    .extensions_mut()
                    .extend(self.state_extensions.clone());
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
        if let Some(router) = self.routes.get(method)
            && let Ok(matched) = router.at(path)
        {
            let params = PathParams::new(
                matched
                    .params
                    .iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            );
            return RouteMatch::Found(matched.value, params);
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
        state_extensions: Extensions,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                manifest_json,
                middlewares,
                route_index,
                routes,
                state_extensions,
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
    /// Per-capability introspection injection: a route receives exactly the
    /// payloads its handler opted into via `#[action(manifest|routes)]`.
    mod introspection_gating {
        use super::*;
        use crate::handler::DynHandler;

        /// A handler that records which introspection payloads its request
        /// carried, as `(manifest_present, routes_present)`, and reports `needs`.
        struct CapProbe {
            needs: IntrospectionNeeds,
            seen: Arc<Mutex<Option<(bool, bool)>>>,
        }

        impl DynHandler for CapProbe {
            fn call(&self, ctx: RequestContext) -> HandlerFuture {
                let seen = Arc::clone(&self.seen);
                Box::pin(async move {
                    *seen.lock().unwrap() = Some((
                        ctx.extension::<ManifestJson>().is_some(),
                        ctx.extension::<RouteTable>().is_some(),
                    ));
                    response_with_body(StatusCode::OK, Body::empty())
                })
            }
            fn introspection_needs(&self) -> IntrospectionNeeds {
                self.needs
            }
        }

        #[test]
        fn manifest_and_routes_route_injects_both() {
            // Combined `#[action(manifest, routes)]` → both payloads injected.
            let seen = run_probe(
                RouterService::builder().with_manifest_json("{\"app\":{\"name\":\"t\"}}"),
                IntrospectionNeeds {
                    manifest: true,
                    routes: true,
                },
            );
            assert_eq!(seen, (true, true));
        }

        #[test]
        fn manifest_route_injects_only_manifest() {
            // Manifest available AND requested → ManifestJson present, RouteTable absent.
            let seen = run_probe(
                RouterService::builder().with_manifest_json("{\"app\":{\"name\":\"t\"}}"),
                IntrospectionNeeds {
                    manifest: true,
                    routes: false,
                },
            );
            assert_eq!(seen, (true, false));
        }

        #[test]
        fn middleware_sees_injected_manifest() {
            // Injection happens before the middleware chain, so a middleware on a
            // manifest-flagged route sees the payload.
            struct Probe(Arc<Mutex<Option<bool>>>);
            #[async_trait::async_trait(?Send)]
            impl Middleware for Probe {
                async fn handle(
                    &self,
                    ctx: RequestContext,
                    next: Next<'_>,
                ) -> Result<Response, EdgeError> {
                    *self.0.lock().unwrap() = Some(ctx.extension::<ManifestJson>().is_some());
                    next.run(ctx).await
                }
            }

            let saw: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
            let router = RouterService::builder()
                .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
                .middleware(Probe(Arc::clone(&saw)))
                .get(
                    "/",
                    CapProbe {
                        needs: IntrospectionNeeds {
                            manifest: true,
                            routes: false,
                        },
                        seen: Arc::new(Mutex::new(None)),
                    },
                )
                .build();
            let request = request_builder()
                .method(Method::GET)
                .uri("/")
                .body(Body::empty())
                .unwrap();
            block_on(router.oneshot(request)).unwrap();
            assert_eq!(*saw.lock().unwrap(), Some(true));
        }

        #[test]
        fn plain_route_injects_neither() {
            // Manifest IS baked but the route requested nothing → neither injected.
            let seen = run_probe(
                RouterService::builder().with_manifest_json("{\"app\":{\"name\":\"t\"}}"),
                IntrospectionNeeds::default(),
            );
            assert_eq!(seen, (false, false));
        }

        #[test]
        fn routes_route_injects_only_routes() {
            // Only `routes` requested → RouteTable present (from the always-available
            // route index), ManifestJson absent (not requested; none baked either).
            let seen = run_probe(
                RouterService::builder(),
                IntrospectionNeeds {
                    manifest: false,
                    routes: true,
                },
            );
            assert_eq!(seen, (false, true));
        }

        fn run_probe(builder: RouterBuilder, needs: IntrospectionNeeds) -> (bool, bool) {
            let seen = Arc::new(Mutex::new(None));
            let router = builder
                .get(
                    "/",
                    CapProbe {
                        needs,
                        seen: Arc::clone(&seen),
                    },
                )
                .build();
            let request = request_builder()
                .method(Method::GET)
                .uri("/")
                .body(Body::empty())
                .unwrap();
            block_on(router.oneshot(request)).unwrap();
            let observed = *seen.lock().unwrap();
            observed.expect("handler ran")
        }
    }

    use super::*;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{Method, Request, Response, StatusCode, request_builder};
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
            introspection_needs: IntrospectionNeeds::default(),
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
    fn streams_body_through_router() {
        use bytes::Bytes;
        use futures_util::StreamExt as _;
        use futures_util::stream;

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

    #[test]
    fn with_state_exposes_value_to_handler() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct Counter(u32);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(counter) = State::<Counter>::from_request(&ctx).await?;
            Ok(format!("count={}", counter.0))
        }

        let service = RouterService::builder()
            .with_state(Counter(9))
            .get("/count", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/count")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"count=9");
    }

    #[test]
    fn with_state_last_write_wins_for_same_type() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct Counter(u32);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(counter) = State::<Counter>::from_request(&ctx).await?;
            Ok(format!("count={}", counter.0))
        }

        let service = RouterService::builder()
            .with_state(Counter(1))
            .with_state(Counter(2))
            .get("/c", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/c")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.body().as_bytes().expect("buffered"), b"count=2");
    }

    #[test]
    fn with_state_no_cross_request_bleed() {
        use crate::extractor::{FromRequest as _, State};
        use std::future::Future as _;

        #[derive(Clone)]
        struct Tag(&'static str);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(tag) = State::<Tag>::from_request(&ctx).await?;
            Ok(tag.0.to_owned())
        }

        let service = RouterService::builder()
            .with_state(Tag("shared"))
            .get("/t", handler)
            .build();

        let req1 = request_builder()
            .method(Method::GET)
            .uri("/t")
            .body(Body::empty())
            .expect("req1");
        let req2 = request_builder()
            .method(Method::GET)
            .uri("/t")
            .body(Body::empty())
            .expect("req2");

        // Two independent in-flight requests, polled interleaved on one thread.
        let mut f1 = Box::pin(service.oneshot(req1));
        let mut f2 = Box::pin(service.oneshot(req2));
        let mut cx = Context::from_waker(noop_waker_ref());

        let mut r1 = None;
        let mut r2 = None;
        while r1.is_none() || r2.is_none() {
            if r1.is_none()
                && let Poll::Ready(value) = f1.as_mut().poll(&mut cx)
            {
                r1 = Some(value);
            }
            if r2.is_none()
                && let Poll::Ready(value) = f2.as_mut().poll(&mut cx)
            {
                r2 = Some(value);
            }
        }

        let resp1 = r1.unwrap().expect("resp1");
        let resp2 = r2.unwrap().expect("resp2");
        assert_eq!(resp1.body().as_bytes().expect("buffered"), b"shared");
        assert_eq!(resp2.body().as_bytes().expect("buffered"), b"shared");
    }

    #[test]
    fn with_state_supports_multiple_distinct_types() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct First(u32);
        #[derive(Clone)]
        struct Second(&'static str);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(first) = State::<First>::from_request(&ctx).await?;
            let State(second) = State::<Second>::from_request(&ctx).await?;
            Ok(format!("{}-{}", first.0, second.0))
        }

        let service = RouterService::builder()
            .with_state(First(7))
            .with_state(Second("hi"))
            .get("/both", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/both")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.body().as_bytes().expect("buffered"), b"7-hi");
    }
}
