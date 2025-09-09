use log;
use std::collections::{BTreeSet, HashMap};

use crate::handler::{BoxHandler, Handler};
use crate::http::{header, Method, Request, Response};

pub struct Router {
    routes: Vec<Route>,
}

struct Route {
    method: Method,
    segments: Vec<Segment>,
    handler: BoxHandler,
    mode: BodyMode,
}

#[derive(Clone, PartialEq, Eq)]
enum Segment {
    Static(String),
    Param(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    Auto,
    Streaming,
    Buffered,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    pub fn add<H>(&mut self, method: Method, path: &str, handler: H, mode: BodyMode)
    where
        H: Handler,
    {
        log::info!("Adding route: {} {}", method.as_str(), path);

        let segments = parse_segments(path);
        self.routes.push(Route {
            method,
            segments,
            handler: Box::new(handler),
            mode,
        });
    }

    pub fn route(&self, req: Request) -> Response {
        // Collect all routes that match the path
        let mut candidates: Vec<(&Route, HashMap<String, String>)> = Vec::new();
        for route in &self.routes {
            if let Some(params) = match_path(&route.segments, &req.path) {
                candidates.push((route, params));
            }
        }
        if candidates.is_empty() {
            return Response::not_found();
        }

        // HEAD behaves like GET with no body
        let is_head = req.method == Method::HEAD;
        let desired = if is_head {
            Method::GET
        } else {
            req.method.clone()
        };

        // Find a matching method among candidates
        for (route, params) in &candidates {
            if route.method == desired {
                let mut req2 = req.clone();
                req2.params = params.clone();
                let mut res = route.handler.handle(req2);
                if is_head {
                    res.clear_body();
                }
                // Apply body mode policy
                match route.mode {
                    BodyMode::Auto => return res,
                    BodyMode::Streaming => return res.into_streaming(),
                    BodyMode::Buffered => {
                        if res.is_streaming() {
                            return Response::new(500).text("Streaming not allowed for this route");
                        } else {
                            return res;
                        }
                    }
                }
            }
        }

        // If method is OPTIONS, return Allow header with 204
        let mut methods: BTreeSet<String> = BTreeSet::new();
        let mut has_get = false;
        for (route, _) in &candidates {
            if route.method == Method::GET {
                has_get = true;
            }
            methods.insert(route.method.as_str().to_string());
        }
        if has_get {
            methods.insert("HEAD".to_string());
        }
        methods.insert("OPTIONS".to_string());
        let allow = methods.into_iter().collect::<Vec<_>>().join(", ");

        if req.method == Method::OPTIONS {
            return Response::new(204).with_header(header::ALLOW, allow);
        }

        Response::new(405)
            .with_header(header::ALLOW, allow)
            .text("Method Not Allowed")
    }
}

fn parse_segments(path: &str) -> Vec<Segment> {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(param_name) = s.strip_prefix(':') {
                Segment::Param(param_name.to_string())
            } else {
                Segment::Static(s.to_string())
            }
        })
        .collect()
}

fn match_path(segments: &[Segment], path: &str) -> Option<HashMap<String, String>> {
    let parts: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (seg, part) in segments.iter().zip(parts.iter()) {
        match seg {
            Segment::Static(s) if s == part => {}
            Segment::Static(_) => return None,
            Segment::Param(name) => {
                params.insert(name.clone(), (*part).to_string());
            }
        }
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Method, Request, Response};

    #[test]
    fn router_static_route_matches() {
        let mut app = App::new();
        app.get("/hi", |_req: Request| Response::ok().text("hi"));
        let res = app.handle(Request::new(Method::GET, "/hi"));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(String::from_utf8(res.body).unwrap(), "hi");
    }

    #[test]
    fn router_param_route_extracts() {
        let mut app = App::new();
        app.get("/users/:id", |req: Request| {
            let id = req.param("id").unwrap_or("");
            Response::ok().text(id)
        });
        let res = app.handle(Request::new(Method::GET, "/users/42"));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(String::from_utf8(res.body).unwrap(), "42");
    }

    #[test]
    fn not_found_when_path_missing() {
        let app = App::new();
        let res = app.handle(Request::new(Method::GET, "/nope"));
        assert_eq!(res.status.as_u16(), 404);
    }

    #[test]
    fn method_not_allowed_with_allow_header() {
        let mut app = App::new();
        app.get("/hi", |_req: Request| Response::ok().text("hi"));
        let res = app.handle(Request::new(Method::POST, "/hi"));
        assert_eq!(res.status.as_u16(), 405);
        let allow = res
            .headers
            .get(header::ALLOW)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(allow.contains("GET"));
        assert!(allow.contains("HEAD"));
        assert!(allow.contains("OPTIONS"));
    }

    #[test]
    fn options_returns_allow_header() {
        let mut app = App::new();
        app.get("/hi", |_req: Request| Response::ok().text("hi"));
        let res = app.handle(Request::new(Method::OPTIONS, "/hi"));
        assert_eq!(res.status.as_u16(), 204);
        let allow = res
            .headers
            .get(header::ALLOW)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(allow.contains("GET") && allow.contains("HEAD") && allow.contains("OPTIONS"));
    }

    #[test]
    fn head_behaves_like_get_without_body() {
        let mut app = App::new();
        app.get("/hi", |_req: Request| Response::ok().text("hi"));
        let res = app.handle(Request::new(Method::HEAD, "/hi"));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(res.body.len(), 0);
    }

    #[test]
    fn streaming_route_coerces_buffered_body_to_streaming() {
        let mut app = App::new();
        app.route_with(
            Method::GET,
            "/s",
            |_req: Request| Response::ok().with_body("hello"),
            crate::app::RouteOptions::streaming(),
        );

        let mut res = app.handle(Request::new(Method::GET, "/s"));
        assert_eq!(res.status.as_u16(), 200);
        assert!(res.is_streaming());
        assert_eq!(res.content_len(), None);
        // Body should be empty and data provided via stream iterator
        assert!(res.body.is_empty());
        let collected = if let Some(mut it) = res.stream.take() {
            let mut all = Vec::new();
            for c in &mut it {
                all.extend_from_slice(&c);
            }
            all
        } else {
            Vec::new()
        };
        assert_eq!(String::from_utf8(collected).unwrap(), "hello");
    }

    #[test]
    fn buffered_route_rejects_streaming_handlers() {
        let mut app = App::new();
        app.route_with(
            Method::GET,
            "/b",
            |_req: Request| Response::ok().with_chunks(vec![b"part1".to_vec(), b"part2".to_vec()]),
            crate::app::RouteOptions::buffered(),
        );
        let res = app.handle(Request::new(Method::GET, "/b"));
        assert_eq!(res.status.as_u16(), 500);
        assert!(!res.is_streaming());
        assert_eq!(
            String::from_utf8(res.body).unwrap(),
            "Streaming not allowed for this route"
        );
    }
}
