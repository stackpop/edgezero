use crate::context::SpinRequestContext;
use crate::proxy::SpinProxyClient;
use crate::response::from_core_response;
use edgezero_core::app::App;
use edgezero_core::body::Body;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{request_builder, Request, Uri};
use edgezero_core::proxy::ProxyHandle;
use spin_sdk::http::{IncomingRequest, IntoResponse};

/// Convert a Spin `IncomingRequest` into an EdgeZero core `Request`.
///
/// Reads the full body into a buffered `Body::Once`, inserts
/// `SpinRequestContext` and a `ProxyHandle` into extensions.
pub async fn into_core_request(req: IncomingRequest) -> Result<Request, EdgeError> {
    let method = req.method();
    let path_with_query = req.path_with_query().unwrap_or_else(|| "/".to_string());

    let uri: Uri = path_with_query
        .parse()
        .map_err(|err| EdgeError::bad_request(format!("invalid URI: {}", err)))?;

    // Extract headers before consuming the request body. The WASI `headers()`
    // handle borrows the request and must be dropped before `into_body()`.
    let headers = req.headers();
    let header_entries = headers.entries();

    let mut builder = request_builder()
        .method(into_core_method(&method))
        .uri(uri);

    for (name, value) in &header_entries {
        if let Ok(value_str) = std::str::from_utf8(value) {
            builder = builder.header(name.as_str(), value_str);
        }
    }

    let client_addr = find_header_string(&header_entries, "spin-client-addr");
    let full_url = find_header_string(&header_entries, "spin-full-url");

    // Drop the WASI resource handle before consuming the body.
    drop(headers);

    let body_bytes = req
        .into_body()
        .await
        .map_err(|e| EdgeError::bad_request(format!("failed to read request body: {}", e)))?;

    let mut request = builder
        .body(Body::from(body_bytes))
        .map_err(|e| EdgeError::bad_request(format!("failed to build request: {}", e)))?;

    SpinRequestContext::insert(
        &mut request,
        SpinRequestContext {
            client_addr,
            full_url,
        },
    );
    request
        .extensions_mut()
        .insert(ProxyHandle::with_client(SpinProxyClient));

    Ok(request)
}

/// Find a header value by name from pre-extracted header entries.
fn find_header_string(entries: &[(String, Vec<u8>)], name: &str) -> Option<String> {
    entries
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
}

/// Dispatch a Spin request through the EdgeZero router and return
/// a Spin-compatible response.
pub async fn dispatch(
    app: &App,
    req: IncomingRequest,
) -> anyhow::Result<impl IntoResponse> {
    let core_request = into_core_request(req).await?;
    let response = app.router().oneshot(core_request).await;
    Ok(from_core_response(response).await?)
}

fn into_core_method(method: &spin_sdk::http::Method) -> edgezero_core::http::Method {
    match method {
        spin_sdk::http::Method::Get => edgezero_core::http::Method::GET,
        spin_sdk::http::Method::Post => edgezero_core::http::Method::POST,
        spin_sdk::http::Method::Put => edgezero_core::http::Method::PUT,
        spin_sdk::http::Method::Delete => edgezero_core::http::Method::DELETE,
        spin_sdk::http::Method::Patch => edgezero_core::http::Method::PATCH,
        spin_sdk::http::Method::Head => edgezero_core::http::Method::HEAD,
        spin_sdk::http::Method::Options => edgezero_core::http::Method::OPTIONS,
        spin_sdk::http::Method::Connect => edgezero_core::http::Method::CONNECT,
        spin_sdk::http::Method::Trace => edgezero_core::http::Method::TRACE,
        spin_sdk::http::Method::Other(s) => {
            edgezero_core::http::Method::from_bytes(s.as_bytes())
                .unwrap_or(edgezero_core::http::Method::GET)
        }
    }
}
