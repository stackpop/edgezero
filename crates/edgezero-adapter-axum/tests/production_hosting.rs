//! Real local processes, public entrypoints and ordinary main termination output.
#![cfg(all(feature = "axum", feature = "test-utils", unix))]

#[cfg(test)]
#[path = "fixtures/native_proxy.rs"]
mod native_proxy;

#[cfg(test)]
#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "subprocess fixtures group setup, process ownership and acceptance scenarios"
)]
mod tests {
    use super::native_proxy::{Assertions, LocalProxy, verified_tls_request};
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, BufRead as _, BufReader, Read, Write as _};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    const WAIT: Duration = Duration::from_secs(5);

    struct Roots {
        config: TempDir,
        cwd: TempDir,
        data: TempDir,
    }

    impl Roots {
        fn new() -> Self {
            let roots = Self {
                config: tempfile::tempdir().expect("config root"),
                cwd: tempfile::tempdir().expect("unrelated cwd"),
                data: tempfile::tempdir().expect("data root"),
            };
            roots.map(&json!({"settings": blob("configured-greeting", "fixture_token")}));
            roots
        }

        fn map(&self, value: &Value) {
            fs::write(
                self.config.path().join("local-config-settings.json"),
                serde_json::to_vec(value).expect("JSON"),
            )
            .expect("config fixture");
        }
    }

    fn blob(greeting: &str, token: &str) -> String {
        serde_json::to_string(&BlobEnvelope::new(
            json!({"greeting": greeting, "token": token}),
            "2026-10-05T00:00:00Z".to_owned(),
        ))
        .expect("envelope")
    }

    fn address() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .expect("port reservation")
            .local_addr()
            .expect("address")
    }

    fn command(mode: &str, roots: &Roots, addr: SocketAddr) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_edgezero-hosting-fixture"));
        command
            .env_clear()
            .current_dir(roots.cwd.path())
            .env("FIXTURE_MODE", mode)
            .env("FIXTURE_BIND", addr.to_string())
            .env("FIXTURE_CONFIG_ROOT", roots.config.path())
            .env("FIXTURE_DATA_ROOT", roots.data.path())
            .env("EDGEZERO__ADAPTER__HOST", "127.0.0.1")
            .env("EDGEZERO__ADAPTER__PORT", addr.port().to_string())
            .env("EDGEZERO__ADAPTER__SHUTDOWN_GRACE_SECONDS", "1")
            .env("EDGEZERO__ADAPTER__CONFIG_DIR", roots.config.path())
            .env("EDGEZERO__ADAPTER__DATA_DIR", roots.data.path())
            .env("EDGEZERO__STORES__KV__FIRST__NAME", "shared-fixture")
            .env("EDGEZERO__STORES__KV__SECOND__NAME", "shared-fixture")
            .env("fixture_token", "resolved-fixture-token");
        command
    }

    struct Server {
        addr: SocketAddr,
        child: Child,
        events: mpsc::Receiver<String>,
        output: Arc<Mutex<String>>,
        readers: Vec<JoinHandle<()>>,
    }

    impl Server {
        fn spawn(mut command: Command, addr: SocketAddr) -> Self {
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("fixture process");
            let output = Arc::new(Mutex::new(String::new()));
            let (sender, events) = mpsc::channel();
            let stdout = child.stdout.take().expect("stdout");
            let stderr = child.stderr.take().expect("stderr");
            let mut readers = Vec::new();
            let streams: [Box<dyn Read + Send>; 2] = [Box::new(stdout), Box::new(stderr)];
            for stream in streams {
                let captured = Arc::clone(&output);
                let stream_sender = sender.clone();
                readers.push(thread::spawn(move || {
                    for result in BufReader::new(stream).lines() {
                        let Ok(line) = result else { break };
                        let mut captured_output = captured.lock().expect("output");
                        captured_output.push_str(&line);
                        captured_output.push('\n');
                        drop(captured_output);
                        let _sent = stream_sender.send(line);
                    }
                }));
            }
            Self {
                addr,
                child,
                events,
                output,
                readers,
            }
        }

        fn log(&self) -> String {
            self.output.lock().expect("output").clone()
        }

        fn ready(&mut self) {
            let deadline = Instant::now().checked_add(WAIT).expect("startup timer");
            loop {
                assert!(
                    Instant::now() < deadline,
                    "startup deadline: {}",
                    self.log()
                );
                assert!(
                    self.child.try_wait().expect("status").is_none(),
                    "early process failure: {}",
                    self.log()
                );
                if let Ok(response) = self.request("/ready")
                    && response.starts_with("HTTP/1.1 200")
                {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn request(&self, path: &str) -> io::Result<String> {
            let mut socket = self.socket()?;
            write!(
                socket,
                "GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n"
            )?;
            let mut response = String::new();
            socket.read_to_string(&mut response)?;
            Ok(response)
        }

        fn signal(&self, name: &str) {
            assert!(
                Command::new("kill")
                    .arg(format!("-{name}"))
                    .arg(self.child.id().to_string())
                    .status()
                    .expect("send signal")
                    .success()
            );
        }

        fn socket(&self) -> io::Result<TcpStream> {
            let socket = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50))?;
            socket.set_read_timeout(Some(WAIT))?;
            socket.set_write_timeout(Some(WAIT))?;
            Ok(socket)
        }

        fn wait(&mut self) -> ExitStatus {
            let deadline = Instant::now().checked_add(WAIT).expect("exit timer");
            loop {
                if let Some(status) = self.child.try_wait().expect("process status") {
                    for reader in self.readers.drain(..) {
                        reader.join().expect("output reader");
                    }
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "process exit deadline: {}",
                    self.log()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_line(&self, expected: &str) {
            let deadline = Instant::now().checked_add(WAIT).expect("event timer");
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let line = self.events.recv_timeout(remaining).unwrap_or_else(|error| {
                    panic!("waiting for {expected}: {error}; {}", self.log())
                });
                if line == expected {
                    return;
                }
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _killed = self.child.kill();
            let _status = self.child.wait();
            for reader in self.readers.drain(..) {
                let _joined = reader.join();
            }
        }
    }

    fn failed(command: Command, addr: SocketAddr, category: &str) -> String {
        let mut server = Server::spawn(command, addr);
        assert!(!server.wait().success());
        let log = server.log();
        assert!(log.contains(category), "missing category {category}: {log}");
        assert!(
            !log.contains("SENTINEL"),
            "startup diagnostic leaked fixture contents: {log}"
        );
        assert!(!log.contains("INITIALIZED"));
        TcpStream::connect_timeout(&addr, Duration::from_millis(50))
            .expect_err("failed startup never accepts connections");
        log
    }

    fn exact_json(bytes: usize) -> Vec<u8> {
        assert!(bytes >= 2);
        let mut body = vec![b'x'; bytes];
        body[0] = b'"';
        *body.last_mut().expect("JSON closing quote") = b'"';
        body
    }

    fn upload(
        server: &Server,
        path: &str,
        media: Option<&str>,
        body: Vec<u8>,
        chunked: bool,
    ) -> String {
        let mut socket = server.socket().expect("upload connection");
        let mut writer = socket.try_clone().expect("upload writer");
        let content_type =
            media.map_or_else(String::new, |value| format!("Content-Type: {value}\r\n"));
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let request_head = format!(
            "POST {path} HTTP/1.1\r\nHost: fixture\r\n{content_type}{framing}Connection: close\r\n\r\n"
        );
        let sending = thread::spawn(move || -> io::Result<()> {
            writer.write_all(request_head.as_bytes())?;
            for chunk in body.chunks(0x4000) {
                if chunked {
                    write!(writer, "{:x}\r\n", chunk.len())?;
                }
                writer.write_all(chunk)?;
                if chunked {
                    writer.write_all(b"\r\n")?;
                }
            }
            if chunked {
                writer.write_all(b"0\r\n\r\n")?;
            }
            Ok(())
        });
        let mut response = String::new();
        let reading = socket.read_to_string(&mut response);
        let sent = sending.join().expect("bounded upload writer");
        let (head, payload) = response
            .split_once("\r\n\r\n")
            .expect("usable response head");
        let is_413 = head.starts_with("HTTP/1.1 413");
        let length = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("response length"))
            })
            .expect("finite response framing");
        assert_eq!(payload.len(), length, "complete response: {response}");
        if let Err(error) = reading {
            assert!(
                is_413 && error.kind() == io::ErrorKind::ConnectionReset,
                "response read: {error}"
            );
        }
        if let Err(error) = sent {
            assert!(is_413, "upload failure without 413: {error}");
        }
        response
    }

    fn assert_upload_status(response: &str, status: u16) {
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
        if status == 413 {
            let (_, body) = response.split_once("\r\n\r\n").expect("response");
            let value: Value = serde_json::from_str(body).expect("usable error body");
            assert_eq!(value["error"]["kind"], "payload_too_large");
            assert_eq!(value["error"]["message"], "request body too large");
        }
    }

    fn captured_json(log: &str, marker: &str) -> Vec<Value> {
        log.lines()
            .filter_map(|line| line.strip_prefix(marker))
            .map(|value| serde_json::from_str(value).expect("fixture JSON marker"))
            .collect()
    }

    fn observations(server: &Server) -> Vec<Value> {
        captured_json(&server.log(), "NATIVE_OBSERVATION ")
    }

    fn framework_logs(server: &Server) -> Vec<Value> {
        captured_json(&server.log(), "FRAMEWORK_LOG ")
    }

    fn wait_for(server: &Server, predicate: impl Fn(&str) -> bool) {
        let deadline = Instant::now().checked_add(WAIT).expect("observation timer");
        loop {
            let log = server.log();
            if predicate(&log) {
                return;
            }
            assert!(Instant::now() < deadline, "observation deadline: {log}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn route_record(server: &Server, route: &str) -> Value {
        wait_for(server, |log| {
            captured_json(log, "NATIVE_OBSERVATION ")
                .iter()
                .any(|record| record["route"] == route)
        });
        let records = observations(server)
            .into_iter()
            .filter(|record| record["route"] == route)
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "one terminal owner: {}", server.log());
        records.into_iter().next().expect("route record")
    }

    fn raw_request(addr: SocketAddr, request: &str) -> String {
        let mut socket = TcpStream::connect_timeout(&addr, WAIT).expect("wire connection");
        socket.set_read_timeout(Some(WAIT)).expect("read timeout");
        socket.set_write_timeout(Some(WAIT)).expect("write timeout");
        socket.write_all(request.as_bytes()).expect("wire request");
        let mut response = String::new();
        socket.read_to_string(&mut response).expect("wire response");
        response
    }

    fn response_json(response: &str) -> Value {
        let (_, body) = response.split_once("\r\n\r\n").expect("response head");
        serde_json::from_str(body).expect("finite fixture JSON response")
    }

    fn metadata_request(headers: &str) -> String {
        format!(
            "GET /native-metadata HTTP/1.1\r\nHost: direct.example:8080\r\n{headers}Connection: close\r\n\r\n"
        )
    }

    fn assert_metadata(value: &Value, client: &str, host: &str, scheme: &str, result: &str) {
        let facts = &value["handler"];
        assert_eq!(value["admission"], *facts, "admission view");
        assert_eq!(value["middleware"], *facts, "middleware view");
        assert_eq!(facts["client"], client);
        assert_eq!(facts["host"], host);
        assert_eq!(facts["scheme"], scheme);
        assert_eq!(facts["forwarding_result"], result);
        assert_eq!(facts["host_header"], "direct.example:8080");
        assert_eq!(facts["uri"], "/native-metadata");
        assert_eq!(facts["raw_forwarding_absent"], true);
        assert_eq!(value["forwarded_host"], host);
        assert_eq!(value["direct_context_peer"], facts["peer"]);
        let id = facts["request_id"].as_str().expect("application ID");
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    fn assert_no_diagnostic_headers(response: &str) {
        let (head, _) = response.split_once("\r\n\r\n").expect("response head");
        for line in head.lines().skip(1) {
            let (name, _) = line.split_once(':').expect("response header");
            let normalized_name = name.to_ascii_lowercase();
            assert!(
                !normalized_name.contains("request-id")
                    && !normalized_name.starts_with("x-edgezero")
            );
            assert!(!matches!(
                normalized_name.as_str(),
                "traceparent" | "tracestate" | "server-timing"
            ));
        }
    }

    fn native_command(
        mode: &str,
        roots: &Roots,
        addr: SocketAddr,
        networks: &str,
        family: &str,
    ) -> Command {
        let mut launch = command(mode, roots, addr);
        launch
            .env("EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS", networks)
            .env("EDGEZERO__ADAPTER__FORWARDING_HEADER_FAMILY", family);
        launch
    }

    fn assert_final_zero(server: &Server) {
        let snapshots = captured_json(&server.log(), "FINAL_SNAPSHOT ");
        assert_eq!(
            snapshots,
            vec![
                json!({"active_requests": 0_u64, "active_connections": 0_u64, "idle_connections": 0_u64})
            ]
        );
    }

    fn assert_hook_order(server: &Server, id: &str) {
        let log = server.log();
        let completion = log
            .find(&format!("APP_COMPLETION {id}\n"))
            .expect("application completion");
        let native_line = log
            .lines()
            .find(|line| line.starts_with("NATIVE_OBSERVATION ") && line.contains(id))
            .expect("native observation");
        let native = log.find(native_line).expect("native offset");
        let observer = log
            .get(native..)
            .expect("native suffix")
            .find("APP_OBSERVER ")
            .expect("application observer");
        assert!(completion < native);
        assert!(observer > 0);
        assert_eq!(log.matches(&format!("APP_COMPLETION {id}\n")).count(), 1);
        assert_eq!(
            observations(server)
                .iter()
                .filter(|record| record["id"] == id)
                .count(),
            1
        );
    }

    fn assert_record_log(server: &Server, record: &Value) {
        let id = record["id"].as_str().expect("record ID");
        let request_logs = framework_logs(server)
            .into_iter()
            .filter(|entry| {
                let message = entry["message"].as_str().expect("framework message");
                (message.starts_with("request_terminal ")
                    || message.starts_with("request_ingress "))
                    && message.contains(id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            request_logs.len(),
            1,
            "facade cardinality: {}",
            server.log()
        );
        let entry = request_logs
            .into_iter()
            .next()
            .expect("request facade entry");
        assert_eq!(entry["level"], record["level"]);
        let message = entry["message"].as_str().expect("safe request text");
        assert!(message.contains(&format!(
            "method={}",
            record["method"].as_str().expect("method")
        )));
        assert!(message.contains("duration_us="));
        assert!(message.contains("bytes_written=") || record["bytes_written"].is_null());
    }

    #[test]
    fn native_direct_boundary_strips_spoofs_and_accounts_the_original_head() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
        server.ready();
        let spoof = "Forwarded: for=198.51.100.9;host=FORWARDED_SENTINEL;proto=https\r\nX-Forwarded-For: 203.0.113.9\r\nX-Forwarded-Host: XFH_SENTINEL\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Secret: HEADER_SENTINEL\r\nX-Request-ID: VISITOR_ID_SENTINEL\r\n";
        let request = metadata_request(spoof);
        let response = raw_request(addr, &request);
        assert_no_diagnostic_headers(&response);
        let value = response_json(&response);
        assert_metadata(
            &value,
            "127.0.0.1",
            "direct.example:8080",
            "http",
            "untrusted_peer",
        );
        for field in ["client_source", "host_source", "scheme_source"] {
            assert_eq!(value["handler"][field], "direct");
        }
        let peer: SocketAddr = value["handler"]["peer"]
            .as_str()
            .expect("owned peer")
            .parse()
            .expect("socket peer");
        assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let expected_bytes = request
            .split("\r\n")
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| line.len().checked_add(2).expect("normalized line bytes"))
            .try_fold(2_usize, usize::checked_add)
            .expect("normalized head bytes");
        assert_eq!(value["normalized"]["header_bytes"], expected_bytes);
        assert_eq!(value["normalized"]["header_count"], 8_u64);
        assert_eq!(
            value["normalized"]["target_bytes"],
            "/native-metadata".len()
        );
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        let record = route_record(&server, "/native-metadata");
        assert_eq!(record["id"], value["handler"]["request_id"]);
        assert_record_log(&server, &record);
        for entry in framework_logs(&server) {
            let message = entry["message"].as_str().expect("framework message");
            assert!(!message.contains("SENTINEL"));
            assert!(!message.contains("127.0.0.1") && !message.contains("direct.example"));
        }
    }

    #[test]
    fn native_source_bound_single_proxy_and_mapped_cidr_match_only_owned_peers() {
        for networks in [
            "127.0.0.2/32",
            "::ffff:127.0.0.2/128",
            "::ffff:127.0.0.2/127",
            "127.0.0.3/32",
        ] {
            let roots = Roots::new();
            let addr = address();
            let mut server = Server::spawn(
                native_command("diagnostics", &roots, addr, networks, "forwarded"),
                addr,
            );
            server.ready();
            let proxy = LocalProxy::start(
                addr,
                Ipv4Addr::new(127, 0, 0, 2),
                Assertions::AppendForwarded {
                    host: "public.example",
                    scheme: "https",
                },
                false,
            );
            let response = raw_request(
                proxy.addr(),
                &metadata_request(
                    "Forwarded: for=198.51.100.99;host=attacker.example;proto=http\r\nX-Forwarded-Host: conflicting.example\r\n",
                ),
            );
            let value = response_json(&response);
            let trusted = networks != "127.0.0.3/32";
            assert_metadata(
                &value,
                if trusted { "127.0.0.1" } else { "127.0.0.2" },
                if trusted {
                    "public.example"
                } else {
                    "direct.example:8080"
                },
                if trusted { "https" } else { "http" },
                if trusted {
                    "accepted"
                } else {
                    "untrusted_peer"
                },
            );
            let peer: SocketAddr = value["handler"]["peer"]
                .as_str()
                .expect("proxy peer")
                .parse()
                .expect("peer address");
            assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
            assert_no_diagnostic_headers(&response);
            server.signal("TERM");
            assert!(server.wait().success());
            assert_final_zero(&server);
        }
    }

    #[test]
    fn native_two_proxy_chain_stops_at_untrusted_hop_and_gateway_replaces_prefix() {
        for (networks, boundary) in [
            ("127.0.0.2/32,127.0.0.3/32", "127.0.0.1"),
            ("127.0.0.3/32", "127.0.0.2"),
        ] {
            let roots = Roots::new();
            let addr = address();
            let mut server = Server::spawn(
                native_command("diagnostics", &roots, addr, networks, "forwarded"),
                addr,
            );
            server.ready();
            let ingress = LocalProxy::start(
                addr,
                Ipv4Addr::new(127, 0, 0, 3),
                Assertions::AppendForwarded {
                    host: "final.example",
                    scheme: "https",
                },
                false,
            );
            let first = LocalProxy::start(
                ingress.addr(),
                Ipv4Addr::new(127, 0, 0, 2),
                Assertions::AppendForwarded {
                    host: "earlier.example",
                    scheme: "http",
                },
                false,
            );
            let value = response_json(&raw_request(
                first.addr(),
                &metadata_request(
                    "Forwarded: for=198.51.100.99;host=attacker.example;proto=http\r\n",
                ),
            ));
            assert_metadata(&value, boundary, "final.example", "https", "accepted");
            assert_eq!(value["handler"]["host_source"], "trusted_forwarded");
            assert!(
                value["handler"]["peer"]
                    .as_str()
                    .expect("last proxy peer")
                    .starts_with("127.0.0.3:")
            );
            server.signal("TERM");
            assert!(server.wait().success());
            assert_final_zero(&server);
        }
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(
            native_command("diagnostics", &roots, addr, "127.0.0.3/32", "forwarded"),
            addr,
        );
        server.ready();
        let gateway = LocalProxy::start(
            addr,
            Ipv4Addr::new(127, 0, 0, 3),
            Assertions::ReplaceForwarded {
                client: "203.0.113.7".parse().expect("verified gateway client"),
                host: "gateway.example",
                scheme: "https",
            },
            false,
        );
        let value = response_json(&raw_request(
            gateway.addr(),
            &metadata_request(
                "Forwarded: for=198.51.100.99;host=attacker.example;proto=http\r\nX-Forwarded-Host: attacker.example\r\n",
            ),
        ));
        assert_metadata(
            &value,
            "203.0.113.7",
            "gateway.example",
            "https",
            "accepted",
        );
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
    }

    fn assert_missing_unresolved_and_duplicate_family(addr: SocketAddr, family: &str) {
        // This source-bound relay preserves deliberate bad inputs rather than sanitizing them.
        let gateway = LocalProxy::start(
            addr,
            Ipv4Addr::new(127, 0, 0, 2),
            Assertions::Preserve,
            false,
        );
        let absent = if family == "forwarded" {
            "Forwarded: host=partial.example;proto=https\r\nX-Forwarded-For: 198.51.100.9\r\n"
        } else {
            "X-Forwarded-Host: partial.example\r\nX-Forwarded-Proto: https\r\nForwarded: for=198.51.100.9\r\n"
        };
        let missing_client = response_json(&raw_request(gateway.addr(), &metadata_request(absent)));
        assert_metadata(
            &missing_client,
            "127.0.0.2",
            "partial.example",
            "https",
            if family == "forwarded" {
                "unresolved_chain"
            } else {
                "partial"
            },
        );
        for headers in [
            if family == "forwarded" {
                "Forwarded: for=unknown;host=gap.example;proto=https\r\n"
            } else {
                "X-Forwarded-For: unknown\r\nX-Forwarded-Host: gap.example\r\nX-Forwarded-Proto: https\r\n"
            },
            if family == "forwarded" {
                "Forwarded: for=127.0.0.2;host=gap.example;proto=https\r\n"
            } else {
                "X-Forwarded-For: 127.0.0.2\r\nX-Forwarded-Host: gap.example\r\nX-Forwarded-Proto: https\r\n"
            },
        ] {
            let value = response_json(&raw_request(gateway.addr(), &metadata_request(headers)));
            assert_metadata(
                &value,
                "127.0.0.2",
                "gap.example",
                "https",
                "unresolved_chain",
            );
        }
        let duplicate = if family == "forwarded" {
            "Forwarded: for=198.51.100.8;for=203.0.113.9;host=good.example;proto=https\r\n"
        } else {
            "X-Forwarded-For: 198.51.100.8\r\nX-Forwarded-Host: good.example\r\nX-Forwarded-Host: duplicate.example\r\nX-Forwarded-Proto: https\r\n"
        };
        let malformed_duplicate =
            response_json(&raw_request(gateway.addr(), &metadata_request(duplicate)));
        assert_metadata(
            &malformed_duplicate,
            "127.0.0.2",
            "direct.example:8080",
            "http",
            "malformed",
        );
    }

    #[test]
    fn native_selected_families_conflicts_malformed_sets_gaps_and_exhaustion() {
        for family in ["forwarded", "x-forwarded"] {
            let roots = Roots::new();
            let addr = address();
            let mut server = Server::spawn(
                native_command("diagnostics", &roots, addr, "127.0.0.2/32", family),
                addr,
            );
            server.ready();
            let assertions = if family == "forwarded" {
                Assertions::AppendForwarded {
                    host: "selected.example",
                    scheme: "https",
                }
            } else {
                Assertions::AppendXForwarded {
                    host: "selected.example",
                    scheme: "https",
                }
            };
            let proxy = LocalProxy::start(addr, Ipv4Addr::new(127, 0, 0, 2), assertions, false);
            for (prefix, expected_result, client, host, scheme) in [
                (
                    "Forwarded: for=198.51.100.1;host=wrong.example;proto=http\r\nX-Forwarded-For: 203.0.113.1\r\nX-Forwarded-Host: wrong.example\r\nX-Forwarded-Proto: http\r\n",
                    "accepted",
                    "127.0.0.1",
                    "selected.example",
                    "https",
                ),
                (
                    if family == "forwarded" {
                        "Forwarded: for=bad-address;host=good.example;proto=https\r\n"
                    } else {
                        "X-Forwarded-For: bad-address\r\n"
                    },
                    "malformed",
                    "127.0.0.2",
                    "direct.example:8080",
                    "http",
                ),
                (
                    if family == "forwarded" {
                        "Forwarded: for=unknown\r\n"
                    } else {
                        "X-Forwarded-For: unknown\r\n"
                    },
                    "accepted",
                    "127.0.0.1",
                    "selected.example",
                    "https",
                ),
            ] {
                let value = response_json(&raw_request(proxy.addr(), &metadata_request(prefix)));
                assert_metadata(&value, client, host, scheme, expected_result);
            }
            assert_missing_unresolved_and_duplicate_family(addr, family);
            server.signal("TERM");
            assert!(server.wait().success());
            assert_final_zero(&server);
            let records = observations(&server)
                .into_iter()
                .filter(|record| record["route"] == "/native-metadata")
                .collect::<Vec<_>>();
            assert_eq!(records.len(), 7);
            for record in records {
                assert_record_log(&server, &record);
                if record["forwarding_result"] == "malformed" {
                    assert_eq!(record["level"], "WARN");
                }
            }
        }
    }

    #[test]
    fn native_verified_tls_ingress_overwrites_public_assertions_and_rejects_wrong_name() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(
            native_command("diagnostics", &roots, addr, "127.0.0.2/32", "forwarded"),
            addr,
        );
        server.ready();
        let proxy = LocalProxy::start(
            addr,
            Ipv4Addr::new(127, 0, 0, 2),
            Assertions::AppendForwarded {
                host: "native-proxy.test",
                scheme: "https",
            },
            true,
        );
        let request = metadata_request(
            "Forwarded: for=198.51.100.99;host=attacker.example;proto=http\r\nX-Forwarded-Host: attacker.example\r\nX-Forwarded-Proto: http\r\n",
        );
        let response = verified_tls_request(proxy.addr(), request.as_bytes(), "native-proxy.test")
            .expect("CA and hostname verified TLS");
        let value = response_json(&response);
        assert_metadata(
            &value,
            "127.0.0.1",
            "native-proxy.test",
            "https",
            "accepted",
        );
        assert_eq!(value["handler"]["scheme_source"], "trusted_forwarded");
        assert_no_diagnostic_headers(&response);
        let wrong_name = verified_tls_request(proxy.addr(), request.as_bytes(), "wrong-name.test")
            .expect_err("wrong server name must fail verification");
        assert_eq!(
            wrong_name.kind(),
            io::ErrorKind::InvalidData,
            "certificate verification failure, not a connection timeout"
        );
        assert!(
            format!("{wrong_name:?}").contains("NotValidForName"),
            "hostname verification must be the rejecting boundary"
        );
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        assert_eq!(
            observations(&server)
                .iter()
                .filter(|record| record["route"] == "/native-metadata")
                .count(),
            1
        );
    }

    fn assert_native_error_record(
        server: &Server,
        path: &str,
        status: u16,
        failure: &str,
        level: &str,
    ) {
        if path.starts_with("/PRIVATE_PATH") {
            return;
        }
        let route = path.split('?').next().expect("registered route");
        let record = route_record(server, route);
        assert_eq!(record["status"], status);
        assert_eq!(record["level"], level);
        if !failure.is_empty() {
            assert_eq!(record["failure"], failure);
        }
        if path == "/native-status503" {
            assert!(record["failure"].is_null(), "status alone is not rejection");
        }
        if matches!(path, "/native-conversion" | "/native-expired") {
            assert_eq!(record["body_kind"], "fallback");
            assert_eq!(record["fallback"], "completed");
            assert_ne!(record["outcome"], "completed");
        }
    }

    fn assert_default_error_observations(server: &Server) {
        let records = observations(server);
        let overflows = records
            .iter()
            .filter(|record| record["status"] == 413_u16)
            .collect::<Vec<_>>();
        assert_eq!(overflows.len(), 1);
        assert_eq!(
            overflows
                .into_iter()
                .next()
                .expect("real JSON cap rejection")["failure"],
            "payload_too_large"
        );
        let extension = records
            .iter()
            .find(|record| record["method"] == "other")
            .expect("extension method bounded category");
        assert_eq!(extension["status"], 405_u16);
        assert_eq!(extension["level"], "INFO");
        for entry in framework_logs(server) {
            assert!(
                !entry["message"]
                    .as_str()
                    .expect("framework message")
                    .contains("SENTINEL"),
                "framework log leak: {entry}"
            );
            let message = entry["message"].as_str().expect("framework message");
            assert!(
                !message.contains("CONFIG_VALUE_SENTINEL")
                    && !message.contains("SECRET_VALUE_SENTINEL"),
                "environment-derived private inputs leaked through the facade: {entry}"
            );
        }
        for record in records {
            assert_record_log(server, &record);
        }
    }

    #[test]
    fn native_default_errors_json_cap_and_logs_keep_sentinels_private() {
        let roots = Roots::new();
        let addr = address();
        let mut launch = command("diagnostics", &roots, addr);
        launch
            .env("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES", "64")
            .env("PRIVATE_CONFIG_SENTINEL", "CONFIG_VALUE_SENTINEL")
            .env("fixture_token", "SECRET_VALUE_SENTINEL");
        let mut server = Server::spawn(launch, addr);
        server.ready();
        for (path, status, failure, level) in [
            (
                "/PRIVATE_PATH_SENTINEL?token=QUERY_SENTINEL",
                404,
                "not_found",
                "INFO",
            ),
            (
                "/native-error?token=QUERY_SENTINEL",
                500,
                "internal",
                "WARN",
            ),
            ("/native-upstream", 502, "bad_gateway", "WARN"),
            ("/native-conversion", 500, "", "WARN"),
            ("/native-expired", 504, "", "WARN"),
            ("/native-status503", 503, "", "INFO"),
        ] {
            let response = raw_request(
                addr,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: HOST_SENTINEL\r\nAuthorization: Bearer AUTH_SENTINEL\r\nCookie: COOKIE_SENTINEL\r\nX-Request-ID: ID_SENTINEL\r\nConnection: close\r\n\r\n"
                ),
            );
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}")),
                "{response}"
            );
            assert!(
                !response.contains("SENTINEL"),
                "default wire leak: {response}"
            );
            assert!(
                !response.contains("CONFIG_VALUE_SENTINEL")
                    && !response.contains("SECRET_VALUE_SENTINEL"),
                "environment-derived private inputs leaked through the wire: {response}"
            );
            assert_no_diagnostic_headers(&response);
            assert_native_error_record(&server, path, status, failure, level);
        }
        let extension_response = raw_request(
            addr,
            "METHOD_SENTINEL /native-status503?token=QUERY_SENTINEL HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
        );
        assert!(extension_response.starts_with("HTTP/1.1 405"));
        assert!(!extension_response.contains("SENTINEL"));
        assert!(extension_response.to_ascii_lowercase().contains("allow:"));
        let json_response = upload(
            &server,
            "/native-json-number",
            Some("application/json"),
            b"\"PAYLOAD_SENTINEL\"".to_vec(),
            false,
        );
        assert_upload_status(&json_response, 400);
        assert!(!json_response.contains("PAYLOAD_SENTINEL"));
        assert_eq!(
            response_json(&json_response)["error"]["message"],
            "invalid JSON payload"
        );
        assert_upload_status(&upload(&server, "/json", None, exact_json(64), false), 200);
        assert_upload_status(&upload(&server, "/json", None, exact_json(65), true), 413);
        let handler_error = server
            .request("/native-handler404")
            .expect("application error");
        assert!(
            handler_error.contains("HANDLER_404_SENTINEL"),
            "application error messages remain application policy"
        );
        let handler_record = route_record(&server, "/native-handler404");
        assert_eq!(handler_record["level"], "WARN");
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        assert_eq!(
            server
                .log()
                .lines()
                .filter(|line| *line == "PRIVATE_INPUTS_USED")
                .count(),
            2,
            "both error paths consumed the fixture's captured environment-derived configuration and secret inputs"
        );
        assert_default_error_observations(&server);
    }

    #[test]
    fn native_custom_renderer_keeps_ownership_but_framework_json_parse_text_stays_fixed() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics-custom", &roots, addr), addr);
        server.ready();
        let custom = server
            .request("/CUSTOM_ROUTE_SENTINEL")
            .expect("custom route error");
        assert!(custom.starts_with("HTTP/1.1 404"));
        assert!(
            custom.contains("CUSTOM_ROUTE_SENTINEL"),
            "custom renderer retains original routing detail"
        );
        let parsed = upload(
            &server,
            "/native-json-number",
            Some("application/json"),
            b"\"PAYLOAD_SENTINEL\"".to_vec(),
            false,
        );
        assert!(parsed.starts_with("HTTP/1.1 400"));
        assert!(parsed.ends_with("invalid JSON payload"));
        assert!(!parsed.contains("PAYLOAD_SENTINEL"));
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        for entry in framework_logs(&server) {
            assert!(
                !entry["message"]
                    .as_str()
                    .expect("framework message")
                    .contains("SENTINEL")
            );
        }
    }

    #[test]
    fn native_original_head_overage_cannot_be_hidden_by_raw_header_stripping() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
        server.ready();
        let request = metadata_request(&format!(
            "X-Forwarded-Secret: {}\r\n",
            "HEAD_SENTINEL".repeat(6000)
        ));
        let response = raw_request(addr, &request);
        assert!(
            response.starts_with("HTTP/1.1 431"),
            "original normalized head limit: {response}"
        );
        assert!(!response.contains("SENTINEL"));
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        let rejections = observations(&server)
            .into_iter()
            .filter(|record| record["status"] == 431_u16)
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 1);
        let record = rejections
            .into_iter()
            .next()
            .expect("head rejection record");
        assert_eq!(record["level"], "WARN");
        assert_eq!(
            server.log().matches("APP_COMPLETION ").count(),
            1,
            "head failure precedes app admission; only readiness was admitted"
        );
        assert_record_log(&server, &record);
    }

    #[test]
    fn native_ids_remain_private_under_mutation_and_concurrent_hosting_requests() {
        let roots = Roots::new();
        let addr = address();
        let other_addr = address();
        let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
        let mut other = Server::spawn(command("diagnostics", &roots, other_addr), other_addr);
        server.ready();
        other.ready();
        let response = raw_request(
            addr,
            "GET /native-mutated HTTP/1.1\r\nHost: fixture\r\nX-Request-ID: VISITOR_ID_SENTINEL\r\nConnection: close\r\n\r\n",
        );
        assert_no_diagnostic_headers(&response);
        let mutated = response_json(&response);
        assert_eq!(mutated["metadata_absent"], true);
        assert_eq!(mutated["forwarded_host_after"], "replacement.example");
        let authoritative = route_record(&server, "/native-mutated");
        assert_eq!(authoritative["id"], mutated["removed_id"]);
        let mut clients = Vec::new();
        for target in [addr, other_addr] {
            for _request in 0..4_u8 {
                clients.push(thread::spawn(move || {
                    let concurrent_response = raw_request(target, "GET /native-status503 HTTP/1.1\r\nHost: fixture\r\nX-Request-ID: VISITOR_ID_SENTINEL\r\nConnection: close\r\n\r\n");
                    assert!(concurrent_response.starts_with("HTTP/1.1 503"));
                    assert_no_diagnostic_headers(&concurrent_response);
                }));
            }
        }
        for client in clients {
            client.join().expect("bounded concurrent wire client");
        }
        for running in [&mut server, &mut other] {
            running.signal("TERM");
            assert!(running.wait().success());
            assert_final_zero(running);
        }
        let mut ids = HashSet::new();
        for running in [&server, &other] {
            let records = observations(running);
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record["route"] == "/native-status503")
                    .count(),
                4
            );
            assert_eq!(
                running.log().matches("APP_COMPLETION ").count(),
                records.len()
            );
            assert_eq!(
                running.log().matches("APP_OBSERVER ").count(),
                records.len()
            );
            for record in records {
                let id = record["id"].as_str().expect("generated ID");
                assert_eq!(id.len(), 64);
                assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert!(ids.insert(id.to_owned()), "no process/request ID collision");
                assert!(
                    record["counts"]["active_requests"]
                        .as_u64()
                        .expect("request count")
                        <= 3,
                    "the observed request releases before its sink; only other concurrent requests remain"
                );
                assert_record_log(running, &record);
            }
            assert!(!running.log().contains("VISITOR_ID_SENTINEL"));
        }
    }

    #[test]
    fn native_logger_panic_preserves_order_cardinality_and_resource_release() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics-panic", &roots, addr), addr);
        server.ready();
        assert!(
            server
                .request("/native-status503")
                .expect("surviving response")
                .starts_with("HTTP/1.1 503")
        );
        let selected_record = route_record(&server, "/native-status503");
        wait_for(&server, |log| log.matches("APP_OBSERVER ").count() >= 2);
        assert_hook_order(&server, selected_record["id"].as_str().expect("record ID"));
        assert_eq!(selected_record["counts"]["active_requests"], 0_u64);
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        let log = server.log();
        assert_eq!(log.matches("APP_LOGGER_INSTALLED").count(), 1);
        assert_eq!(
            log.matches("LOGGER_FAULT").count(),
            1,
            "no recursive logger fault reporting"
        );
        let records = observations(&server);
        assert_eq!(log.matches("APP_COMPLETION ").count(), records.len());
        assert_eq!(log.matches("APP_OBSERVER ").count(), records.len());
        for record in records {
            assert_record_log(&server, &record);
        }
    }

    fn wire_prefix(socket: &mut TcpStream, prefix: &[u8]) -> Vec<u8> {
        let mut received = Vec::new();
        let deadline = Instant::now().checked_add(WAIT).expect("prefix timer");
        while !received.windows(prefix.len()).any(|bytes| bytes == prefix) {
            assert!(Instant::now() < deadline, "wire prefix deadline");
            assert!(received.len() < 0x4000, "finite response head and prefix");
            let mut chunk = [0_u8; 512];
            let count = socket
                .read(&mut chunk)
                .expect("wire prefix before terminal failure");
            assert_ne!(count, 0, "connection closed before prefix: {received:?}");
            received.extend_from_slice(chunk.get(..count).expect("wire read length"));
        }
        assert!(received.starts_with(b"HTTP/1.1 200"));
        assert!(received.windows(4).any(|bytes| bytes == b"\r\n\r\n"));
        received
    }

    #[test]
    fn native_actual_wire_prefix_precedes_source_and_deadline_failure_without_second_response() {
        for (path, prefix, outcome, dropped) in [
            (
                "/native-source",
                b"source-prefix".as_slice(),
                "source_error",
                "SOURCE_DROPPED",
            ),
            (
                "/native-deadline",
                b"deadline-prefix".as_slice(),
                "deadline_exceeded",
                "DEADLINE_DROPPED",
            ),
        ] {
            let roots = Roots::new();
            let addr = address();
            let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
            server.ready();
            let mut client = server.socket().expect("streaming client");
            write!(
                client,
                "GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n"
            )
            .expect("stream request");
            let mut wire = wire_prefix(&mut client, prefix);
            let mut tail = Vec::new();
            let terminal = client.read_to_end(&mut tail);
            if let Err(error) = terminal {
                assert!(
                    matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                    ),
                    "failure did not settle before timeout: {error}"
                );
            }
            wire.extend_from_slice(&tail);
            let wire_text = String::from_utf8(wire).expect("ASCII fixture response");
            assert_eq!(
                wire_text.matches("HTTP/1.1 ").count(),
                1,
                "no replacement response after commitment"
            );
            assert!(!wire_text.contains("SENTINEL"));
            assert!(
                !wire_text.ends_with("0\r\n\r\n"),
                "stream failure must not look like completed chunked framing"
            );
            let record = route_record(&server, path);
            assert_eq!(
                record["status"], 200_u16,
                "selected status is not successful delivery"
            );
            assert_eq!(record["outcome"], outcome);
            assert_eq!(record["level"], "WARN");
            assert_eq!(record["bytes_written"], prefix.len());
            assert_eq!(record["body_kind"], "application");
            assert!(record["fallback"].is_null());
            assert_eq!(record["counts"]["active_requests"], 0_u64);
            wait_for(&server, |log| {
                log.contains(dropped) && log.matches("APP_OBSERVER ").count() >= 2
            });
            assert_hook_order(&server, record["id"].as_str().expect("record ID"));
            assert_record_log(&server, &record);
            server.signal("TERM");
            assert!(server.wait().success());
            assert_final_zero(&server);
            assert_eq!(server.log().matches(dropped).count(), 1);
            assert_eq!(
                observations(&server)
                    .iter()
                    .filter(|observed| observed["route"] == path)
                    .count(),
                1
            );
            for entry in framework_logs(&server) {
                assert!(
                    !entry["message"]
                        .as_str()
                        .expect("framework message")
                        .contains("SENTINEL")
                );
            }
        }
    }

    #[test]
    fn native_actual_wire_disconnect_keeps_selected_status_and_one_terminal_owner() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
        server.ready();
        let mut client = server.socket().expect("stream client");
        client
            .write_all(b"GET /stream HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("stream request");
        let wire = wire_prefix(&mut client, b"stream-prefix");
        assert_eq!(
            String::from_utf8(wire)
                .expect("wire head")
                .matches("HTTP/1.1 ")
                .count(),
            1
        );
        drop(client);
        let record = route_record(&server, "/stream");
        assert_eq!(record["status"], 200_u16);
        // The existing HTTP/1 connection owner reports this peer loss as a transport failure.
        assert_eq!(record["outcome"], "transport_error");
        assert_eq!(record["level"], "WARN");
        assert_eq!(record["counts"]["active_requests"], 0_u64);
        wait_for(&server, |log| {
            log.contains("STREAM_DROPPED") && log.matches("APP_OBSERVER ").count() >= 2
        });
        assert_hook_order(&server, record["id"].as_str().expect("record ID"));
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        assert_eq!(server.log().matches("STREAM_DROPPED").count(), 1);
        assert_record_log(&server, &record);
    }

    #[test]
    fn native_refusal_and_no_response_abort_keep_separate_observer_contracts() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("diagnostics", &roots, addr), addr);
        server.ready();
        let response = server.request("/native-refuse").expect("refusal response");
        assert!(response.starts_with("HTTP/1.1 503"));
        let mut client = server.socket().expect("abort connection");
        client
            .write_all(b"GET /native-abort HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
            .expect("abort request");
        let mut aborted_wire = Vec::new();
        let abort_read = client.read_to_end(&mut aborted_wire);
        if let Err(error) = abort_read {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        }
        assert!(aborted_wire.is_empty(), "no fabricated abort response");
        server.signal("TERM");
        assert!(server.wait().success());
        assert_final_zero(&server);
        let records = observations(&server);
        let refusal = records
            .iter()
            .filter(|record| record["failure"] == "admission_refused")
            .collect::<Vec<_>>();
        assert_eq!(refusal.len(), 1);
        assert_eq!(
            refusal.into_iter().next().expect("refusal record")["status"],
            503_u16
        );
        let abort_records = records
            .iter()
            .filter(|record| record["outcome"] == "aborted")
            .collect::<Vec<_>>();
        assert_eq!(abort_records.len(), 1);
        let abort_record = abort_records
            .into_iter()
            .next()
            .expect("no response ingress record");
        assert_eq!(abort_record["event"], "request_ingress");
        assert!(abort_record["status"].is_null() && abort_record["bytes_written"].is_null());
        assert_eq!(
            server.log().matches("APP_COMPLETION ").count(),
            2,
            "readiness plus supplied refusal completion only"
        );
        assert_eq!(
            server.log().matches("APP_OBSERVER ").count(),
            1,
            "refusal is detached; abort has no egress observer"
        );
        for record in records {
            assert_record_log(&server, &record);
        }
    }

    fn native_counts(server: &Server, expected_requests: u64, expected_idle: u64) -> Value {
        let deadline = Instant::now().checked_add(WAIT).expect("snapshot timer");
        loop {
            let counts = response_json(&server.request("/native-counts").expect("counter query"));
            if counts["active_requests"] == expected_requests
                && counts["idle_connections"] == expected_idle
            {
                return counts;
            }
            assert!(
                Instant::now() < deadline,
                "snapshot did not converge: {counts}; {}",
                server.log()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_shutdown_observations(server: &Server, path: &str, graceful: bool, optout: bool) {
        let logs = framework_logs(server);
        for phase in ["starting", "ready", "draining", "stopped"] {
            assert!(
                logs.iter().any(|entry| entry["message"]
                    .as_str()
                    .expect("lifecycle message")
                    .contains(&format!("event=lifecycle phase={phase}"))),
                "missing lifecycle {phase}"
            );
        }
        if !graceful {
            assert!(logs.iter().any(|entry| {
                entry["message"]
                    .as_str()
                    .expect("fault category")
                    .contains("reason=grace_exhausted")
            }));
        }
        let records = observations(server);
        assert!(!records.is_empty(), "native observer retained");
        assert!(server.log().contains("APP_OBSERVER ") && server.log().contains("APP_COMPLETION "));
        if optout {
            assert!(
                !logs.iter().any(|entry| {
                    let message = entry["message"].as_str().expect("framework message");
                    message.starts_with("request_terminal ")
                        || message.starts_with("request_ingress ")
                }),
                "request-record opt-out"
            );
        }
        let abandoned = records
            .iter()
            .filter(|record| record["outcome"] == "abandoned")
            .collect::<Vec<_>>();
        if matches!(path, "/pending" | "/json") {
            assert_eq!(abandoned.len(), 1);
            let record = abandoned
                .into_iter()
                .next()
                .expect("pre-attempt abandonment");
            assert!(record["status"].is_null());
            assert_eq!(record["event"], "request_ingress");
        }
    }

    #[test]
    fn native_idle_and_held_work_snapshots_drain_to_zero_with_request_records_opted_out() {
        for optout in [false, true] {
            for (path, prefix, marker, graceful) in [
                ("/finite", None, "DISPATCH", true),
                ("/pending", None, "DISPATCH", false),
                (
                    "/stream",
                    Some(b"stream-prefix".as_slice()),
                    "STREAM_READY",
                    false,
                ),
                ("/json", None, "", false),
            ] {
                let roots = Roots::new();
                let addr = address();
                let mut launch = command(
                    if optout {
                        "diagnostics-optout"
                    } else {
                        "diagnostics"
                    },
                    &roots,
                    addr,
                );
                if optout {
                    launch.env("EDGEZERO__LOGGING__REQUEST_RECORDS", "false");
                }
                let mut server = Server::spawn(launch, addr);
                server.ready();
                let mut idle = server.socket().expect("owned keepalive connection");
                idle.write_all(b"GET /ready HTTP/1.1\r\nHost: fixture\r\n\r\n")
                    .expect("keepalive request");
                let mut ready = Vec::new();
                while !ready.ends_with(b"ready") {
                    let mut buffer = [0_u8; 512];
                    let count = idle.read(&mut buffer).expect("keepalive ready response");
                    assert_ne!(count, 0);
                    ready.extend_from_slice(buffer.get(..count).expect("read length"));
                }
                let idle_counts = native_counts(&server, 1, 1);
                assert_eq!(
                    idle_counts["active_connections"], 2_u64,
                    "idle connection plus count query"
                );
                let mut held = server.socket().expect("held request");
                if path == "/json" {
                    held.write_all(b"POST /json HTTP/1.1\r\nHost: fixture\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n\"x").expect("partial body held open");
                } else {
                    write!(held, "GET {path} HTTP/1.1\r\nHost: fixture\r\n\r\n")
                        .expect("held request");
                    server.wait_line(marker);
                }
                if let Some(expected_prefix) = prefix {
                    let _wire = wire_prefix(&mut held, expected_prefix);
                }
                let held_counts = native_counts(&server, 2, 1);
                assert_eq!(
                    held_counts["active_connections"], 3_u64,
                    "held, idle and query connections"
                );
                assert_eq!(held_counts["phase"], "ready");
                let started = Instant::now();
                server.signal("TERM");
                if graceful {
                    let mut response = String::new();
                    held.read_to_string(&mut response)
                        .expect("graceful finite response");
                    assert!(
                        response.starts_with("HTTP/1.1 200")
                            && response.contains("finite-complete")
                    );
                }
                let status = server.wait();
                assert_eq!(status.success(), graceful, "{}", server.log());
                assert!(started.elapsed() < Duration::from_secs(4));
                if !graceful {
                    assert!(started.elapsed() >= Duration::from_millis(700));
                }
                assert_final_zero(&server);
                assert_eq!(idle.read(&mut [0_u8; 1]).expect("idle close"), 0);
                assert_shutdown_observations(&server, path, graceful, optout);
            }
        }
    }

    #[test]
    fn native_invalid_proxy_and_record_settings_fail_before_ready_without_input_disclosure() {
        let roots = Roots::new();
        for (networks, family, records, category) in [
            (
                Some("NETWORK_SENTINEL.invalid"),
                Some("forwarded"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (
                Some("0.0.0.0/0"),
                Some("forwarded"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (
                Some("::/0"),
                Some("forwarded"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (
                Some("0.0.0.0/1,128.0.0.0/1"),
                Some("forwarded"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (Some("127.0.0.2/32"), None, None, "TRUSTED_PROXY_POLICY"),
            (
                Some("127.0.0.2/32"),
                Some("FAMILY_SENTINEL"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (
                Some(""),
                Some("FAMILY_SENTINEL"),
                None,
                "TRUSTED_PROXY_POLICY",
            ),
            (None, None, Some("RECORDS_SENTINEL"), "REQUEST_RECORDS"),
            (None, None, Some("FALSE"), "REQUEST_RECORDS"),
        ] {
            let addr = address();
            let mut launch = command("diagnostics", &roots, addr);
            if let Some(value) = networks {
                launch.env("EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS", value);
            }
            if let Some(value) = family {
                launch.env("EDGEZERO__ADAPTER__FORWARDING_HEADER_FAMILY", value);
            }
            if let Some(value) = records {
                launch.env("EDGEZERO__LOGGING__REQUEST_RECORDS", value);
            }
            let log = failed(launch, addr, category);
            assert!(
                !log.contains("HOOK_ENTERED"),
                "invalid settings precede app construction"
            );
            assert!(!log.contains("phase=ready"));
            if let Some(value) = networks.filter(|text| !text.is_empty()) {
                assert!(!log.contains(value), "network input leaked: {log}");
            }
            if let Some(value) = records {
                assert!(!log.contains(value), "record-setting input leaked: {log}");
            }
        }
    }

    #[test]
    fn native_non_unicode_proxy_and_record_settings_fail_instead_of_defaulting() {
        use std::os::unix::ffi::OsStringExt as _;
        let roots = Roots::new();
        for (key, category) in [
            (
                "EDGEZERO__ADAPTER__TRUSTED_PROXY_CIDRS",
                "TRUSTED_PROXY_CIDRS",
            ),
            (
                "EDGEZERO__ADAPTER__FORWARDING_HEADER_FAMILY",
                "FORWARDING_HEADER_FAMILY",
            ),
            ("EDGEZERO__LOGGING__REQUEST_RECORDS", "REQUEST_RECORDS"),
        ] {
            let addr = address();
            let mut launch = command("diagnostics", &roots, addr);
            let mut raw = b"NON_UNICODE_SENTINEL".to_vec();
            raw.push(0xff);
            launch.env(key, OsString::from_vec(raw));
            let log = failed(launch, addr, category);
            assert!(
                log.contains("non-Unicode"),
                "captured native setting was rejected: {log}"
            );
            assert!(!log.contains("HOOK_ENTERED") && !log.contains("phase=ready"));
            assert!(
                !log.contains('\u{fffd}'),
                "no lossy rendering of non-Unicode input"
            );
        }
    }

    #[test]
    fn json_body_limit_wire_content_length_and_chunked() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("bare", &roots, addr), addr);
        server.ready();
        server.wait_line("HOOK_ENTERED");
        for chunked in [false, true] {
            assert_upload_status(
                &upload(
                    &server,
                    "/json",
                    Some("application/json"),
                    exact_json(0x0020_0000),
                    chunked,
                ),
                200,
            );
            server.wait_line("JSON_HANDLER_ENTERED");
            assert_upload_status(
                &upload(
                    &server,
                    "/json",
                    Some("application/json"),
                    exact_json(0x0020_0001),
                    chunked,
                ),
                413,
            );
        }
        server.signal("TERM");
        assert!(server.wait().success());
        assert_eq!(server.log().matches("JSON_HANDLER_ENTERED").count(), 2);
    }

    #[test]
    fn json_limit_overrides_helpers_generic_reads_and_taken_streams() {
        for mode in ["bare", "bare-dev", "embedding-bare"] {
            let roots = Roots::new();
            let addr = address();
            let mut launch = command(mode, &roots, addr);
            launch.env("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES", "64");
            if mode == "embedding-bare" {
                launch.env("FIXTURE_JSON_LIMIT", "64").env(
                    "EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES",
                    "INVALID_SENTINEL",
                );
            }
            let mut server = Server::spawn(launch, addr);
            server.ready();
            for media in [
                None,
                Some("text/plain"),
                Some("APPLICATION/JSON; charset=utf-8"),
                Some("application/vnd.fixture+json"),
            ] {
                assert_upload_status(&upload(&server, "/json", media, exact_json(64), false), 200);
                assert_upload_status(&upload(&server, "/json", media, exact_json(65), true), 413);
            }
            assert_upload_status(
                &upload(&server, "/json-explicit", None, exact_json(65), false),
                413,
            );
            assert_upload_status(
                &upload(&server, "/json-small", None, exact_json(32), false),
                200,
            );
            assert_upload_status(
                &upload(&server, "/json-small", None, exact_json(33), false),
                413,
            );
            assert_upload_status(
                &upload(&server, "/json", None, b"invalid json".to_vec(), false),
                400,
            );
            for media in [
                "application/json",
                "Application/Problem+JSON; charset=utf-8",
            ] {
                assert_upload_status(
                    &upload(&server, "/generic", Some(media), exact_json(65), true),
                    413,
                );
            }
            assert_upload_status(
                &upload(
                    &server,
                    "/generic",
                    Some("text/plain"),
                    exact_json(65),
                    false,
                ),
                200,
            );
            for media in ["application/json", "application/octet-stream"] {
                assert_upload_status(
                    &upload(&server, "/taken", Some(media), exact_json(4096), true),
                    200,
                );
            }
        }
        let roots = Roots::new();
        let addr = address();
        let cap = 9 * 1024 * 1024;
        let mut launch = command("bare", &roots, addr);
        launch.env("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES", cap.to_string());
        let mut server = Server::spawn(launch, addr);
        server.ready();
        assert_upload_status(&upload(&server, "/json", None, exact_json(cap), true), 200);
        assert_upload_status(
            &upload(&server, "/json", None, exact_json(cap + 1), false),
            413,
        );
    }

    #[test]
    fn invalid_json_limits_fail_before_application_hooks_in_dev_and_production() {
        use std::os::unix::ffi::OsStringExt as _;
        for mode in ["bare", "bare-dev"] {
            for value in [
                OsString::from("0"),
                OsString::from("INVALID_SENTINEL"),
                OsString::from_vec(vec![0xff]),
            ] {
                let roots = Roots::new();
                let addr = address();
                let mut launch = command(mode, &roots, addr);
                launch.env("EDGEZERO__ADAPTER__JSON_BODY_LIMIT_BYTES", value);
                let log = failed(launch, addr, "JSON_BODY_LIMIT_BYTES");
                assert!(!log.contains("HOOK_ENTERED"));
            }
        }
    }

    #[test]
    fn both_signals_stop_idle_processes() {
        for signal in ["INT", "TERM"] {
            let roots = Roots::new();
            let addr = address();
            let mut server = Server::spawn(command("bare", &roots, addr), addr);
            server.ready();
            server.signal(signal);
            assert!(server.wait().success(), "{}", server.log());
        }
    }

    #[test]
    fn sigterm_drains_dispatched_response_closes_idle_and_gates_pipeline() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("bare", &roots, addr), addr);
        server.ready();
        let mut idle = server.socket().expect("idle socket");
        idle.write_all(b"GET /ready HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("idle request");
        let mut idle_response = Vec::new();
        while !idle_response.ends_with(b"ready") {
            let mut buffer = [0_u8; 1024];
            let read = idle.read(&mut buffer).expect("idle response");
            assert_ne!(read, 0);
            idle_response.extend_from_slice(buffer.get(..read).expect("read length"));
        }
        let mut client = server.socket().expect("finite socket");
        client.write_all(b"GET /finite HTTP/1.1\r\nHost: fixture\r\n\r\nGET /after HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
        .expect("pipeline");
        server.wait_line("DISPATCH");
        server.signal("TERM");
        server.wait_line("DRAINING");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("complete finite response");
        assert!(response.contains("finite-complete"));
        assert!(!response.contains("forbidden"));
        assert_eq!(idle.read(&mut [0_u8; 1]).expect("idle close"), 0);
        assert!(server.wait().success(), "{}", server.log());
        assert!(!server.log().contains("FORBIDDEN_DISPATCH"));
    }

    #[test]
    fn process_budget_forces_pending_handler_stream_and_zero_read_peer() {
        for (path, marker, dropped) in [
            ("/pending", "DISPATCH", "WORK_DROPPED"),
            ("/stream", "STREAM_READY", "STREAM_DROPPED"),
            ("/flood", "STREAM_READY", "STREAM_DROPPED"),
        ] {
            let roots = Roots::new();
            let addr = address();
            let mut cmd = command("bare", &roots, addr);
            cmd.env("FIXTURE_FREEZE_CLOCK", "1");
            let mut server = Server::spawn(cmd, addr);
            server.ready();
            let mut client = server.socket().expect("pending socket");
            write!(client, "GET {path} HTTP/1.1\r\nHost: fixture\r\n\r\n").expect("request");
            server.wait_line(marker);
            let started = Instant::now();
            server.signal("TERM");
            assert!(!server.wait().success());
            assert!(started.elapsed() >= Duration::from_millis(700));
            assert!(started.elapsed() < Duration::from_secs(4));
            assert!(server.log().contains("grace exhausted"), "{}", server.log());
            assert_eq!(server.log().matches(dropped).count(), 1);
        }
    }

    #[test]
    fn second_signal_forces_before_budget_exhaustion() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("bare", &roots, addr), addr);
        server.ready();
        let mut client = server.socket().expect("pending socket");
        client
            .write_all(b"GET /pending HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("request");
        server.wait_line("DISPATCH");
        server.signal("TERM");
        server.wait_line("DRAINING");
        let started = Instant::now();
        server.signal("INT");
        assert!(!server.wait().success());
        assert!(started.elapsed() < Duration::from_millis(700));
        assert!(server.log().contains("forced stop"));
        assert_eq!(server.log().matches("WORK_DROPPED").count(), 1);
    }

    #[test]
    fn stop_during_startup_never_binds_or_becomes_ready() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("starting", &roots, addr), addr);
        server.wait_line("INITIALIZING");
        TcpStream::connect_timeout(&addr, Duration::from_millis(50))
            .expect_err("initialization precedes binding");
        server.signal("TERM");
        assert!(server.wait().success());
        assert!(server.log().contains("STARTUP_DROPPED"));
    }

    #[test]
    fn invalid_settings_and_callback_sources_are_redacted_nonzero_failures() {
        let roots = Roots::new();
        for (key, value, category) in [
            ("EDGEZERO__ADAPTER__HOST", "HOST_SENTINEL", "HOST"),
            ("EDGEZERO__ADAPTER__PORT", "PORT_SENTINEL", "PORT"),
            ("EDGEZERO__ADAPTER__PORT", "0", "PORT"),
            ("EDGEZERO__LOGGING__LEVEL", "LOGGING_SENTINEL", "LEVEL"),
            (
                "EDGEZERO__ADAPTER__SHUTDOWN_GRACE_SECONDS",
                "0",
                "SHUTDOWN_GRACE_SECONDS",
            ),
            (
                "EDGEZERO__ADAPTER__CONFIG_DIR",
                "ROOT_SENTINEL",
                "CONFIG_DIR",
            ),
            (
                "EDGEZERO__STORES__CONFIG__SETTINGS__KEY",
                "KEY_SENTINEL\n",
                "settings",
            ),
        ] {
            let addr = address();
            let mut cmd = command("initialized", &roots, addr);
            cmd.env(key, value);
            failed(cmd, addr, category);
        }
        for (mode, category) in [
            ("hook-error", "application configuration"),
            ("initializer-error", "initializer"),
        ] {
            let addr = address();
            failed(command(mode, &roots, addr), addr, category);
        }
        let addr = address();
        let mut cmd = command("initialized", &roots, addr);
        cmd.env_remove("EDGEZERO__ADAPTER__CONFIG_DIR");
        failed(cmd, addr, "CONFIG_DIR");
        let missing = roots.cwd.path().join("missing-data-SENTINEL");
        let missing_addr = address();
        let mut missing_cmd = command("initialized", &roots, missing_addr);
        missing_cmd.env("EDGEZERO__ADAPTER__DATA_DIR", &missing);
        failed(missing_cmd, missing_addr, "DATA_DIR");
        assert!(!missing.exists());
    }

    #[test]
    fn occupied_bind_is_a_real_redacted_process_failure() {
        let roots = Roots::new();
        let occupied = TcpListener::bind("127.0.0.1:0").expect("occupied port");
        let addr = occupied.local_addr().expect("address");
        let mut server = Server::spawn(command("bare", &roots, addr), addr);
        assert!(!server.wait().success());
        assert!(server.log().contains("bind"));
        assert!(!server.log().contains("SENTINEL"));
    }

    #[test]
    fn required_files_and_typed_initializer_fail_before_acceptance() {
        let roots = Roots::new();
        let file = roots.config.path().join("local-config-settings.json");
        fs::remove_file(&file).expect("missing file fixture");
        let addr = address();
        failed(command("initialized", &roots, addr), addr, "missing");
        fs::create_dir_all(&file).expect("non-regular fixture");
        let directory_addr = address();
        failed(
            command("initialized", &roots, directory_addr),
            directory_addr,
            "regular file",
        );
        fs::remove_dir(&file).expect("remove fixture directory");
        roots.map(&json!({"settings": {"CONFIG_SENTINEL": true}}));
        let malformed_addr = address();
        failed(
            command("initialized", &roots, malformed_addr),
            malformed_addr,
            "malformed",
        );
        for (map, category) in [
            (json!({}), "MissingBlob"),
            (json!({"settings": "BLOB_SENTINEL"}), "MalformedEnvelope"),
            (
                json!({"settings": blob("x", "fixture_token")}),
                "Validation",
            ),
            (
                json!({"settings": blob("valid-greeting", "MISSING_SECRET_SENTINEL")}),
                "MissingSecret",
            ),
        ] {
            roots.map(&map);
            let typed_addr = address();
            failed(
                command("initialized", &roots, typed_addr),
                typed_addr,
                category,
            );
        }
        let mut envelope: Value =
            serde_json::from_str(&blob("valid-greeting", "fixture_token")).expect("envelope");
        envelope["data"]["greeting"] = json!("CHANGED_CONFIG_SENTINEL");
        roots.map(&json!({"settings": serde_json::to_string(&envelope).expect("envelope")}));
        let integrity_addr = address();
        failed(
            command("initialized", &roots, integrity_addr),
            integrity_addr,
            "IntegrityMismatch",
        );
    }

    #[test]
    fn convenience_checks_framework_without_guessing_required_schema() {
        let roots = Roots::new();
        roots.map(&json!({"settings": "INVALID_TYPED_SENTINEL"}));
        let addr = address();
        let mut server = Server::spawn(command("framework", &roots, addr), addr);
        server.ready();
        assert!(
            !server
                .request("/typed")
                .expect("typed failure")
                .starts_with("HTTP/1.1 200")
        );
        server.signal("TERM");
        assert!(server.wait().success());
        assert!(!server.log().contains("INITIALIZED"));
    }

    #[test]
    fn retained_state_key_aliases_snapshot_restart_and_closed_backup_restore() {
        let roots = Roots::new();
        roots.map(&json!({"settings": blob("wrong-key-greeting", "fixture_token"), "selected": blob("selected-greeting", "fixture_token")}));
        let addr = address();
        let mut cmd = command("initialized", &roots, addr);
        cmd.env("EDGEZERO__STORES__CONFIG__SETTINGS__KEY", "selected");
        let mut server = Server::spawn(cmd, addr);
        server.ready();
        for _request in 0..3_u8 {
            assert!(
                server
                    .request("/state")
                    .expect("state")
                    .ends_with("selected-greeting")
            );
            assert!(
                server
                    .request("/typed")
                    .expect("typed")
                    .ends_with("selected-greeting")
            );
        }
        assert!(
            server
                .request("/write")
                .expect("write")
                .ends_with("written")
        );
        assert!(
            server
                .request("/alias")
                .expect("alias")
                .ends_with("durable-value")
        );
        assert!(
            !server
                .request("/distinct")
                .expect("distinct")
                .contains("durable-value")
        );
        roots.map(&json!({"settings": blob("restart-greeting", "fixture_token")}));
        assert!(
            server
                .request("/typed")
                .expect("snapshot")
                .ends_with("selected-greeting")
        );
        let second_addr = address();
        failed(
            command("initialized", &roots, second_addr),
            second_addr,
            "locked",
        );
        assert_eq!(
            fs::read_dir(roots.data.path()).expect("databases").count(),
            2
        );
        server.signal("TERM");
        assert!(server.wait().success());
        assert_eq!(server.log().matches("INITIALIZED").count(), 1);
        let mut restarted = Server::spawn(command("initialized", &roots, addr), addr);
        restarted.ready();
        assert!(
            restarted
                .request("/state")
                .expect("restarted config")
                .ends_with("restart-greeting")
        );
        assert!(
            restarted
                .request("/read")
                .expect("persisted")
                .ends_with("durable-value")
        );
        restarted.signal("TERM");
        assert!(restarted.wait().success());
        let restore = tempfile::tempdir().expect("restore root");
        for result in fs::read_dir(roots.data.path()).expect("closed databases") {
            let entry = result.expect("database entry");
            fs::copy(entry.path(), restore.path().join(entry.file_name()))
                .expect("stopped-writer backup");
        }
        let mut restored_command = command("initialized", &roots, addr);
        restored_command.env("EDGEZERO__ADAPTER__DATA_DIR", restore.path());
        let mut restored = Server::spawn(restored_command, addr);
        restored.ready();
        assert!(
            restored
                .request("/read")
                .expect("restored value")
                .ends_with("durable-value")
        );
        restored.signal("TERM");
        assert!(restored.wait().success());
    }

    #[test]
    fn explicit_embedding_ignores_ambient_settings_and_uses_local_rc_stop() {
        let roots = Roots::new();
        let addr = address();
        let mut cmd = command("embedding", &roots, addr);
        cmd.env("EDGEZERO__ADAPTER__HOST", "IGNORED_HOST_SENTINEL")
            .env("EDGEZERO__ADAPTER__PORT", "0")
            .env("EDGEZERO__ADAPTER__CONFIG_DIR", "IGNORED_ROOT_SENTINEL")
            .env(
                "EDGEZERO__STORES__CONFIG__SETTINGS__KEY",
                "IGNORED_KEY_SENTINEL",
            );
        let mut server = Server::spawn(cmd, addr);
        server.ready();
        assert!(
            server
                .request("/state")
                .expect("state")
                .ends_with("configured-greeting")
        );
        assert!(
            server
                .request("/stop")
                .expect("caller stop")
                .ends_with("stopping")
        );
        assert!(server.wait().success(), "{}", server.log());
        assert_eq!(server.log().matches("INITIALIZED").count(), 1);
    }

    #[test]
    fn embedding_does_not_install_process_signal_handlers() {
        let roots = Roots::new();
        let addr = address();
        let mut server = Server::spawn(command("embedding-bare", &roots, addr), addr);
        server.ready();
        server.signal("TERM");
        assert_eq!(server.wait().signal(), Some(15_i32));
    }

    #[test]
    fn projected_readonly_config_and_writable_data_preserve_uid_and_permissions() {
        let roots = Roots::new();
        let file = roots.config.path().join("local-config-settings.json");
        let target = roots.config.path().join("projected-value");
        fs::rename(&file, &target).expect("projected target");
        symlink(&target, &file).expect("projected symlink");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).expect("read-only config");
        fs::set_permissions(roots.config.path(), fs::Permissions::from_mode(0o555))
            .expect("read-only root");
        let config_before = fs::metadata(roots.config.path()).expect("config metadata");
        let data_before = fs::metadata(roots.data.path()).expect("data metadata");
        let addr = address();
        let mut server = Server::spawn(command("initialized", &roots, addr), addr);
        server.ready();
        assert!(
            server
                .request("/write")
                .expect("write")
                .ends_with("written")
        );
        server.signal("TERM");
        assert!(server.wait().success());
        let config_after = fs::metadata(roots.config.path()).expect("config metadata");
        let data_after = fs::metadata(roots.data.path()).expect("data metadata");
        assert_eq!(
            (config_before.uid(), config_before.mode()),
            (config_after.uid(), config_after.mode())
        );
        assert_eq!(
            (data_before.uid(), data_before.mode()),
            (data_after.uid(), data_after.mode())
        );
        fs::set_permissions(roots.config.path(), fs::Permissions::from_mode(0o700))
            .expect("fixture cleanup");
    }
}
