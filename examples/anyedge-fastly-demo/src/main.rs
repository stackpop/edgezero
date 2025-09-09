use anyedge_core::{App, Request as ARequest, Response as AResponse};

#[cfg_attr(not(all(target_arch = "wasm32")), allow(dead_code))]
fn build_app() -> App {
    let mut app = App::new();

    app.get("/", |_req: ARequest| {
        AResponse::ok().text("AnyEdge Fastly Demo")
    });

    app.get("/echo/:name", |req: ARequest| {
        let name = req.param("name").unwrap_or("world");
        AResponse::ok().text(format!("Hello, {}!", name))
    });

    app.get("/headers", |req: ARequest| {
        // Show a couple of headers for demonstration
        let ua = req.header("User-Agent").unwrap_or("(unknown)");
        let host = req.header("Host").unwrap_or("(unknown)");
        AResponse::ok().text(format!("ua={}; host={}", ua, host))
    });

    app.get("/ip", |req: ARequest| {
        let ip = req
            .ctx
            .get("client_ip")
            .map(|s| s.as_str())
            .unwrap_or("(unknown)");
        AResponse::ok().text(format!("ip={}", ip))
    });

    app.get("/version", |_req: ARequest| {
        let ver = std::env::var("FASTLY_SERVICE_VERSION").unwrap_or_else(|_| "".into());
        AResponse::ok().text(format!("version={}", ver))
    });

    app.get("/health", |_req: ARequest| AResponse::ok().text("ok"));

    // Streaming example: send a few chunks. On Fastly we currently buffer the chunks.
    app.route_with(
        anyedge_core::Method::GET,
        "/stream",
        |_req: ARequest| {
            let chunks: Vec<Vec<u8>> = (0..5)
                .map(|i| format!("chunk {}\n", i).into_bytes())
                .collect();
            AResponse::ok()
                .with_header("Content-Type", "text/plain; charset=utf-8")
                .with_chunks(chunks)
        },
        anyedge_core::app::RouteOptions::streaming(),
    );

    app
}

#[cfg(all(target_arch = "wasm32"))]
mod wasm {
    use super::*;
    use fastly::{Error, Request, Response};
    use log::LevelFilter;

    #[fastly::main]
    pub fn main(req: Request) -> Result<Response, Error> {
        let app = super::build_app();
        // Initialize provider logger directly (single API)
        let endpoint =
            std::env::var("ANYEDGE_FASTLY_LOG_ENDPOINT").unwrap_or_else(|_| "anyedge_log".into());
        anyedge_fastly::init_logger(&endpoint, LevelFilter::Info, true)
            .expect("init fastly logger");
        Ok(anyedge_fastly::handle(&app, req))
    }
}

#[cfg(not(all(target_arch = "wasm32")))]
fn main() {
    // Native builds are allowed but not runnable as a Fastly service.
    println!(
        "anyedge-fastly-demo: build OK (native stub). Target wasm32-wasip1 to run with Fastly."
    );
}
