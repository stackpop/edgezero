use bytes::Bytes;
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::{Headers, Json, Path};
use edgezero_core::http::{self, Response, StatusCode, Uri};
use edgezero_core::proxy::ProxyRequest;
use edgezero_core::response::Text;
use futures::{stream, StreamExt};

const DEFAULT_PROXY_BASE: &str = "https://httpbin.org";

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
    Text::new("EdgeZero Demo App")
}

#[action]
pub(crate) async fn echo(Path(params): Path<EchoParams>) -> Text<String> {
    Text::new(format!("Hello, {}!", params.name))
}

#[action]
pub(crate) async fn headers(Headers(headers): Headers) -> Text<String> {
    let ua = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(unknown)");
    Text::new(format!("ua={}", ua))
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

#[derive(serde::Deserialize)]
struct ProxyPath {
    #[serde(default)]
    rest: String,
}

#[action]
pub(crate) async fn proxy_demo(RequestContext(ctx): RequestContext) -> Result<Response, EdgeError> {
    let params: ProxyPath = ctx.path()?;
    let proxy_handle = ctx.proxy_handle();
    let request = ctx.into_request();
    let target = build_proxy_target(&params.rest, request.uri())?;
    let proxy_request = ProxyRequest::from_request(request, target);
    if let Some(handle) = proxy_handle {
        handle.forward(proxy_request).await
    } else {
        proxy_not_available_response()
    }
}

fn build_proxy_target(rest: &str, original_uri: &Uri) -> Result<Uri, EdgeError> {
    let base = std::env::var("API_BASE_URL").unwrap_or_else(|_| DEFAULT_PROXY_BASE.to_string());
    let mut target = base.trim_end_matches('/').to_string();
    let trimmed_rest = rest.trim_start_matches('/');
    if !trimmed_rest.is_empty() {
        target.push('/');
        target.push_str(trimmed_rest);
    }

    if let Some(query) = original_uri.query() {
        if !query.is_empty() {
            target.push('?');
            target.push_str(query);
        }
    }

    target
        .parse::<Uri>()
        .map_err(|err| EdgeError::bad_request(format!("invalid proxy target URI: {err}")))
}

fn proxy_not_available_response() -> Result<Response, EdgeError> {
    let body = Body::text(
        "proxy example is not enabled for this adapter build; enable a proxy-capable adapter",
    );
    http::response_builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .map_err(EdgeError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use edgezero_core::body::Body;
    use edgezero_core::context::RequestContext;
    use edgezero_core::http::header::{HeaderName, HeaderValue};
    use edgezero_core::http::{request_builder, Method, StatusCode, Uri};
    use edgezero_core::params::PathParams;
    use edgezero_core::proxy::{ProxyClient, ProxyHandle, ProxyResponse};
    use edgezero_core::response::IntoResponse;
    use edgezero_core::router::DEFAULT_ROUTE_LISTING_PATH;
    use futures::{executor::block_on, StreamExt};
    use std::collections::HashMap;
    use std::env;

    #[test]
    fn root_returns_static_body() {
        let ctx = empty_context("/");
        let response = block_on(root(ctx)).expect("handler ok").into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"EdgeZero Demo App");
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

        let response = block_on(headers(ctx)).expect("handler ok").into_response();
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

    #[test]
    fn build_proxy_target_merges_rest_and_query() {
        env::set_var("API_BASE_URL", "https://example.com/api");
        let original = Uri::from_static("/proxy/status?foo=bar");
        let target = super::build_proxy_target("status/200", &original).expect("target uri");
        assert_eq!(
            target.to_string(),
            "https://example.com/api/status/200?foo=bar"
        );
        env::remove_var("API_BASE_URL");
    }

    #[test]
    fn proxy_demo_without_proxy_support_returns_placeholder() {
        env::set_var("API_BASE_URL", "https://example.com/api");

        let ctx = context_with_params("/proxy/status/200", &[("rest", "status/200")]);
        let response = block_on(proxy_demo(ctx)).expect("response");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);

        env::remove_var("API_BASE_URL");
    }

    struct TestProxyClient;

    #[async_trait(?Send)]
    impl ProxyClient for TestProxyClient {
        async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
            let (_method, uri, _headers, _body, _) = request.into_parts();
            assert!(uri.to_string().contains("status/201"));
            Ok(ProxyResponse::new(StatusCode::CREATED, Body::empty()))
        }
    }

    #[test]
    fn proxy_demo_uses_injected_proxy_handle() {
        env::set_var("API_BASE_URL", "https://example.com/api");

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/proxy/status/201")
            .body(Body::empty())
            .expect("request");
        request
            .extensions_mut()
            .insert(ProxyHandle::with_client(TestProxyClient));

        let mut params = HashMap::new();
        params.insert("rest".to_string(), "status/201".to_string());
        let ctx = RequestContext::new(request, PathParams::new(params));

        let response = block_on(proxy_demo(ctx)).expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);

        env::remove_var("API_BASE_URL");
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
