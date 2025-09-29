use anyedge_core::action;
use anyedge_core::body::Body;
use anyedge_core::context::RequestContext;
use anyedge_core::error::EdgeError;
use anyedge_core::extractor::{Json, Path};
use anyedge_core::http::{self, Response, StatusCode};
use anyedge_core::response::Text;
use bytes::Bytes;
use futures::{stream, StreamExt};

#[derive(serde::Deserialize)]
pub(crate) struct EchoParams {
    pub(crate) name: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct EchoBody {
    pub(crate) name: String,
}

#[action]
pub(crate) async fn root() -> Text<&'static str> {
    Text::new("AnyEdge Demo App")
}

#[action]
pub(crate) async fn echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("Hello, {}!", params.name))
}

pub(crate) async fn headers(ctx: RequestContext) -> Result<Text<String>, EdgeError> {
    let ua = ctx
        .request()
        .headers()
        .get("User-Agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(unknown)");
    Ok(Text::new(format!("ua={}", ua)))
}

#[action]
pub(crate) async fn stream() -> Response {
    let body =
        Body::stream(stream::iter(0..5).map(|index| Bytes::from(format!("chunk {}\n", index))));

    http::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .expect("static stream response")
}

#[action]
pub(crate) async fn echo_json(Json(body): Json<EchoBody>) -> Text<String> {
    Text::new(format!("Hello, {}!", body.name))
}

#[derive(serde::Serialize)]
struct RouteSummary {
    method: String,
    path: String,
}

#[action]
pub(crate) async fn list_routes() -> Result<Response, EdgeError> {
    let routes = crate::build_router().routes();
    let payload: Vec<RouteSummary> = routes
        .into_iter()
        .map(|route| RouteSummary {
            method: route.method().as_str().to_string(),
            path: route.path().to_string(),
        })
        .collect();

    let body = Body::json(&payload).map_err(EdgeError::internal)?;
    let response = http::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(body)
        .map_err(EdgeError::internal)?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::body::Body;
    use anyedge_core::context::RequestContext;
    use anyedge_core::http::header::HeaderName;
    use anyedge_core::http::{request_builder, HeaderValue, Method, StatusCode};
    use anyedge_core::params::PathParams;
    use anyedge_core::response::IntoResponse;
    use anyedge_core::router::DEFAULT_ROUTE_LISTING_PATH;
    use futures::{executor::block_on, StreamExt};
    use std::collections::HashMap;

    #[test]
    fn root_returns_static_body() {
        let ctx = empty_context("/");
        let response = block_on(root(ctx)).expect("handler ok").into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"AnyEdge Demo App");
    }

    #[test]
    fn echo_formats_name_from_path() {
        let ctx = context_with_params("/echo/alice", &[("name", "alice")]);
        let response = block_on(echo(ctx)).expect("handler ok").into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"Hello, alice!");
    }

    #[test]
    fn headers_reports_user_agent() {
        let ctx = context_with_header(
            "/headers",
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("DemoAgent"),
        );

        let response = block_on(headers(ctx))
            .expect("handler result")
            .into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"ua=DemoAgent");
    }

    #[test]
    fn stream_emits_expected_chunks() {
        let ctx = empty_context("/stream");
        let response = block_on(stream(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::OK);

        let mut chunks = response.into_body().into_stream().expect("stream body");
        let collected = block_on(async {
            let mut buf = Vec::new();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.expect("chunk");
                buf.extend_from_slice(&chunk);
            }
            buf
        });
        assert_eq!(
            String::from_utf8(collected).expect("utf8"),
            "chunk 0\nchunk 1\nchunk 2\nchunk 3\nchunk 4\n"
        );
    }

    #[test]
    fn echo_json_formats_payload() {
        let ctx = context_with_json("/echo", r#"{"name":"Edge"}"#);
        let response = block_on(echo_json(ctx))
            .expect("handler ok")
            .into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"Hello, Edge!");
    }

    #[test]
    fn list_routes_returns_manifest_entries() {
        let ctx = empty_context(DEFAULT_ROUTE_LISTING_PATH);
        let response = block_on(list_routes(ctx)).expect("handler ok");
        assert_eq!(response.status(), StatusCode::OK);

        let payload: serde_json::Value =
            serde_json::from_slice(response.body().as_bytes()).expect("json payload");
        let routes = payload.as_array().expect("array");
        assert!(routes.iter().any(|entry| {
            entry["method"] == "GET" && entry["path"] == DEFAULT_ROUTE_LISTING_PATH
        }));
        assert!(routes
            .iter()
            .any(|entry| { entry["method"] == "GET" && entry["path"] == "/echo/{name}" }));
    }

    fn empty_context(path: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    fn context_with_params(path: &str, params: &[(&str, &str)]) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let map = params
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<HashMap<_, _>>();
        RequestContext::new(request, PathParams::new(map))
    }

    fn context_with_header(path: &str, header: HeaderName, value: HeaderValue) -> RequestContext {
        let mut request = request_builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        request.headers_mut().insert(header, value);
        RequestContext::new(request, PathParams::default())
    }

    fn context_with_json(path: &str, json: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::from(json))
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }
}
