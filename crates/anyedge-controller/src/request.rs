use anyedge_core::Request;

#[derive(Debug)]
pub struct RequestParts {
    request: Option<Request>,
    body_taken: bool,
}

impl RequestParts {
    pub fn new(request: Request) -> Self {
        Self {
            request: Some(request),
            body_taken: false,
        }
    }

    pub fn request(&self) -> &Request {
        self.request.as_ref().expect("request already taken")
    }

    pub fn request_mut(&mut self) -> &mut Request {
        self.request.as_mut().expect("request already taken")
    }

    pub fn extensions(&self) -> &anyedge_core::http::Extensions {
        &self.request().extensions
    }

    pub fn take_body(&mut self) -> Vec<u8> {
        self.body_taken = true;
        if let Some(req) = self.request.as_mut() {
            std::mem::take(&mut req.body)
        } else {
            panic!("request already taken");
        }
    }

    pub fn body_taken(&self) -> bool {
        self.body_taken
    }

    pub fn take_request(&mut self) -> Request {
        self.request.take().expect("request already taken")
    }

    pub fn into_request(self) -> Request {
        self.request.expect("request already taken")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{Method, Request};

    fn build_request() -> Request {
        let mut req = Request::new(Method::POST, "/test");
        req.body = b"payload".to_vec();
        req
    }

    #[test]
    fn take_body_returns_bytes_once() {
        let mut parts = RequestParts::new(build_request());
        let body = parts.take_body();
        assert_eq!(body, b"payload".to_vec());
        assert!(parts.body_taken());
    }

    #[test]
    #[should_panic(expected = "request already taken")]
    fn request_cannot_be_taken_twice() {
        let mut parts = RequestParts::new(build_request());
        let _ = parts.take_request();
        let _ = parts.take_request();
    }
}
