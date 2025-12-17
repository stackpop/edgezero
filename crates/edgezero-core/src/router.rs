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
    StatusCode,
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

                    let body = Body::json(&payload).map_err(EdgeError::internal)?;
                    let response = response_builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                        .body(body)
                        .map_err(EdgeError::internal)?;
                    Ok(response)
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
    use crate::http::{request_builder, Method, StatusCode};
    use futures::executor::block_on;
    use serde::Deserialize;
    use serde_json::json;

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
    }

    #[test]
    fn returns_method_not_allowed() {
        async fn handler(_ctx: RequestContext) -> Result<(), EdgeError> {
            Ok(())
        }

        let service = RouterService::builder().post("/submit", handler).build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/submit")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn returns_not_found() {
        let service = RouterService::builder().build();
        let request = request_builder()
            .method(Method::GET)
            .uri("/missing")
            .body(Body::empty())
            .expect("request");

        let error = block_on(service.clone().call(request)).expect_err("error");
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
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
}
