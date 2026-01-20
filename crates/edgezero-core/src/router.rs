use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use matchit::Router as PathRouter;
use serde::Serialize;
use tower_service::Service;

use crate::body::Body;
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::{BoxHandler, IntoHandler};
use crate::http::{
    header::CONTENT_TYPE, response_builder, HandlerFuture, HeaderValue, Method, Request, Response,
    ResponseBuilder, StatusCode,
};
use crate::middleware::{BoxMiddleware, Middleware, Next};
use crate::params::PathParams;
use crate::response::IntoResponse;

pub const DEFAULT_ROUTE_LISTING_PATH: &str = "/__edgezero/routes";

#[derive(Clone, Debug)]
pub struct RouteInfo {
    method: Method,
    path: String,
}

impl RouteInfo {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
        }
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

#[derive(Serialize)]
struct RouteListingEntry {
    method: String,
    path: String,
}

fn build_listing_response<T: Serialize>(
    payload: &T,
    builder: ResponseBuilder,
) -> Result<Response, EdgeError> {
    let body = Body::json(payload).map_err(EdgeError::internal)?;
    let response = builder
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(body)
        .map_err(EdgeError::internal)?;
    Ok(response)
}

#[derive(Default)]
pub struct RouterBuilder {
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    middlewares: Vec<BoxMiddleware>,
    route_info: Vec<RouteInfo>,
    route_listing_path: Option<String>,
}

impl RouterBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable_route_listing(self) -> Self {
        self.enable_route_listing_at(DEFAULT_ROUTE_LISTING_PATH)
    }

    pub fn enable_route_listing_at<S>(mut self, path: S) -> Self
    where
        S: Into<String>,
    {
        let path = path.into();
        assert!(!path.is_empty(), "route listing path cannot be empty");
        assert!(
            path.starts_with('/'),
            "route listing path must begin with '/'"
        );
        self.route_listing_path = Some(path);
        self
    }

    pub fn route<H>(mut self, path: &str, method: Method, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.add_route(path, method, handler);
        self
    }

    pub fn get<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::GET, handler)
    }

    pub fn post<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::POST, handler)
    }

    pub fn put<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::PUT, handler)
    }

    pub fn delete<H>(self, path: &str, handler: H) -> Self
    where
        H: IntoHandler,
    {
        self.route(path, Method::DELETE, handler)
    }

    pub fn middleware<M>(mut self, middleware: M) -> Self
    where
        M: Middleware,
    {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    pub fn middleware_arc(mut self, middleware: BoxMiddleware) -> Self {
        self.middlewares.push(middleware);
        self
    }

    pub fn build(mut self) -> RouterService {
        let listing_path = self.route_listing_path.clone();

        let mut route_info = self.route_info.clone();
        if let Some(ref path) = listing_path {
            route_info.push(RouteInfo::new(Method::GET, path.clone()));
        }

        let route_index = Arc::new(route_info);

        if let Some(path) = listing_path {
            let index = Arc::clone(&route_index);
            let listing_handler = move |_ctx: RequestContext| {
                let index = Arc::clone(&index);
                async move {
                    let payload: Vec<RouteListingEntry> = index
                        .iter()
                        .map(|route| RouteListingEntry {
                            method: route.method().as_str().to_string(),
                            path: route.path().to_string(),
                        })
                        .collect();

                    build_listing_response(&payload, response_builder())
                }
            };

            self.routes
                .entry(Method::GET)
                .or_default()
                .insert(
                    path.as_str(),
                    RouteEntry {
                        handler: listing_handler.into_handler(),
                    },
                )
                .unwrap_or_else(|err| panic!("duplicate route definition for {}: {}", path, err));
        }

        RouterService::new(self.routes, self.middlewares, route_index)
    }

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
            .unwrap_or_else(|err| panic!("duplicate route definition for {}: {}", path, err));

        self.route_info
            .push(RouteInfo::new(method, path.to_string()));
    }
}

#[derive(Clone)]
pub struct RouterService {
    inner: Arc<RouterInner>,
}

impl RouterService {
    fn new(
        routes: HashMap<Method, PathRouter<RouteEntry>>,
        middlewares: Vec<BoxMiddleware>,
        route_index: Arc<Vec<RouteInfo>>,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                routes,
                middlewares,
                route_index,
            }),
        }
    }

    pub fn builder() -> RouterBuilder {
        RouterBuilder::new()
    }

    pub fn routes(&self) -> Vec<RouteInfo> {
        (*self.inner.route_index).clone()
    }

    pub async fn oneshot(&self, request: Request) -> Response {
        let mut service = self.clone();
        match service.call(request).await {
            Ok(response) => response,
            Err(err) => err.into_response(),
        }
    }
}

struct RouterInner {
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    middlewares: Vec<BoxMiddleware>,
    route_index: Arc<Vec<RouteInfo>>,
}

enum RouteMatch<'a> {
    Found(&'a RouteEntry, PathParams),
    MethodNotAllowed(Vec<Method>),
    NotFound,
}

impl RouterInner {
    async fn dispatch(&self, request: Request) -> Result<Response, EdgeError> {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        match self.find_route(&method, &path) {
            RouteMatch::Found(entry, params) => {
                let ctx = RequestContext::new(request, params);
                let next = Next::new(&self.middlewares, entry.handler.as_ref());
                next.run(ctx).await
            }
            RouteMatch::MethodNotAllowed(mut allowed) => {
                allowed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
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
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                );
                return RouteMatch::Found(matched.value, params);
            }
        }

        let mut allowed = HashSet::new();
        for (candidate_method, router) in &self.routes {
            if router.at(path).is_ok() {
                allowed.insert(candidate_method.clone());
            }
        }

        if allowed.is_empty() {
            RouteMatch::NotFound
        } else {
            RouteMatch::MethodNotAllowed(allowed.into_iter().collect())
        }
    }
}

impl Service<Request> for RouterService {
    type Response = Response;
    type Error = EdgeError;
    type Future = HandlerFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.dispatch(request).await })
    }
}

struct RouteEntry {
    handler: BoxHandler,
}

impl Clone for RouteEntry {
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::context::RequestContext;
    use crate::error::EdgeError;
    use crate::http::{request_builder, Method, Request, Response, StatusCode};
    use crate::response::response_with_body;
    use crate::params::PathParams;
    use futures::executor::block_on;
    use futures::task::noop_waker_ref;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    async fn ok_handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
        Ok(response_with_body(StatusCode::OK, Body::empty()))
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
        assert_eq!(response.body().as_bytes(), b"hello world");
    }

    #[test]
    fn route_listing_outputs_all_routes() {
        async fn noop(_ctx: RequestContext) -> Result<(), EdgeError> {
            Ok(())
        }

        let service = RouterService::builder()
            .enable_route_listing()
            .get("/health", noop)
            .post("/items", noop)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri(DEFAULT_ROUTE_LISTING_PATH)
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.clone().call(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.body().as_bytes();
        let payload: Vec<serde_json::Value> = serde_json::from_slice(body).expect("json payload");

        assert!(payload.contains(&json!({
            "method": "GET",
            "path": DEFAULT_ROUTE_LISTING_PATH
        })));
        assert!(payload.contains(&json!({
            "method": "GET",
            "path": "/health"
        })));
        assert!(payload.contains(&json!({
            "method": "POST",
            "path": "/items"
        })));

        let routes = service.routes();
        assert!(routes
            .iter()
            .any(|route| route.path() == "/health" && *route.method() == Method::GET));

        let health_request = request_builder()
            .method(Method::GET)
            .uri("/health")
            .body(Body::empty())
            .expect("request");
        let health_response = block_on(service.clone().call(health_request)).expect("response");
        assert_eq!(health_response.status(), StatusCode::NO_CONTENT);

        let items_request = request_builder()
            .method(Method::POST)
            .uri("/items")
            .body(Body::empty())
            .expect("request");
        let items_response = block_on(service.clone().call(items_request)).expect("response");
        assert_eq!(items_response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn route_listing_response_handles_json_failure() {
        struct FailingSerialize;

        impl Serialize for FailingSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("boom"))
            }
        }

        let err = build_listing_response(&FailingSerialize, response_builder())
            .expect_err("expected error");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn route_listing_response_handles_builder_failure() {
        #[derive(Serialize)]
        struct Payload {
            ok: bool,
        }

        let builder = response_builder().header("bad\nname", "value");
        let err = build_listing_response(&Payload { ok: true }, builder)
            .expect_err("expected error");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    #[should_panic(expected = "duplicate route definition")]
    fn route_listing_duplicate_path_panics() {
        RouterService::builder()
            .enable_route_listing()
            .get(DEFAULT_ROUTE_LISTING_PATH, ok_handler)
            .build();
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
                .map_err(|_| EdgeError::bad_request("invalid id"))?;
            Ok(format!("hello {}", id))
        }

        let service = RouterService::builder().get("/items/{id}", handler).build();
        let ok_request = request_builder()
            .method(Method::GET)
            .uri("/items/42")
            .body(Body::empty())
            .expect("request");
        let ok_response = block_on(service.clone().call(ok_request)).expect("response");
        assert_eq!(ok_response.status(), StatusCode::OK);
        assert_eq!(ok_response.body().as_bytes(), b"hello 42");

        let request = request_builder()
            .method(Method::GET)
            .uri("/items/abc")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn streams_body_through_router() {
        use bytes::Bytes;
        use futures_util::stream;
        use futures_util::StreamExt;

        async fn handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
            let chunks = stream::iter(vec![
                Bytes::from_static(b"chunk-one\n"),
                Bytes::from_static(b"chunk-two\n"),
            ]);

            Ok((StatusCode::OK, Body::stream(chunks)).into_response())
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
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("chunk");
                acc.extend_from_slice(&chunk);
            }
            acc
        });
        assert_eq!(collected, b"chunk-one\nchunk-two\n");
    }

    #[test]
    #[should_panic(expected = "route listing path cannot be empty")]
    fn route_listing_rejects_empty_path() {
        let _ = RouterService::builder().enable_route_listing_at("");
    }

    #[test]
    #[should_panic(expected = "route listing path must begin with '/'")]
    fn route_listing_rejects_missing_slash() {
        let _ = RouterService::builder().enable_route_listing_at("routes");
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
        RouterService::builder()
            .get("/dup", ok_handler)
            .get("/dup", ok_handler)
            .build();
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
            .middleware_arc(Arc::new(second) as BoxMiddleware)
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
    fn oneshot_returns_success_response() {
        let service = RouterService::builder().get("/ok", ok_handler).build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/ok")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request));
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn oneshot_returns_error_response() {
        let service = RouterService::builder().build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/missing")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request));
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
}
