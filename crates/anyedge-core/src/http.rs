use std::collections::HashMap;

pub use http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub params: HashMap<String, String>,
    pub ctx: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            params: HashMap::new(),
            ctx: HashMap::new(),
            query_params: HashMap::new(),
        }
    }

    pub fn with_body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = bytes.into();
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn set_header<K, V>(&mut self, name: K, value: V)
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        if let (Ok(n), Ok(v)) = (name.try_into(), value.try_into()) {
            self.headers.insert(n, v);
        }
    }

    pub fn append_header<K, V>(&mut self, name: K, value: V)
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        if let (Ok(n), Ok(v)) = (name.try_into(), value.try_into()) {
            self.headers.append(n, v);
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(|s| s.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query_params.get(name).map(|s| s.as_str())
    }

    pub fn query_all(&self) -> &HashMap<String, String> {
        &self.query_params
    }

    // Keep previous convenience
    pub fn method_from(s: &str) -> Method {
        Method::from_bytes(s.as_bytes()).unwrap_or(Method::GET)
    }

    pub fn headers_all(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
            .collect()
    }

    pub fn headers_all_raw(&self, name: &str) -> Vec<HeaderValue> {
        self.headers.get_all(name).iter().cloned().collect()
    }
}

pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub stream: Option<Box<dyn Iterator<Item = Vec<u8>> + Send>>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            headers: HeaderMap::new(),
            body: Vec::new(),
            stream: None,
        }
    }

    pub fn with_header<K, V>(mut self, name: K, value: V) -> Self
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        if let (Ok(n), Ok(v)) = (name.try_into(), value.try_into()) {
            self.headers.insert(n, v);
        }
        self
    }

    pub fn with_body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = bytes.into();
        self.stream = None;
        self
    }

    pub fn with_chunks<I>(mut self, chunks: I) -> Self
    where
        I: IntoIterator<Item = Vec<u8>> + Send + 'static,
        <I as IntoIterator>::IntoIter: Send,
    {
        self.stream = Some(Box::new(chunks.into_iter()));
        self.body.clear();
        self
    }

    pub fn append_header<K, V>(mut self, name: K, value: V) -> Self
    where
        K: TryInto<HeaderName>,
        V: TryInto<HeaderValue>,
    {
        if let (Ok(n), Ok(v)) = (name.try_into(), value.try_into()) {
            self.headers.append(n, v);
        }
        self
    }

    pub fn ok() -> Self {
        Self::new(200)
    }
    pub fn not_found() -> Self {
        Self::new(404).text("Not Found")
    }
    pub fn internal_error() -> Self {
        Self::new(500).text("Internal Server Error")
    }

    pub fn text<T: Into<String>>(self, body: T) -> Self {
        let s: String = body.into();
        self.with_header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .with_body(s.into_bytes())
    }

    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    pub fn content_len(&self) -> Option<usize> {
        if self.stream.is_some() {
            None
        } else {
            Some(self.body.len())
        }
    }

    pub fn clear_body(&mut self) {
        self.body.clear();
        self.stream = None;
    }

    pub fn into_streaming(mut self) -> Self {
        if self.stream.is_none() {
            let bytes = std::mem::take(&mut self.body);
            self.stream = Some(Box::new(std::iter::once(bytes)));
        }
        self.headers.remove(header::CONTENT_LENGTH);
        self
    }

    pub fn headers_all(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
            .collect()
    }

    pub fn headers_all_raw(&self, name: &str) -> Vec<HeaderValue> {
        self.headers.get_all(name).iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_text_sets_content_type_and_body() {
        let res = Response::ok().text("hello");
        assert_eq!(res.status.as_u16(), 200);
        let ct = res.headers.get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().contains("text/plain"));
        assert_eq!(String::from_utf8(res.body).unwrap(), "hello");
    }

    #[test]
    fn headers_all_returns_all_values() {
        let mut req = Request::new(Method::GET, "/");
        req.append_header("Set-Cookie", "a=1");
        req.append_header("Set-Cookie", "b=2");
        let vals = req.headers_all("set-cookie");
        assert_eq!(vals.len(), 2);
        let raw_vals = req.headers_all_raw("set-cookie");
        assert_eq!(raw_vals.len(), 2);
        assert_eq!(raw_vals[0].to_str().unwrap(), "a=1");
        assert_eq!(raw_vals[1].to_str().unwrap(), "b=2");

        let res = Response::ok()
            .append_header("x-test", "v1")
            .append_header("x-test", "v2");
        let vals = res.headers_all("x-test");
        assert_eq!(vals, vec!["v1".to_string(), "v2".to_string()]);
        let raw_vals = res.headers_all_raw("x-test");
        assert_eq!(raw_vals.len(), 2);
        assert_eq!(raw_vals[0].to_str().unwrap(), "v1");
        assert_eq!(raw_vals[1].to_str().unwrap(), "v2");
    }

    #[test]
    fn set_vs_append_header_behavior() {
        let mut req = Request::new(Method::GET, "/");
        // set_header replaces
        req.set_header("x-one", "a");
        req.set_header("x-one", "b");
        assert_eq!(req.headers_all("x-one"), vec!["b".to_string()]);
        // append_header preserves duplicates
        req.append_header("x-two", "a");
        req.append_header("x-two", "b");
        assert_eq!(
            req.headers_all("x-two"),
            vec!["a".to_string(), "b".to_string()]
        );

        // Response variants
        let res = Response::ok()
            .with_header("x-r", "a")
            .with_header("x-r", "b")
            .append_header("x-r2", "a")
            .append_header("x-r2", "b");
        assert_eq!(res.headers_all("x-r"), vec!["b".to_string()]);
        assert_eq!(
            res.headers_all("x-r2"),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
