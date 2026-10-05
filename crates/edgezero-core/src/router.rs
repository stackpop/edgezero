use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
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

/// Shared optional interception performed before router lookup and state injection.
pub type BoxPreDispatchHook = Arc<dyn PreDispatchHook>;

/// Inspect or terminate a request before method/path lookup and route middleware.
///
/// The hook receives adapter extensions but no router-injected state or
/// introspection. It may capture application state in its own implementation.
/// Futures deliberately do not require `Send` so WASM runtimes remain supported.
#[async_trait(?Send)]
pub trait PreDispatchHook: Send + Sync + 'static {
    /// Return a terminal response, or continue with the mutated request.
    ///
    /// # Errors
    ///
    /// Errors prevent dispatch. [`Service::call`] propagates them, while
    /// [`RouterService::oneshot`] uses the existing error-response rendering.
    async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError>;
}

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
    pre_dispatch_hook: Option<BoxPreDispatchHook>,
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
            self.pre_dispatch_hook,
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

    /// Install one hook before route lookup, state injection and middleware.
    ///
    /// Repeated registration replaces the previous hook. Router clones share
    /// the same instance. None of the normal route lifecycle runs after a
    /// terminal response; outer adapter/application finalizers may still run.
    #[must_use]
    #[inline]
    pub fn pre_dispatch_hook(mut self, hook: BoxPreDispatchHook) -> Self {
        self.pre_dispatch_hook = Some(hook);
        self
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
    pre_dispatch_hook: Option<BoxPreDispatchHook>,
    route_index: Arc<[RouteInfo]>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    state_extensions: Extensions,
}

impl RouterInner {
    async fn dispatch(&self, mut request: Request) -> Result<Response, EdgeError> {
        if let Some(hook) = &self.pre_dispatch_hook
            && let Some(response) = hook.handle(&mut request).await?
        {
            return Ok(response);
        }
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
        pre_dispatch_hook: Option<BoxPreDispatchHook>,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                manifest_json,
                middlewares,
                pre_dispatch_hook,
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

#[cfg(test)]
mod pre_dispatch_tests {
    use std::io::Error as IoError;
    use std::mem::take;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures::future::ready;
    use futures::stream;
    use tower_service::Service as _;

    use crate::action;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{Method, Request, Response, StatusCode, request_builder, response_builder};
    use crate::introspection::{ManifestJson, RouteTable};
    use crate::middleware::{Middleware, Next};
    use crate::router::{PreDispatchHook, RouterService};

    struct CountedState {
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CountedState {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::SeqCst);
            Self {
                clones: Arc::clone(&self.clones),
            }
        }

        fn clone_from(&mut self, source: &Self) {
            *self = source.clone();
        }
    }

    struct CountingMiddleware {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait(?Send)]
    impl Middleware for CountingMiddleware {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            next.run(ctx).await
        }
    }

    #[derive(Clone)]
    struct HookMarker;

    struct InspectHook;

    #[async_trait(?Send)]
    impl PreDispatchHook for InspectHook {
        async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
            let local = Rc::new(7_u32);
            let body = take(request.body_mut());
            body.into_bytes_bounded(0).await?;
            ready(()).await;
            assert_eq!(*local, 7_u32, "should support a non-Send hook future");
            Ok(Some(
                response_builder()
                    .status(204)
                    .body(Body::empty())
                    .map_err(EdgeError::internal)?,
            ))
        }
    }

    struct RewriteHook;

    #[async_trait(?Send)]
    impl PreDispatchHook for RewriteHook {
        async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
            assert!(request.extensions().get::<CountedState>().is_none());
            *request.method_mut() = Method::POST;
            *request.uri_mut() = "/after".parse().expect("should parse fixed URI");
            request.headers_mut().insert(
                "x-hook",
                "present".parse().expect("should parse fixed header"),
            );
            request.extensions_mut().insert(HookMarker);
            *request.body_mut() = Body::text("changed");
            Ok(None)
        }
    }

    struct TerminalHook {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait(?Send)]
    impl PreDispatchHook for TerminalHook {
        async fn handle(&self, request: &mut Request) -> Result<Option<Response>, EdgeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(request.extensions().get::<CountedState>().is_none());
            assert!(request.extensions().get::<ManifestJson>().is_none());
            assert!(request.extensions().get::<RouteTable>().is_none());
            if self.fail {
                return Err(EdgeError::bad_request("hook rejected request"));
            }
            Ok(Some(
                response_builder()
                    .status(418)
                    .header("allow", "POST")
                    .header("www-authenticate", "Basic")
                    .body(Body::text("local"))
                    .map_err(EdgeError::internal)?,
            ))
        }
    }

    #[action]
    async fn after_handler(context: RequestContext) -> Result<Response, EdgeError> {
        assert!(context.request().extensions().get::<HookMarker>().is_some());
        assert!(
            context
                .request()
                .extensions()
                .get::<CountedState>()
                .is_some()
        );
        assert_eq!(context.request().headers()["x-hook"], "present");
        assert_eq!(
            context.request().body().as_bytes(),
            Some(b"changed".as_slice())
        );
        response_builder()
            .body(Body::text("continued"))
            .map_err(EdgeError::internal)
    }

    #[action(manifest, routes)]
    async fn unreachable_handler() -> &'static str {
        panic!("should short-circuit before handler");
    }

    fn input(method: Method, path: &str, body: Body) -> Request {
        request_builder()
            .method(method)
            .uri(path)
            .body(body)
            .expect("should build hook input")
    }

    #[test]
    fn pre_dispatch_continuation_uses_mutated_request() {
        let clones = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .with_state(CountedState {
                clones: Arc::clone(&clones),
            })
            .pre_dispatch_hook(Arc::new(RewriteHook))
            .post("/after", after_handler)
            .build();
        let response = block_on(router.oneshot(input(Method::GET, "/before", Body::empty())))
            .expect("should dispatch rewritten request");
        assert_eq!(response.body().as_bytes(), Some(b"continued".as_slice()));
        assert_eq!(clones.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pre_dispatch_errors_stop_routing_in_both_service_apis() {
        let clones = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .with_state(CountedState {
                clones: Arc::clone(&clones),
            })
            .pre_dispatch_hook(Arc::new(TerminalHook {
                calls: Arc::clone(&calls),
                fail: true,
            }))
            .post("/after", after_handler)
            .build();
        let error = block_on(
            router
                .clone()
                .call(input(Method::POST, "/after", Body::empty())),
        )
        .expect_err("should propagate hook error from Service");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let response = block_on(router.oneshot(input(Method::POST, "/after", Body::empty())))
            .expect("should render hook error from oneshot");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(clones.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pre_dispatch_hook_is_shared_across_router_clones() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .pre_dispatch_hook(Arc::new(TerminalHook {
                calls: Arc::clone(&calls),
                fail: false,
            }))
            .build();
        for service in [router.clone(), router] {
            let response = block_on(service.oneshot(input(Method::GET, "/missing", Body::empty())))
                .expect("should reuse shared hook");
            assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn pre_dispatch_inspects_empty_stream_without_send_bound() {
        let router = RouterService::builder()
            .pre_dispatch_hook(Arc::new(InspectHook))
            .build();
        let chunks = stream::iter([Ok::<Bytes, IoError>(Bytes::new()), Ok(Bytes::new())]);
        let response =
            block_on(router.oneshot(input(Method::POST, "/unknown", Body::from_stream(chunks))))
                .expect("should inspect clean streamed EOF");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn pre_dispatch_registration_replaces_previous_hook() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .pre_dispatch_hook(Arc::new(TerminalHook {
                calls: Arc::clone(&first),
                fail: true,
            }))
            .pre_dispatch_hook(Arc::new(TerminalHook {
                calls: Arc::clone(&second),
                fail: false,
            }))
            .build();
        let response = block_on(router.oneshot(input(Method::GET, "/unknown", Body::empty())))
            .expect("should run replacement hook only");
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pre_dispatch_rejects_nonempty_or_failed_streams() {
        let router = RouterService::builder()
            .pre_dispatch_hook(Arc::new(InspectHook))
            .build();
        let chunks = stream::iter([Ok::<Bytes, IoError>(Bytes::from_static(b"x"))]);
        let response =
            block_on(router.oneshot(input(Method::POST, "/unknown", Body::from_stream(chunks))))
                .expect("should render nonempty body rejection");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let failure = stream::iter([Err::<Bytes, IoError>(IoError::other("fixture failure"))]);
        let failure_response =
            block_on(router.oneshot(input(Method::POST, "/unknown", Body::from_stream(failure))))
                .expect("should render stream failure");
        assert_eq!(failure_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn pre_dispatch_terminal_response_precedes_method_path_and_lifecycle() {
        let calls = Arc::new(AtomicUsize::new(0));
        let clones = Arc::new(AtomicUsize::new(0));
        let middleware_calls = Arc::new(AtomicUsize::new(0));
        let router = RouterService::builder()
            .with_state(CountedState {
                clones: Arc::clone(&clones),
            })
            .with_manifest_json("{}")
            .middleware(CountingMiddleware {
                calls: Arc::clone(&middleware_calls),
            })
            .pre_dispatch_hook(Arc::new(TerminalHook {
                calls: Arc::clone(&calls),
                fail: false,
            }))
            .post("/after", unreachable_handler)
            .build();
        for (method, path) in [
            (Method::POST, "/after"),
            (Method::GET, "/after"),
            (
                Method::from_bytes(b"EXAMPLE-METHOD").expect("should parse extension method"),
                "/after",
            ),
            (Method::GET, "/missing"),
        ] {
            let response = block_on(router.oneshot(input(method, path, Body::empty())))
                .expect("should intercept before method/path lookup");
            assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
            assert_eq!(response.headers()["allow"], "POST");
            assert_eq!(response.headers()["www-authenticate"], "Basic");
            assert_eq!(response.body().as_bytes(), Some(b"local".as_slice()));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(clones.load(Ordering::SeqCst), 0);
        assert_eq!(middleware_calls.load(Ordering::SeqCst), 0);
    }
}
