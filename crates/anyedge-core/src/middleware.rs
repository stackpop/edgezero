use crate::http::{Request, Response};

pub type Next<'a> = &'a dyn Fn(Request) -> Response;

pub trait Middleware: Send + Sync + 'static {
    fn handle(&self, req: Request, next: Next) -> Response;
}

pub struct Logger;

impl Middleware for Logger {
    fn handle(&self, req: Request, next: Next) -> Response {
        // Use the standard Rust `log` facade; caller installs a logger
        log::info!("{} {}", req.method, req.path);
        let res = next(req);
        log::info!("-> {}", res.status.as_u16());
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Request, Response};

    struct AddHeader(&'static str, &'static str);
    impl Middleware for AddHeader {
        fn handle(&self, req: Request, next: Next) -> Response {
            let res = next(req);
            res.with_header(self.0, self.1)
        }
    }

    #[test]
    fn middleware_runs_and_can_modify_response() {
        let mut app = App::new();
        app.middleware(AddHeader("X-M1", "1"));
        app.middleware(AddHeader("X-M2", "1"));
        app.get("/", |_req: Request| Response::ok().text("ok"));

        let res = app.handle(Request::new(crate::http::Method::GET, "/"));
        assert!(res.headers.get("x-m1").is_some());
        assert!(res.headers.get("x-m2").is_some());
    }
}
