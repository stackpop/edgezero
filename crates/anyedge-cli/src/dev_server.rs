use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[cfg(not(feature = "dev-example"))]
use anyedge_core::{action, Text};
use anyedge_core::{request_builder, Body, HeaderName, HeaderValue, Method, RouterService, Uri};
use futures::{executor::block_on, pin_mut, StreamExt};

#[cfg(feature = "dev-example")]
use app_demo_core::DemoApp;

pub fn run_dev() {
    println!("[anyedge] dev: starting local server on http://127.0.0.1:8787");
    let router = build_dev_router();
    if let Err(err) = run_local_server("127.0.0.1:8787", router) {
        eprintln!("[anyedge] dev server error: {err}");
    }
}

fn run_local_server(addr: &str, router: RouterService) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        if let Err(err) = handle_conn(&mut stream, router.clone()) {
            eprintln!("[anyedge] conn error: {err}");
        }
    }
    Ok(())
}

fn handle_conn(stream: &mut TcpStream, router: RouterService) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let mut read = 0usize;
    // Read until CRLF CRLF or buffer fills
    loop {
        let n = stream.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
        if read >= 4 && buf[..read].windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if read == buf.len() {
            break;
        }
    }

    let req_text = String::from_utf8_lossy(&buf[..read]);
    let mut lines = req_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method_token = parts.next().unwrap_or("GET");
    let path_token = parts.next().unwrap_or("/");

    let method = Method::from_bytes(method_token.as_bytes()).unwrap_or(Method::GET);
    let uri = path_token
        .parse::<Uri>()
        .unwrap_or_else(|_| "/".parse::<Uri>().expect("static URI"));

    let mut req = request_builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .map_err(std::io::Error::other)?;

    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if let (Ok(header_name), Ok(header_value)) = (
                HeaderName::from_bytes(name.trim().as_bytes()),
                HeaderValue::from_str(value.trim()),
            ) {
                req.headers_mut().append(header_name, header_value);
            }
        }
    }

    let response = block_on(router.oneshot(req));
    write_response(stream, response)
}

fn write_response(stream: &mut TcpStream, response: anyedge_core::Response) -> std::io::Result<()> {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let reason = status.canonical_reason().unwrap_or("OK");

    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", status.as_u16(), reason).as_bytes());

    let mut has_content_length = false;
    for (name, value) in parts.headers.iter() {
        if name.as_str().eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        out.extend_from_slice(
            format!("{}: {}\r\n", name.as_str(), value.to_str().unwrap_or("")).as_bytes(),
        );
    }

    let body_bytes = match body {
        Body::Once(bytes) => bytes,
        Body::Stream(stream_body) => {
            let collected = block_on(async move {
                let mut buf = Vec::new();
                pin_mut!(stream_body);
                while let Some(chunk) = stream_body.next().await {
                    let chunk = chunk.map_err(|err| std::io::Error::other(err.to_string()))?;
                    buf.extend_from_slice(&chunk);
                }
                Ok::<Vec<u8>, std::io::Error>(buf)
            })?;
            collected.into()
        }
    };

    if !has_content_length {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body_bytes.len()).as_bytes());
    }

    out.extend_from_slice(b"\r\n");
    stream.write_all(&out)?;
    stream.write_all(body_bytes.as_ref())?;
    Ok(())
}

fn build_dev_router() -> RouterService {
    #[cfg(feature = "dev-example")]
    {
        DemoApp::build_app().into_router()
    }

    #[cfg(not(feature = "dev-example"))]
    {
        default_router()
    }
}

#[cfg(not(feature = "dev-example"))]
fn default_router() -> RouterService {
    RouterService::builder()
        .get("/", dev_root)
        .get("/echo/{name}", dev_echo)
        .build()
}

#[cfg(not(feature = "dev-example"))]
#[derive(serde::Deserialize)]
struct EchoParams {
    name: String,
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn dev_root() -> Text<&'static str> {
    Text::new("AnyEdge dev server")
}

#[cfg(not(feature = "dev-example"))]
#[action]
async fn dev_echo(Path(params): anyedge_core::Path<EchoParams>) -> Text<String> {
    Text::new(format!("hello {}", params.name))
}
