use anyedge_core::{Hooks, RouterService};

use crate::handlers::{echo, echo_json, headers, root, stream};

pub struct DemoApp;

impl Hooks for DemoApp {
    fn routes() -> RouterService {
        build_router()
    }

    fn name() -> &'static str {
        "AnyEdge Demo"
    }
}

pub fn build_router() -> RouterService {
    RouterService::builder()
        .get("/", root)
        .get("/echo/{name}", echo)
        .get("/headers", headers)
        .get("/stream", stream)
        .post("/echo", echo_json)
        .build()
}
