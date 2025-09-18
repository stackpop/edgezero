#[cfg(feature = "cloudflare")]
use worker::{Headers, Method as WMethod, Request as WRequest, Response as WResponse};

use anyedge_core::{header, HeaderName, HeaderValue, Method, Request, Response};
use std::collections::HashMap;

#[cfg(feature = "cloudflare")]
pub async fn to_anyedge_request(mut req: WRequest) -> worker::Result<Request> {
    let method = map_method(req.method());
    let url = req.url()?;
    let path = url.path().to_string();
    let body = req.bytes().await.unwrap_or_default();

    let mut areq = Request::new(method, path).with_body(body);

    // Headers
    let headers: Headers = req.headers().clone();
    for (name, value) in headers.entries() {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            areq.headers.append(n, v);
        }
    }

    // Query params
    let mut qp: HashMap<String, String> = HashMap::new();
    for (k, v) in url.query_pairs() {
        qp.insert(k.to_string(), v.to_string());
    }
    areq.query_params = qp;

    Ok(areq)
}

#[cfg(feature = "cloudflare")]
pub fn from_anyedge_response(res: Response) -> worker::Result<WResponse> {
    let status = res.status.as_u16() as u16;
    let body_len = res.body.len();
    let has_len_header = res.headers.get(header::CONTENT_LENGTH).is_some();
    let streaming = res.is_streaming();
    let mut resp = WResponse::from_bytes(res.body)?;
    resp = resp.with_status(status);
    {
        let hdrs = resp.headers_mut();
        for (k, v) in res.headers.iter() {
            // Cloudflare headers are case-insensitive; use as-is
            hdrs.set(k.as_str(), v.to_str().unwrap_or("")).ok();
        }
        // Ensure content-length if not present and not streaming
        if !streaming && !has_len_header {
            hdrs.set("Content-Length", &body_len.to_string()).ok();
        }
    }
    Ok(resp)
}

// No stub definitions here; see `stub.rs` for feature-disabled builds.

#[cfg(feature = "cloudflare")]
fn map_method(m: WMethod) -> Method {
    match m {
        WMethod::Get => Method::GET,
        WMethod::Post => Method::POST,
        WMethod::Put => Method::PUT,
        WMethod::Delete => Method::DELETE,
        WMethod::Head => Method::HEAD,
        WMethod::Options => Method::OPTIONS,
        WMethod::Patch => Method::PATCH,
        other => Method::from_bytes(other.to_string().as_bytes()).unwrap_or(Method::GET),
    }
}

#[cfg(all(test, feature = "cloudflare"))]
mod tests {
    use super::*;

    #[test]
    fn map_method_maps_basic_verbs() {
        assert_eq!(super::map_method(WMethod::Get), Method::GET);
        assert_eq!(super::map_method(WMethod::Post), Method::POST);
        assert_eq!(super::map_method(WMethod::Put), Method::PUT);
        assert_eq!(super::map_method(WMethod::Delete), Method::DELETE);
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn from_anyedge_response_sets_status_headers_and_length() {
        let res = Response::new(201)
            .with_header("X-Test", "ok")
            .with_body(b"hello".to_vec());
        let wr = from_anyedge_response(res).expect("cloudflare response");
        assert_eq!(wr.status_code(), 201);
        let hdrs = wr.headers();
        assert_eq!(hdrs.get("X-Test").unwrap().as_deref(), Some("ok"));
        assert_eq!(hdrs.get("Content-Length").unwrap().as_deref(), Some("5"));
    }
}
