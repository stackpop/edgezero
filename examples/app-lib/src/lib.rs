use anyedge_core::{App, Request, Response};

pub fn build_app() -> App {
    let mut app = App::new();

    app.get("/", |_req: Request| Response::ok().text("AnyEdge Dev App"));

    app.get("/echo/:name", |req: Request| {
        let name = req.param("name").unwrap_or("world");
        Response::ok().text(format!("Hello, {}!", name))
    });

    app.get("/headers", |req: Request| {
        let ua = req.header("User-Agent").unwrap_or("(unknown)");
        Response::ok().text(format!("ua={}", ua))
    });

    // Streaming example: send a few chunks using chunked transfer in dev server
    app.route_with(
        anyedge_core::Method::GET,
        "/stream",
        |_req: Request| {
            let chunks: Vec<Vec<u8>> = (0..5)
                .map(|i| format!("chunk {}\n", i).into_bytes())
                .collect();
            Response::ok()
                .with_header("Content-Type", "text/plain; charset=utf-8")
                .with_chunks(chunks)
        },
        anyedge_core::app::RouteOptions::streaming(),
    );

    app
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::{Method, Request};

    #[test]
    fn root_ok() {
        let app = build_app();
        let res = app.handle(Request::new(Method::GET, "/"));
        assert_eq!(res.status.as_u16(), 200);
    }
}
