//! Test-only source-bound proxy and verified TLS ingress. No response is collected here.
#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "fixture ownership precedes bounded relay helpers"
)]

use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, copy_bidirectional_with_sizes,
};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::runtime::Builder;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};

const MAX_HEAD: usize = 0x8000;
const RELAY_BUFFER: usize = 0x4000;

#[derive(Clone, Copy)]
pub(crate) enum Assertions {
    /// Keep deliberate malformed/gap inputs unchanged while preserving a real owned peer.
    Preserve,
    AppendForwarded {
        host: &'static str,
        scheme: &'static str,
    },
    AppendXForwarded {
        host: &'static str,
        scheme: &'static str,
    },
    ReplaceForwarded {
        client: IpAddr,
        host: &'static str,
        scheme: &'static str,
    },
}

pub(crate) struct LocalProxy {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl LocalProxy {
    #[must_use]
    #[inline]
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[must_use]
    #[inline]
    pub(crate) fn start(
        upstream: SocketAddr,
        source: Ipv4Addr,
        assertions: Assertions,
        tls: bool,
    ) -> Self {
        let std_listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("proxy listener");
        let addr = std_listener.local_addr().expect("proxy address");
        std_listener
            .set_nonblocking(true)
            .expect("proxy nonblocking");
        let (stop, mut stopped) = oneshot::channel();
        let worker = thread::spawn(move || {
            let runtime = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("proxy runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(std_listener).expect("proxy adoption");
                let tls_acceptor = tls.then(|| TlsAcceptor::from(Arc::new(server_config())));
                let mut tasks = JoinSet::new();
                #[expect(clippy::integer_division_remainder_used, reason = "Tokio select macro branch arithmetic")]
                loop {
                    tokio::select! {
                        () = async { let _closed = (&mut stopped).await; } => break,
                        _finished = tasks.join_next(), if !tasks.is_empty() => {},
                        accepted = listener.accept(), if tasks.len() < 64 => {
                            let (socket, peer) = accepted.expect("proxy accept");
                            let task_acceptor = tls_acceptor.clone();
                            tasks.spawn(async move {
                                let operation = async {
                                    if let Some(acceptor) = task_acceptor {
                                        if let Ok(mut client) = acceptor.accept(socket).await {
                                            let _relayed = relay(&mut client, peer, upstream, source, assertions).await;
                                        }
                                    } else {
                                        let mut client = socket;
                                        let _relayed = relay(&mut client, peer, upstream, source, assertions).await;
                                    }
                                };
                                let _bounded = timeout(Duration::from_secs(5), operation).await;
                            });
                        }
                    }
                }
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            });
        });
        Self {
            addr,
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

impl Drop for LocalProxy {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _sent = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().expect("proxy worker");
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "invalid bounded fixture head")
}

async fn read_head<Client: AsyncRead + Unpin>(
    client: &mut Client,
) -> io::Result<(String, Vec<u8>)> {
    let mut received = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    loop {
        if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let body_at = end.checked_add(4).ok_or_else(invalid)?;
            let head = String::from_utf8(received.get(..end).ok_or_else(invalid)?.to_vec())
                .map_err(|_error| invalid())?;
            let body = received.get(body_at..).ok_or_else(invalid)?.to_vec();
            return Ok((head, body));
        }
        let count = client.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "fixture head incomplete",
            ));
        }
        received
            .len()
            .checked_add(count)
            .filter(|bytes| *bytes <= MAX_HEAD)
            .ok_or_else(invalid)?;
        received.extend_from_slice(chunk.get(..count).ok_or_else(invalid)?);
    }
}

fn rewrite(head: &str, peer: SocketAddr, assertions: Assertions) -> io::Result<String> {
    let mut lines = head.split("\r\n");
    let request = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(invalid)?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(invalid)?;
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let predecessor = match peer.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("\"[{ip}]\""),
    };
    match assertions {
        Assertions::Preserve => {}
        Assertions::AppendForwarded { host, scheme } => {
            let previous = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("forwarded"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
                .join(",");
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("forwarded"));
            let assertion = format!("for={predecessor};host=\"{host}\";proto={scheme}");
            headers.push((
                "Forwarded".into(),
                if previous.is_empty() {
                    assertion
                } else {
                    format!("{previous},{assertion}")
                },
            ));
        }
        Assertions::AppendXForwarded { host, scheme } => {
            let previous = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-forwarded-for"))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>()
                .join(",");
            headers.retain(|(name, _)| !name.to_ascii_lowercase().starts_with("x-forwarded-"));
            let node = peer.ip().to_string();
            headers.push((
                "X-Forwarded-For".into(),
                if previous.is_empty() {
                    node
                } else {
                    format!("{previous},{node}")
                },
            ));
            headers.push(("X-Forwarded-Host".into(), host.into()));
            headers.push(("X-Forwarded-Proto".into(), scheme.into()));
        }
        Assertions::ReplaceForwarded {
            client,
            host,
            scheme,
        } => {
            headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case("forwarded")
                    && !name.to_ascii_lowercase().starts_with("x-forwarded-")
            });
            // The test gateway's client assertion is explicitly supplied by its trusted producer,
            // never taken from the visitor's fields. This is not a provider-discovery mechanism.
            let node = match client {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => format!("\"[{ip}]\""),
            };
            headers.push((
                "Forwarded".into(),
                format!("for={node};host=\"{host}\";proto={scheme}"),
            ));
        }
    }
    let mut rewritten = format!("{request}\r\n");
    for (name, value) in headers {
        use std::fmt::Write as _;
        write!(rewritten, "{name}: {value}\r\n").map_err(|_error| invalid())?;
    }
    rewritten.push_str("\r\n");
    if rewritten.len() > MAX_HEAD {
        return Err(invalid());
    }
    Ok(rewritten)
}

async fn relay<Client: AsyncRead + AsyncWrite + Unpin>(
    client: &mut Client,
    peer: SocketAddr,
    upstream: SocketAddr,
    source: Ipv4Addr,
    assertions: Assertions,
) -> io::Result<()> {
    let (head, body_prefix) = read_head(client).await?;
    let rewritten = rewrite(&head, peer, assertions)?;
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(IpAddr::V4(source), 0))?;
    let mut origin = socket.connect(upstream).await?;
    origin.write_all(rewritten.as_bytes()).await?;
    origin.write_all(&body_prefix).await?;
    // Finite, per-direction relay buffers. Never collect a response or body for proxying.
    let _copied =
        copy_bidirectional_with_sizes(client, &mut origin, RELAY_BUFFER, RELAY_BUFFER).await?;
    Ok(())
}

fn server_config() -> ServerConfig {
    let certificate = CertificateDer::from(include_bytes!("tls/server.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("tls/server-key.der").to_vec(),
    ));
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("test certificate/key")
}

#[inline]
pub(crate) fn verified_tls_request(
    addr: SocketAddr,
    request: &[u8],
    server_name: &'static str,
) -> io::Result<String> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(include_bytes!("tls/ca.der").to_vec()))
            .map_err(|_error| invalid())?;
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let operation = async {
            let socket = TcpStream::connect(addr).await?;
            let name = ServerName::try_from(server_name).map_err(|_error| invalid())?;
            let mut client = connector.connect(name, socket).await?;
            client.write_all(request).await?;
            let mut response = String::new();
            client.read_to_string(&mut response).await?;
            Ok(response)
        };
        timeout(Duration::from_secs(5), operation)
            .await
            .map_err(|_error| io::Error::new(ErrorKind::TimedOut, "TLS fixture timeout"))?
    })
}
