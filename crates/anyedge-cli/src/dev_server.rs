use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[cfg(not(feature = "dev-example"))]
use anyedge_core::action;
use anyedge_core::body::Body;
use anyedge_core::http::{
    request_builder, HeaderName, HeaderValue, Method, Response as CoreResponse, Uri,
};
use anyedge_core::router::RouterService;
use futures::{executor::block_on, pin_mut, StreamExt};

#[cfg(feature = "dev-example")]
use app_demo_core::App;

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

    let request = request_from_buffer(&buf[..read])?;
    let response = block_on(router.oneshot(request));
    write_response(stream, response)
}

fn request_from_buffer(raw: &[u8]) -> std::io::Result<anyedge_core::http::Request> {
    let req_text = String::from_utf8_lossy(raw);
    let mut lines = req_text.lines();
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

    Ok(req)
}

fn write_response(stream: &mut TcpStream, response: CoreResponse) -> std::io::Result<()> {
    let (head, body) = serialize_response(response)?;
    stream.write_all(&head)?;
    stream.write_all(&body)?;
    Ok(())
}

fn serialize_response(response: CoreResponse) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let reason = status.canonical_reason().unwrap_or("OK");

    let mut head = Vec::new();
    head.extend_from_slice(b"HTTP/1.1 ");
    let status_code = status.as_u16().to_string();
    head.extend_from_slice(status_code.as_bytes());
    head.push(b' ');
    head.extend_from_slice(reason.as_bytes());
    head.extend_from_slice(b"\r\n");

    let mut has_content_length = false;
    for (name, value) in parts.headers.iter() {
        if name.as_str().eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.to_str().unwrap_or("").as_bytes());
        head.extend_from_slice(b"\r\n");
    }

    let body_bytes: Vec<u8> = match body {
        Body::Once(bytes) => bytes.to_vec(),
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
            collected
        }
    };

    if !has_content_length {
        head.extend_from_slice(b"Content-Length: ");
        head.extend_from_slice(body_bytes.len().to_string().as_bytes());
        head.extend_from_slice(b"\r\n");
    }

    head.extend_from_slice(b"\r\n");

    Ok((head, body_bytes))
}

fn build_dev_router() -> RouterService {
    #[cfg(feature = "dev-example")]
    {
        use anyedge_core::app::Hooks;

        let demo_app = App::build_app();
        demo_app.router().clone()
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
async fn dev_echo(Path(params): anyedge_core::extractor::Path<EchoParams>) -> Text<String> {
    Text::new(format!("hello {}", params.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyedge_core::http::{header::HOST, response_builder, Method, StatusCode};
    use anyedge_core::response::Text;

    #[anyedge_core::action]
    async fn hello() -> Text<&'static str> {
        Text::new("hello world")
    }

    #[test]
    fn request_from_buffer_parses_basic_get() {
        let request = request_from_buffer(
            b"GET /demo HTTP/1.1
Host: example

",
        )
        .expect("request");
        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.uri().path(), "/demo");
        assert_eq!(
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok()),
            Some("example")
        );
    }

    #[test]
    fn serialize_response_includes_headers_and_body() {
        let response = response_builder()
            .status(StatusCode::OK)
            .header("x-test", "value")
            .body(Body::text("hi"))
            .expect("response");
        let (head, body) = serialize_response(response).expect("serialize");
        let head_text = String::from_utf8(head).expect("utf8");
        assert!(head_text.starts_with("HTTP/1.1 200 OK"));
        assert!(head_text.contains("Content-Length: 2"));
        assert!(head_text.contains("x-test: value"));
        assert!(head_text.contains("\r\n\r\n"));
        assert_eq!(body, b"hi");
    }

    #[test]
    fn router_handles_request_via_helpers() {
        let router = RouterService::builder().get("/", hello).build();
        let request = request_from_buffer(
            b"GET / HTTP/1.1
Host: localhost

",
        )
        .expect("request");
        let response = block_on(router.oneshot(request));
        let (_head, body) = serialize_response(response).expect("serialize");
        assert_eq!(body, b"hello world");
    }
}
