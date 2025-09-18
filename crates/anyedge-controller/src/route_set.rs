use anyedge_core::{app::RouteOptions, router::BodyMode, Method};

use crate::handler::{ControllerHandler, IntoHandler};

#[derive(Default, Clone)]
pub struct RouteSet {
    prefix: Option<String>,
    entries: Vec<RouteEntry>,
}

#[derive(Clone)]
pub struct RouteEntry {
    method: Method,
    path: String,
    handler: ControllerHandler,
    body_mode: BodyMode,
}

#[derive(Clone)]
pub struct RouteSpec {
    method: Method,
    handler: ControllerHandler,
    body_mode: BodyMode,
}

impl RouteSpec {
    pub fn new(method: Method, handler: ControllerHandler) -> Self {
        Self {
            method,
            handler,
            body_mode: BodyMode::Auto,
        }
    }

    pub fn method(&self) -> Method {
        self.method.clone()
    }

    pub fn handler(&self) -> ControllerHandler {
        self.handler.clone()
    }

    pub fn body_mode(&self) -> BodyMode {
        self.body_mode
    }

    pub fn with_options(mut self, options: RouteOptions) -> Self {
        self.body_mode = options.body_mode;
        self
    }
}

impl RouteSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_prefix(prefix: &str) -> Self {
        let mut set = Self::new();
        set.prefix = Some(Self::normalize_prefix(prefix));
        set
    }

    pub fn set_prefix(&mut self, prefix: &str) -> &mut Self {
        self.prefix = Some(Self::normalize_prefix(prefix));
        self
    }

    pub fn prefix(mut self, prefix: &str) -> Self {
        self.set_prefix(prefix);
        self
    }

    pub fn add_route<H>(&mut self, method: Method, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        let full_path = Self::compose_path(self.prefix.as_deref(), path);
        self.push_entry(method, full_path, handler.into_handler(), BodyMode::Auto);
        self
    }

    pub fn add_route_spec(&mut self, path: &str, route: RouteSpec) -> &mut Self {
        let full_path = Self::compose_path(self.prefix.as_deref(), path);
        self.push_entry(
            route.method.clone(),
            full_path,
            route.handler(),
            route.body_mode(),
        );
        self
    }

    pub fn add(mut self, path: &str, route: RouteSpec) -> Self {
        self.add_route_spec(path, route);
        self
    }

    pub fn get<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::GET, path, handler)
    }

    pub fn post<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::POST, path, handler)
    }

    pub fn put<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::PUT, path, handler)
    }

    pub fn delete<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::DELETE, path, handler)
    }

    pub fn patch<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::PATCH, path, handler)
    }

    pub fn head<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::HEAD, path, handler)
    }

    pub fn options<H>(&mut self, path: &str, handler: H) -> &mut Self
    where
        H: IntoHandler,
    {
        self.add_route(Method::OPTIONS, path, handler)
    }

    pub fn merge(&mut self, other: RouteSet) -> &mut Self {
        self.entries.extend(other.entries);
        self
    }

    pub fn merge_all<I>(&mut self, sets: I) -> &mut Self
    where
        I: IntoIterator<Item = RouteSet>,
    {
        for set in sets {
            self.merge(set);
        }
        self
    }

    pub fn nest(&mut self, prefix: &str, set: RouteSet) -> &mut Self {
        let nested_prefix = Self::normalize_prefix(prefix);
        for entry in set.entries {
            let combined = Self::compose_path(Some(&nested_prefix), &entry.path);
            self.push_entry(entry.method, combined, entry.handler, entry.body_mode);
        }
        self
    }

    pub fn apply(self, app: &mut anyedge_core::App) {
        for entry in self.entries {
            app.route_with(
                entry.method,
                &entry.path,
                entry.handler,
                RouteOptions {
                    body_mode: entry.body_mode,
                },
            );
        }
    }

    fn push_entry(
        &mut self,
        method: Method,
        path: String,
        handler: ControllerHandler,
        body_mode: BodyMode,
    ) {
        self.entries.push(RouteEntry {
            method,
            path,
            handler,
            body_mode,
        });
    }

    fn normalize_prefix(prefix: &str) -> String {
        let trimmed = prefix.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let mut segments: Vec<String> = Vec::new();
        for segment in trimmed.split('/') {
            let s = segment.trim();
            if s.is_empty() {
                continue;
            }
            segments.push(s.to_string());
        }
        format!("/{}", segments.join("/"))
    }

    fn compose_path(prefix: Option<&str>, path: &str) -> String {
        let mut segments = Vec::new();
        if let Some(pref) = prefix {
            for segment in pref.split('/') {
                if segment.is_empty() {
                    continue;
                }
                segments.push(segment);
            }
        }
        for segment in path.split('/') {
            let s = segment.trim();
            if s.is_empty() {
                continue;
            }
            segments.push(s);
        }
        if segments.is_empty() {
            return "/".to_string();
        }
        format!("/{}", segments.join("/"))
    }
}

#[allow(dead_code)]
impl RouteEntry {
    pub fn method(&self) -> Method {
        self.method.clone()
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn handler(&self) -> &ControllerHandler {
        &self.handler
    }

    pub fn body_mode(&self) -> BodyMode {
        self.body_mode
    }
}

pub fn route_with<H>(method: Method, handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    RouteSpec::new(method, handler.into_handler())
}

pub fn get<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::GET, handler)
}

pub fn post<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::POST, handler)
}

pub fn put<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::PUT, handler)
}

pub fn delete<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::DELETE, handler)
}

pub fn patch<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::PATCH, handler)
}

pub fn head<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::HEAD, handler)
}

pub fn options<H>(handler: H) -> RouteSpec
where
    H: IntoHandler,
{
    route_with(Method::OPTIONS, handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action, get, post, Path, Responder, State, Text, ValidatedJson};
    use anyedge_core::{app::RouteOptions, App, Method, Request, Response};
    use futures::executor::block_on;
    use serde_json::json;
    use validator::Validate;

    #[derive(serde::Deserialize)]
    struct Params {
        slug: String,
    }

    #[action]
    fn list(path: Path<Params>) -> impl crate::responder::Responder {
        let Path(params) = path;
        crate::responder::Text::new(format!("list:{}", params.slug))
    }

    #[action]
    fn create() -> impl crate::responder::Responder {
        crate::responder::Text::new("create")
    }

    #[test]
    fn normalize_prefix_cleans_input() {
        assert_eq!(RouteSet::normalize_prefix("//api//v1//"), "/api/v1");
        assert_eq!(RouteSet::normalize_prefix("/"), "/");
    }

    #[test]
    fn compose_path_handles_prefixes() {
        assert_eq!(RouteSet::compose_path(Some("/api"), "/notes"), "/api/notes");
        assert_eq!(RouteSet::compose_path(None, "/notes"), "/notes");
    }

    #[test]
    fn route_set_registers_handlers() {
        let mut app = App::new();
        RouteSet::new()
            .add("/notes/:slug", get(list()))
            .add("/notes", post(create()))
            .apply(&mut app);

        let mut req = Request::new(Method::GET, "/notes/demo");
        req.params.insert("slug".into(), "demo".into());
        let res = block_on(app.handle(req));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(String::from_utf8(res.body).unwrap(), "list:demo");

        let res = block_on(app.handle(Request::new(Method::POST, "/notes")));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(String::from_utf8(res.body).unwrap(), "create");
    }

    #[test]
    fn route_spec_streaming_coerces_body() {
        let mut app = App::new();
        RouteSet::new()
            .add(
                "/stream",
                get(|_req: Request| Response::ok().with_body("stream".as_bytes().to_vec()))
                    .with_options(RouteOptions::streaming()),
            )
            .apply(&mut app);

        let mut res = block_on(app.handle(Request::new(Method::GET, "/stream")));
        assert_eq!(res.status.as_u16(), 200);
        assert!(res.is_streaming());
        assert!(res.body.is_empty());

        let collected = if let Some(mut it) = res.stream.take() {
            let mut all = Vec::new();
            while let Some(chunk) = it.next() {
                all.extend_from_slice(&chunk);
            }
            all
        } else {
            Vec::new()
        };
        assert_eq!(String::from_utf8(collected).unwrap(), "stream");
    }

    #[test]
    fn route_spec_buffered_rejects_streaming() {
        let mut app = App::new();
        RouteSet::new()
            .add(
                "/buffered",
                get(|_req: Request| {
                    Response::ok().with_chunks(vec![b"part1".to_vec(), b"part2".to_vec()])
                })
                .with_options(RouteOptions::buffered()),
            )
            .apply(&mut app);

        let res = block_on(app.handle(Request::new(Method::GET, "/buffered")));
        assert_eq!(res.status.as_u16(), 500);
        assert!(!res.is_streaming());
        assert_eq!(
            String::from_utf8(res.body).unwrap(),
            "Streaming not allowed for this route"
        );
    }

    #[test]
    fn route_set_nested_prefix_applies_to_routes() {
        let mut app = App::new();
        let nested = RouteSet::new().add("/list", get(create()));
        let mut routes = RouteSet::new();
        routes.nest("/api/v1", nested);
        routes.add_route_spec("/api/status", get(create()));
        routes.apply(&mut app);

        let res = block_on(app.handle(Request::new(Method::GET, "/api/v1/list")));
        assert_eq!(res.status.as_u16(), 200);

        let res = block_on(app.handle(Request::new(Method::GET, "/api/status")));
        assert_eq!(res.status.as_u16(), 200);
    }

    #[derive(serde::Deserialize)]
    struct PlainParams {
        id: String,
    }

    #[action]
    async fn plain(Path(params): Path<PlainParams>) -> impl Responder {
        let PlainParams { id } = params;
        Text::new(format!("plain:{}", id))
    }

    #[test]
    fn function_routes_register_without_macros() {
        let mut app = App::new();
        RouteSet::new()
            .prefix("api")
            .add("/plain/:id", get(plain()))
            .apply(&mut app);

        let mut req = Request::new(Method::GET, "/api/plain/demo");
        req.params.insert("id".into(), "demo".into());
        let res = block_on(app.handle(req));
        let body = String::from_utf8(res.body).unwrap();
        assert_eq!(res.status.as_u16(), 200, "body: {body}");
        assert_eq!(body, "plain:demo");
    }

    #[derive(serde::Deserialize)]
    struct NeedsState;

    #[action]
    fn state_handler(_: State<usize>) -> impl Responder {
        Text::new("state")
    }

    #[test]
    fn state_extractor_missing_returns_500() {
        let mut app = App::new();
        RouteSet::new()
            .add("/needs-state", get(state_handler()))
            .apply(&mut app);

        let res = block_on(app.handle(Request::new(Method::GET, "/needs-state")));
        assert_eq!(res.status.as_u16(), 500);
    }

    #[derive(serde::Deserialize, Validate)]
    struct ValidatedBody {
        #[validate(length(min = 1))]
        name: String,
    }

    #[action]
    fn validated_json_handler(
        ValidatedJson(payload): ValidatedJson<ValidatedBody>,
    ) -> impl Responder {
        Text::new(format!("hi {}", payload.name))
    }

    #[test]
    fn validated_json_invalid_returns_422() {
        let mut app = App::new();
        RouteSet::new()
            .add("/validated", post(validated_json_handler()))
            .apply(&mut app);

        let mut req = Request::new(Method::POST, "/validated");
        req.body = serde_json::to_vec(&json!({ "name": "" })).unwrap();

        let res = block_on(app.handle(req));
        assert_eq!(res.status.as_u16(), 422);
    }
}
