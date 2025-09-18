use anyedge_controller::{
    action, get, post, AppRoutes, Hooks, Path, RequestJson, Responder, Routes, State, Text,
};
use anyedge_core::{middleware::Logger, App, Request, Response};
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct EchoParams {
    name: String,
}

#[derive(serde::Deserialize)]
struct EchoBody {
    name: String,
}

#[action]
async fn root() -> impl Responder {
    Text::new("AnyEdge Demo App")
}

#[action]
async fn echo(Path(params): Path<EchoParams>) -> impl Responder {
    let EchoParams { name } = params;
    Text::new(format!("Hello, {}!", name))
}

#[action]
async fn headers(req: Request) -> impl Responder {
    let ua = req.header("User-Agent").unwrap_or("(unknown)");
    Text::new(format!("ua={}", ua))
}

#[action]
async fn stream() -> Response {
    let chunks: Vec<Vec<u8>> = (0..5)
        .map(|i| format!("chunk {}\n", i).into_bytes())
        .collect();
    Response::ok()
        .with_header("Content-Type", "text/plain; charset=utf-8")
        .with_chunks(chunks)
}

#[action]
async fn echo_json(RequestJson(body): RequestJson<EchoBody>) -> impl Responder {
    Text::new(format!("Hello, {}!", body.name))
}

#[action]
async fn info(State(info): State<AppInfo>) -> impl Responder {
    Text::new(format!("App: {}", info.name))
}

pub struct AppInfo {
    pub name: String,
}

pub struct DemoApp;

impl Hooks for DemoApp {
    fn configure(app: &mut App) {
        app.middleware(Logger);
        app.with_state(Arc::new(AppInfo {
            name: "AnyEdge".into(),
        }));
    }

    fn routes() -> AppRoutes {
        AppRoutes::with_default_routes().add_route(routes())
    }
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/", get(root()))
        .add("/echo/:name", get(echo()))
        .add("/headers", get(headers()))
        .add("/stream", get(stream()))
        .add("/echo", post(echo_json()))
        .add("/info", get(info()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{Method, Request};
    use futures::executor::block_on;

    #[test]
    fn root_ok() {
        let app = DemoApp::build_app();
        let res = block_on(app.handle(Request::new(Method::GET, "/")));
        assert_eq!(res.status.as_u16(), 200);
    }

    #[test]
    fn info_reads_state_async() {
        let app = DemoApp::build_app();
        let res = block_on(app.handle(Request::new(Method::GET, "/info")));
        assert_eq!(res.status.as_u16(), 200);
        assert_eq!(String::from_utf8(res.body).unwrap(), "App: AnyEdge");
    }
}
