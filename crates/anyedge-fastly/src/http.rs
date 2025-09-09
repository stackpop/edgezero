use anyedge_core::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response};
use fastly::http::{
    HeaderName as FHeaderName, HeaderValue as FHeaderValue, Method as FMethod, StatusCode,
};
use fastly::{Body, Request as FRequest, Response as FResponse};
use std::collections::HashMap;

pub fn to_anyedge_request(mut req: FRequest) -> Request {
    let method = map_method(req.get_method());
    let path = req.get_path().to_string();
    let body = req.take_body_bytes();

    let mut areq = Request::new(method, path).with_body(body);
    copy_headers_fastly_to_core(req.get_headers(), &mut areq.headers);
    // Copy query parameters into the provider-agnostic request
    if let Ok(qs) = req.get_query::<HashMap<String, String>>() {
        areq.query_params = qs;
    }
    if let Some(ip) = req.get_client_ip_addr() {
        areq.ctx.insert("client_ip".to_string(), ip.to_string());
    }
    areq
}

pub fn from_anyedge_response(res: Response) -> FResponse {
    let status = StatusCode::from_u16(res.status.as_u16()).unwrap_or(StatusCode::OK);
    let mut fresp = FResponse::from_status(status);
    let mut has_len = apply_core_headers_to_fresp(&res.headers, &mut fresp);
    match res.stream {
        Some(mut iter) => {
            if try_stream_native(&mut fresp, &mut iter) {
                return fresp;
            }
            // Buffered fallback (non-wasm builds or testing environments)
            let mut all = Vec::new();
            for c in &mut iter {
                all.extend_from_slice(&c);
            }
            set_content_length_if_missing(&mut fresp, &mut has_len, all.len());
            fresp.set_body(Body::from(all));
        }
        None => {
            set_content_length_if_missing(&mut fresp, &mut has_len, res.body.len());
            fresp.set_body(Body::from(res.body));
        }
    }
    fresp
}

// Fastly native streaming is currently disabled behind a simple fallback.
// If/when we enable it, we can add a wasm-only implementation here.
fn try_stream_native<'a>(_fresp: &mut FResponse, _iter: &mut dyn Iterator<Item = Vec<u8>>) -> bool {
    false
}

fn map_method(m: &FMethod) -> Method {
    Method::from_bytes(m.as_str().as_bytes()).unwrap_or(Method::GET)
}

fn map_method_rev(m: &Method) -> FMethod {
    // Map core Method back to Fastly Method
    FMethod::from_bytes(m.as_str().as_bytes()).unwrap_or(FMethod::GET)
}

pub fn to_fastly_request(req: Request, base_url: &str) -> FRequest {
    let mut url = String::from(base_url);
    if !req.path.is_empty() {
        if !url.ends_with('/') && !req.path.starts_with('/') {
            url.push('/');
        }
        url.push_str(&req.path);
    }
    if !req.query_params.is_empty() {
        url.push('?');
        url.push_str(&encode_query(&req.query_params));
    }
    let mut fr = FRequest::new(map_method_rev(&req.method), &url);
    apply_core_headers_to_freq(&req.headers, &mut fr);
    fr.set_body(req.body);
    fr
}

pub fn to_anyedge_response(mut fresp: FResponse) -> Response {
    let mut res = Response::new(fresp.get_status().as_u16());
    for (name, value) in fresp.get_headers() {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            res.headers.append(n, v);
        }
    }
    let body = fresp.take_body_bytes();
    res.with_body(body)
}

// ---------- internal helpers (reduce duplication) ----------

fn copy_headers_fastly_to_core<'a, I>(iter: I, out: &mut HeaderMap)
where
    I: IntoIterator<Item = (&'a FHeaderName, &'a FHeaderValue)>,
{
    for (name, value) in iter {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
}

fn apply_core_headers_to_fresp(src: &HeaderMap, fresp: &mut FResponse) -> bool {
    let mut has_len = false;
    for (k, v) in src.iter() {
        if let (Ok(n), Ok(val)) = (
            FHeaderName::try_from(k.as_str()),
            FHeaderValue::from_bytes(v.as_bytes()),
        ) {
            fresp.set_header(n, val);
            if k.as_str().eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
        }
    }
    has_len
}

fn apply_core_headers_to_freq(src: &HeaderMap, freq: &mut FRequest) {
    for (k, v) in src.iter() {
        let _ = freq.set_header(k.as_str(), v.as_bytes());
    }
}

fn set_content_length_if_missing(fresp: &mut FResponse, has_len: &mut bool, len: usize) {
    if !*has_len {
        let name = FHeaderName::try_from("content-length").unwrap();
        let val = FHeaderValue::try_from(len.to_string().as_str()).unwrap();
        fresp.set_header(name, val);
        *has_len = true;
    }
}

fn encode_query(q: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut first = true;
    for (k, v) in q.iter() {
        if !first {
            out.push('&');
        } else {
            first = false;
        }
        use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
        out.push_str(&utf8_percent_encode(k, NON_ALPHANUMERIC).to_string());
        out.push('=');
        out.push_str(&utf8_percent_encode(v, NON_ALPHANUMERIC).to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_anyedge_copies_headers_and_body() {
        let mut fr = fastly::Request::get("http://example.com/path");
        fr.set_header("X-Test", "a");
        fr.set_header("X-Test", "b");
        fr.set_body("hello");
        let ar = to_anyedge_request(fr);
        assert_eq!(ar.path, "/path");
        let vals = ar.headers_all("x-test");
        assert_eq!(vals, vec!["b".to_string()]); // Fastly set_header replaces
        assert_eq!(ar.body, b"hello".to_vec());
    }

    #[test]
    fn from_anyedge_sets_content_length_when_missing() {
        let ar = Response::ok().with_body(b"abc".to_vec());
        // Do not set Content-Length explicitly
        let fr = from_anyedge_response(ar);
        let cl = fr.get_header("content-length").unwrap();
        assert_eq!(cl.to_str().unwrap(), "3");
    }

    #[test]
    fn to_fastly_request_sets_method_headers_body_and_query() {
        // Build core request
        let mut req = Request::new(Method::POST, "/p").with_body(b"data".to_vec());
        req.headers.insert(
            HeaderName::try_from("X-Test").unwrap(),
            HeaderValue::from_static("1"),
        );
        req.query_params.insert("a b".into(), "c&d".into());

        let fr = to_fastly_request(req, "http://example.com");
        // Method
        assert_eq!(fr.get_method().as_str(), "POST");
        // Header
        let hv = fr.get_header("x-test").unwrap();
        assert_eq!(hv.to_str().unwrap(), "1");
        // Query round-trip
        let q: HashMap<String, String> = fr.get_query().unwrap();
        assert_eq!(q.get("a b").map(|s| s.as_str()), Some("c&d"));
        // Body
        let body = fr.into_body_bytes();
        assert_eq!(body, b"data".to_vec());
    }

    #[test]
    fn to_anyedge_response_maps_status_headers_body() {
        let mut f = FResponse::from_status(StatusCode::CREATED);
        let _ = f.set_header("X-Test", "ok");
        f.set_body(Body::from("hi"));
        let r = to_anyedge_response(f);
        assert_eq!(r.status.as_u16(), 201);
        assert_eq!(r.headers_all("x-test"), vec!["ok".to_string()]);
        assert_eq!(r.body, b"hi".to_vec());
    }

    #[test]
    fn encode_query_encodes_reserved_chars() {
        let mut q = HashMap::new();
        q.insert("a b".to_string(), "c&d".to_string());
        let s = encode_query(&q);
        // order is deterministic for single pair
        assert_eq!(s, "a%20b=c%26d");
    }

    #[test]
    fn map_method_rev_roundtrip_basic() {
        let fm = map_method_rev(&Method::POST);
        assert_eq!(fm.as_str(), "POST");
    }
}
