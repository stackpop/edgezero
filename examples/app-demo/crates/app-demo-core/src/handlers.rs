use bytes::Bytes;
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::extractor::{Headers, Json, Kv, Path};
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

#[derive(serde::Deserialize)]
struct ProxyPath {
    #[serde(default)]
    rest: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct NoteIdPath {
    pub(crate) id: String,
}

#[action]
pub(crate) async fn root() -> Text<&'static str> {
    Text::new("app-demo app")
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
        Body::stream(stream::iter(0..3).map(|index| Bytes::from(format!("chunk {}\n", index))));

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

// ---------------------------------------------------------------------------
// KV-powered handlers — demonstrate platform-neutral key-value storage.
// ---------------------------------------------------------------------------

/// Increment and return a visit counter stored in KV.
#[action]
pub(crate) async fn kv_counter(Kv(store): Kv) -> Result<Response, EdgeError> {
    let count: i64 = store.update("demo:counter", 0i64, |n| n + 1).await?;
    let body = serde_json::json!({ "count": count }).to_string();
    http::response_builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::text(body))
        .map_err(EdgeError::internal)
}

/// Store a note by id (body = note text).
#[action]
pub(crate) async fn kv_note_put(
    Kv(store): Kv,
    Path(path): Path<NoteIdPath>,
    RequestContext(ctx): RequestContext,
) -> Result<Response, EdgeError> {
    let body = ctx.into_request().into_body();
    let body_bytes = collect_body(body).await?;
    store.put_bytes(&format!("note:{}", path.id), body_bytes).await?;
    http::response_builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .map_err(EdgeError::internal)
}

/// Drain a [`Body`] into a single [`Bytes`] buffer, regardless of variant.
async fn collect_body(body: Body) -> Result<Bytes, EdgeError> {
    if body.is_stream() {
        let mut stream = body.into_stream().expect("checked is_stream");
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(EdgeError::internal)?;
            buf.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(buf))
    } else {
        Ok(body.into_bytes())
    }
}

/// Read a note by id.
#[action]
pub(crate) async fn kv_note_get(
    Kv(store): Kv,
    Path(path): Path<NoteIdPath>,
) -> Result<Response, EdgeError> {
    match store.get_bytes(&format!("note:{}", path.id)).await? {
        Some(data) => http::response_builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(data.to_vec()))
            .map_err(EdgeError::internal),
        None => Err(EdgeError::not_found(format!("note:{}", path.id))),
    }
}

/// Delete a note by id.
#[action]
pub(crate) async fn kv_note_delete(
    Kv(store): Kv,
    Path(path): Path<NoteIdPath>,
) -> Result<Response, EdgeError> {
    store.delete(&format!("note:{}", path.id)).await?;
    http::response_builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
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
    use futures::{executor::block_on, StreamExt};
    use std::collections::HashMap;
    use std::env;

    #[test]
    fn root_returns_static_body() {
        let ctx = empty_context("/");
        let response = block_on(root(ctx)).expect("handler ok").into_response();
        let bytes = response.into_body().into_bytes();
        assert_eq!(bytes.as_ref(), b"app-demo app");
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
            "chunk 0\nchunk 1\nchunk 2\n"
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
    fn build_proxy_target_merges_segments_and_query() {
        env::set_var("API_BASE_URL", "https://example.com/api");
        let original = Uri::from_static("/proxy/status?foo=bar");
        let target = build_proxy_target("status/200", &original).expect("target uri");
        assert_eq!(
            target.to_string(),
            "https://example.com/api/status/200?foo=bar"
        );
        env::remove_var("API_BASE_URL");
    }

    #[test]
    fn proxy_demo_without_handle_returns_placeholder() {
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
    fn proxy_demo_uses_injected_handle() {
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

    // -- KV handler tests --------------------------------------------------

    use edgezero_core::kv::{KvHandle, KvStore, KvError};
    use std::sync::{Arc, Mutex};
    use std::collections::BTreeMap;

    struct MockKv {
        data: Mutex<BTreeMap<String, Bytes>>,
    }
    impl MockKv {
        fn new() -> Self {
            Self { data: Mutex::new(BTreeMap::new()) }
        }
    }

    #[async_trait(?Send)]
    impl KvStore for MockKv {
        async fn get_bytes(&self, key: &str) -> Result<Option<Bytes>, KvError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        async fn put_bytes(&self, key: &str, value: Bytes) -> Result<(), KvError> {
            self.data.lock().unwrap().insert(key.to_string(), value);
            Ok(())
        }
        async fn put_bytes_with_ttl(&self, key: &str, value: Bytes, _ttl: std::time::Duration) -> Result<(), KvError> {
            self.data.lock().unwrap().insert(key.to_string(), value);
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), KvError> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
        async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, KvError> {
            Ok(self.data.lock().unwrap().keys().filter(|k| k.starts_with(prefix)).cloned().collect())
        }
    }

    fn context_with_kv(path: &str, method: Method, body: Body, params: &[(&str, &str)]) -> (RequestContext, KvHandle) {
        let kv = Arc::new(MockKv::new());
        let handle = KvHandle::new(kv);
        let mut request = request_builder()
            .method(method)
            .uri(path)
            .body(body)
            .expect("request");
        request.extensions_mut().insert(handle.clone());
        let map = params
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<HashMap<_, _>>();
        (RequestContext::new(request, PathParams::new(map)), handle)
    }

    #[test]
    fn kv_counter_increments() {
        let (ctx, _) = context_with_kv("/kv/counter", Method::GET, Body::empty(), &[]);
        let resp = block_on(kv_counter(ctx)).expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().into_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["count"], 1);
    }

    #[test]
    fn kv_note_put_and_get() {
        let (ctx, handle) = context_with_kv(
            "/kv/notes/abc",
            Method::POST,
            Body::from("hello world"),
            &[("id", "abc")],
        );
        let resp = block_on(kv_note_put(ctx)).expect("response");
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Now read back via get
        let (ctx2, _) = {
            let mut request = request_builder()
                .method(Method::GET)
                .uri("/kv/notes/abc")
                .body(Body::empty())
                .expect("request");
            request.extensions_mut().insert(handle.clone());
            let mut map = HashMap::new();
            map.insert("id".to_string(), "abc".to_string());
            (RequestContext::new(request, PathParams::new(map)), handle.clone())
        };
        let resp = block_on(kv_note_get(ctx2)).expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.into_body().into_bytes().as_ref(), b"hello world");
    }

    #[test]
    fn kv_note_get_missing_returns_404() {
        let (ctx, _) = context_with_kv("/kv/notes/xyz", Method::GET, Body::empty(), &[("id", "xyz")]);
        let err = block_on(kv_note_get(ctx)).expect_err("should be NotFound");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn kv_note_delete_returns_no_content() {
        let (ctx, handle) = context_with_kv(
            "/kv/notes/del",
            Method::POST,
            Body::from("to-delete"),
            &[("id", "del")],
        );
        block_on(kv_note_put(ctx)).unwrap();

        let (ctx2, _) = {
            let mut request = request_builder()
                .method(Method::DELETE)
                .uri("/kv/notes/del")
                .body(Body::empty())
                .expect("request");
            request.extensions_mut().insert(handle.clone());
            let mut map = HashMap::new();
            map.insert("id".to_string(), "del".to_string());
            (RequestContext::new(request, PathParams::new(map)), handle)
        };
        let resp = block_on(kv_note_delete(ctx2)).expect("response");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
