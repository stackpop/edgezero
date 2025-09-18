use crate::{
    app::App,
    http::{Request, Response},
};
use std::future::Future;
use std::pin::Pin;

pub type MiddlewareFuture<'a> = Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

pub struct Next<'a> {
    app: &'a App,
    idx: usize,
}

impl<'a> Next<'a> {
    pub(crate) fn new(app: &'a App, idx: usize) -> Self {
        Self { app, idx }
    }

    pub fn run(self, req: Request) -> MiddlewareFuture<'a> {
        self.app.run_chain(self.idx, req)
    }
}

pub trait Middleware: Send + Sync + 'static {
    fn handle<'a>(&'a self, req: Request, next: Next<'a>) -> MiddlewareFuture<'a>;
}

pub struct Logger;

impl Middleware for Logger {
    fn handle<'a>(&'a self, req: Request, next: Next<'a>) -> MiddlewareFuture<'a> {
        Box::pin(async move {
            log::info!("{} {}", req.method, req.path);
            let res = next.run(req).await;
            log::info!("-> {}", res.status.as_u16());
            res
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Method, Request, Response};
    use futures::executor::block_on;

    struct AddHeader(&'static str, &'static str);
    impl Middleware for AddHeader {
        fn handle<'a>(&'a self, req: Request, next: Next<'a>) -> MiddlewareFuture<'a> {
            Box::pin(async move {
                let res = next.run(req).await;
                res.with_header(self.0, self.1)
            })
        }
    }

    #[test]
    fn middleware_runs_and_can_modify_response() {
        let mut app = App::new();
        app.middleware(AddHeader("X-M1", "1"));
        app.middleware(AddHeader("X-M2", "1"));
        app.get("/", |_req: Request| Response::ok().text("ok"));

        let res = block_on(app.handle(Request::new(Method::GET, "/")));
        assert!(res.headers.get("x-m1").is_some());
        assert!(res.headers.get("x-m2").is_some());
    }
}
