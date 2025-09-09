use crate::http::{Request, Response};

pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> Response;
}

impl<F> Handler for F
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    fn handle(&self, req: Request) -> Response {
        (self)(req)
    }
}

pub type BoxHandler = Box<dyn Handler + Send + Sync>;
