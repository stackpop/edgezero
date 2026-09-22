use crate::evidence::{Result, require};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub type Signal = Arc<(Mutex<bool>, Condvar)>;
static CLIENT_EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
pub fn take_client_events() -> Vec<Value> {
    std::mem::take(&mut *CLIENT_EVENTS.lock().unwrap())
}

#[derive(Debug)]
pub struct InterruptedResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub reason: String,
}
impl std::fmt::Display for InterruptedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "response {} interrupted after {} bytes: {}",
            self.status,
            self.body.len(),
            self.reason
        )
    }
}
impl std::error::Error for InterruptedResponse {}
pub fn release(signal: &Signal) {
    *signal.0.lock().unwrap() = true;
    signal.1.notify_all();
}
pub struct Backend {
    pub port: u16,
    pub receipts: Arc<Mutex<Vec<String>>>,
    pub release: Signal,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Backend {
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let barrier = Arc::new((Mutex::new(0usize), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s, b, done) = (
            receipts.clone(),
            release.clone(),
            barrier.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (r, s, b) = (r.clone(), s.clone(), b.clone());
                        thread::spawn(move || {
                            let _ = serve(stream, r, s, b);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            port,
            receipts,
            release,
            stop,
            worker: Some(worker),
        })
    }
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        release(&self.release);
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn serve(
    mut stream: TcpStream,
    receipts: Arc<Mutex<Vec<String>>>,
    signal: Signal,
    barrier: Arc<(Mutex<usize>, Condvar)>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line
        .split_whitespace()
        .nth(1)
        .ok_or("missing request path")?
        .to_owned();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    receipts.lock().unwrap().push(path.clone());
    if path.starts_with("/barrier") {
        let mut count = barrier.0.lock().unwrap();
        *count += 1;
        barrier.1.notify_all();
        let (count, _) = barrier
            .1
            .wait_timeout_while(count, Duration::from_secs(8), |n| *n < 2)
            .map_err(|_| "barrier poisoned")?;
        if *count < 2 {
            stream.write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    }
    if path.starts_with("/chunks") {
        stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nfirst\r\n")?;
        stream.flush()?;
        let (ready, _) = signal
            .1
            .wait_timeout_while(signal.0.lock().unwrap(), Duration::from_secs(8), |v| !*v)
            .map_err(|_| "release poisoned")?;
        if *ready {
            if path.contains("large=1") {
                let body = vec![b'z'; 256 * 1024];
                write!(stream, "{:x}\r\n", body.len())?;
                stream.write_all(&body)?;
                stream.write_all(b"\r\n0\r\n\r\n")?;
            } else {
                stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n")?;
            }
        }
    } else if path.starts_with("/abort") {
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial",
        )?;
    } else {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
    }
    Ok(())
}
pub fn port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

pub fn malformed_request(port: u16, token: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /probe/malformed HTTP/1.1\r\nHost: localhost\r\nx-request-token: {token}\r\nContent-Length: invalid\r\nConnection: close\r\n\r\n"
    )?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(line.trim().to_owned())
}
pub fn request(
    port: u16,
    path: &str,
    token: &str,
    backend: Option<&str>,
    signal: Option<&Signal>,
) -> Result<Value> {
    let start = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    stream.set_write_timeout(Some(Duration::from_secs(12)))?;
    let probe = path.starts_with("/probe/");
    let body = if probe { token } else { "" };
    write!(
        stream,
        "{} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-request-token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n",
        if probe { "POST" } else { "GET" },
        body.len()
    )?;
    if let Some(url) = backend {
        write!(stream, "x-fixture-backend: {url}\r\n")?;
    }
    write!(stream, "\r\n{body}")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .ok_or("missing HTTP response status")?
        .parse()?;
    let mut headers = Vec::new();
    let mut length = None;
    let mut chunked = false;
    loop {
        line.clear();
        require(reader.read_line(&mut line)? > 0, "truncated HTTP headers")?;
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or("malformed HTTP header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>()?);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
        headers.push(json!([name, value]));
    }
    let header_ns = start.elapsed().as_nanos() as u64;
    let mut first_byte_ns = None;
    let mut bytes = Vec::new();
    let outcome = (|| -> Result<()> {
        let mut read_part = |reader: &mut BufReader<TcpStream>, count: usize| -> Result<()> {
            if count == 0 {
                return Ok(());
            }
            let mut first = [0];
            reader.read_exact(&mut first)?;
            bytes.push(first[0]);
            if first_byte_ns.is_none() {
                first_byte_ns = Some(start.elapsed().as_nanos() as u64);
                if let Some(signal) = signal {
                    release(signal);
                }
            }
            let mut remaining = count - 1;
            let mut buffer = [0u8; 8192];
            while remaining > 0 {
                let size = remaining.min(buffer.len());
                let read = reader.read(&mut buffer[..size])?;
                require(read > 0, "truncated HTTP body")?;
                bytes.extend_from_slice(&buffer[..read]);
                remaining -= read;
            }
            Ok(())
        };
        if chunked {
            loop {
                line.clear();
                require(reader.read_line(&mut line)? > 0, "truncated chunk header")?;
                let count = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)?;
                if count == 0 {
                    loop {
                        line.clear();
                        require(reader.read_line(&mut line)? > 0, "truncated chunk trailer")?;
                        if line == "\r\n" {
                            break;
                        }
                    }
                    break;
                }
                read_part(&mut reader, count)?;
                let mut crlf = [0; 2];
                reader.read_exact(&mut crlf)?;
                require(crlf == *b"\r\n", "invalid chunk terminator")?;
            }
        } else if let Some(length) = length {
            read_part(&mut reader, length)?;
        } else {
            let mut first = [0];
            if reader.read(&mut first)? != 0 {
                bytes.push(first[0]);
                first_byte_ns = Some(start.elapsed().as_nanos() as u64);
                if let Some(signal) = signal {
                    release(signal);
                }
                reader.read_to_end(&mut bytes)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        return Err(Box::new(InterruptedResponse {
            status,
            body: bytes,
            reason: error.to_string(),
        }));
    }
    let event = json!({"event":"client_completed","token":token,"status":status,"headers":headers,"body":String::from_utf8(bytes)?,"header_ns":header_ns,"first_byte_ns":first_byte_ns.unwrap_or(header_ns),"complete_ns":start.elapsed().as_nanos() as u64});
    CLIENT_EVENTS.lock().unwrap().push(event.clone());
    Ok(event)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progressive_body_and_abort() {
        let backend = Backend::start().unwrap();
        let result = request(backend.port, "/chunks", "x", None, Some(&backend.release)).unwrap();
        assert_eq!(result["body"], "firstsecond");
        let failure = request(backend.port, "/abort", "x", None, None).unwrap_err();
        let partial = failure
            .downcast_ref::<InterruptedResponse>()
            .expect("response started");
        assert_eq!(partial.status, 200);
        assert_eq!(partial.body, b"partial");
    }

    #[test]
    fn connection_failure_is_not_an_interrupted_response() {
        let unused = port().unwrap();
        let failure = request(unused, "/abort", "x", None, None).unwrap_err();
        assert!(failure.downcast_ref::<InterruptedResponse>().is_none());
    }
}
