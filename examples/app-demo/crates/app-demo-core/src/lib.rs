use std::sync::Arc;

use anyedge_core::{
    action, Body, EdgeError, Json, Path, RequestContext, Response, RouterService, StatusCode, Text,
};
use bytes::Bytes;
use futures::{stream, StreamExt};

#[derive(serde::Deserialize)]
struct EchoParams {
    name: String,
}

#[derive(serde::Deserialize)]
struct EchoBody {
    name: String,
}

#[action]
async fn root() -> Text<&'static str> {
    Text::new("AnyEdge Demo App")
}

#[anyedge_core::action]
async fn echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("Hello, {}!", params.name))
}

async fn headers(ctx: RequestContext) -> Result<Text<String>, EdgeError> {
    let ua = ctx
        .request()
        .headers()
        .get("User-Agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(unknown)");
    Ok(Text::new(format!("ua={}", ua)))
}

#[anyedge_core::action]
async fn stream() -> Response {
    let body =
        Body::stream(stream::iter(0..5).map(|index| Bytes::from(format!("chunk {}\n", index))));

    anyedge_core::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .expect("static stream response")
}

#[anyedge_core::action]
async fn echo_json(Json(body): Json<EchoBody>) -> Text<String> {
    Text::new(format!("Hello, {}!", body.name))
}

pub struct AppInfo {
    pub name: String,
}

pub struct DemoApp;

impl DemoApp {
    pub fn build_app() -> anyedge_core::App {
        build_app_with_info(AppInfo {
            name: "AnyEdge".to_string(),
        })
    }
}

pub fn build_app_with_info(info: AppInfo) -> anyedge_core::App {
    let info = Arc::new(info);
    let router = build_router(info);
    anyedge_core::App::new(router)
}

pub fn build_router(info: Arc<AppInfo>) -> RouterService {
    RouterService::builder()
        .get("/", root)
        .get("/echo/{name}", echo)
        .get("/headers", headers)
        .get("/stream", stream)
        .post("/echo", echo_json)
        .get("/info", {
            let info = Arc::clone(&info);
            move |_ctx: RequestContext| {
                let info = Arc::clone(&info);
                async move { Ok::<_, EdgeError>(Text::new(format!("App: {}", info.name))) }
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{request_builder, Body, Method};
    use futures::executor::block_on;

    #[test]
    fn root_ok() {
        let app = DemoApp::build_app();
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");

        let response = block_on(app.router().oneshot(request));
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn info_reads_state() {
        let app = DemoApp::build_app();
        let request = request_builder()
            .method(Method::GET)
            .uri("/info")
            .body(Body::empty())
            .expect("request");

        let response = block_on(app.router().oneshot(request));
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().into_bytes();
        assert_eq!(body.as_ref(), b"App: AnyEdge");
    }
}
