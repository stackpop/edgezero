use anyedge_core::{App, Method, Request};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

pub fn run_dev() {
    println!("[anyedge] dev: starting local server on http://127.0.0.1:8787");
    let app = build_dev_app();
    if let Err(e) = run_local_server("127.0.0.1:8787", app) {
        eprintln!("[anyedge] dev server error: {e}");
    }
}

// Build an App for dev:
// - If built with `dev-example`, use the shared app-lib in this workspace.
// - Otherwise, provide a tiny default app.
fn build_dev_app() -> App {
    #[cfg(feature = "dev-example")]
    {
        anyedge_app_lib::build_app()
    }
    #[cfg(not(feature = "dev-example"))]
    {
        let mut app = App::new();
        app.get("/", |_req| anyedge_core::Response::ok().text("AnyEdge dev server"));
        app
    }
}

fn run_local_server(addr: &str, app: App) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    // Simple, blocking server. Handle connections sequentially to avoid threading and borrowing issues.
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(e) = handle_conn(&mut stream, &app) {
            eprintln!("[anyedge] conn error: {e}");
        }
    }
    Ok(())
}

fn handle_conn(stream: &mut TcpStream, app: &App) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut buf = [0u8; 8192];
    let mut read = 0usize;
    // Read until we find \r\n\r\n or buffer fills
    loop {
        let n = stream.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
        if read >= 4 {
            if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        if read == buf.len() {
            break;
        }
    }

    let req_text = String::from_utf8_lossy(&buf[..read]);
    let mut lines = req_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    let mut req = Request::new(
        Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET),
        path.to_string(),
    );
    // Headers
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            req.append_header(k.trim(), v.trim());
        }
    }
    let res = app.handle(req);

    write_response(stream, res)?;
    Ok(())
}

fn write_response(stream: &mut TcpStream, res: anyedge_core::Response) -> std::io::Result<()> {
    let status = res.status.as_u16();
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes());
    let mut has_len = false;
    for (k, v) in res.headers.iter() {
        if k.as_str().eq_ignore_ascii_case("content-length") {
            has_len = true;
        }
        out.extend_from_slice(
            format!("{}: {}\r\n", k.as_str(), v.to_str().unwrap_or("")).as_bytes(),
        );
    }
    if let Some(mut iter) = res.stream {
        if !res
            .headers
            .iter()
            .any(|(k, _)| k.as_str().eq_ignore_ascii_case("transfer-encoding"))
        {
            out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
        }
        out.extend_from_slice(b"\r\n");
        stream.write_all(&out)?;
        for chunk in &mut iter {
            let line = format!("{:X}\r\n", chunk.len());
            stream.write_all(line.as_bytes())?;
            stream.write_all(&chunk)?;
            stream.write_all(b"\r\n")?;
        }
        stream.write_all(b"0\r\n\r\n")?;
    } else {
        if !has_len {
            out.extend_from_slice(format!("Content-Length: {}\r\n", res.body.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&res.body);
        stream.write_all(&out)?;
    }
    Ok(())
}

