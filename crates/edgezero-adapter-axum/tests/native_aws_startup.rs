//! Loopback-only production startup test proposal.
//!
//! Proposed Cargo wiring for this file and `fixtures/native_aws_host.rs`:
//!
//! ```toml
//! [[bin]]
//! name = "edgezero-native-aws-host"
//! path = "tests/fixtures/native_aws_host.rs"
//! required-features = ["aws-stores", "test-utils"]
//! test = false
//!
//! [[test]]
//! name = "native_aws_startup"
//! path = "tests/native_aws_startup.rs"
//! required-features = ["aws-stores", "test-utils"]
//! ```
//!
//! It exercises the current public runner and real pinned SDK, not a provider seam. Run only
//! after the parent integrates this draft and the completed SDK/provider code. No AWS account,
//! DNS hostname, external socket, image, or real cloud credential is used.

#[cfg(test)]
#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "loopback fixture helpers remain grouped before startup scenarios"
)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::io::{ErrorKind, Read, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::{Value, json};
    use tempfile::TempDir;

    const WAIT: Duration = Duration::from_secs(12);
    const ARN: &str = "arn:aws:secretsmanager:us-east-1:111122223333:secret:native-fixture-AbCdEf";
    const TOKEN_SENTINEL: &str = "native-fixture-secret-value";
    const LOCAL_SENTINEL: &str = "native-fixture-local-secret";
    const AGENT_TOKEN: &str = "native-fixture-agent-token";
    const FALLBACK_SENTINEL: &str = "local-fallback-must-not-be-used";

    #[derive(Debug)]
    struct Request {
        headers: String,
        body: String,
    }

    struct Reply {
        status: u16,
        reason: &'static str,
        body: String,
        content_type: &'static str,
    }

    impl Reply {
        fn ok(body: String, content_type: &'static str) -> Self {
            Self {
                status: 200,
                reason: "OK",
                body,
                content_type,
            }
        }
    }

    /// One finite server bound to an IPv4 loopback literal. `expected=false` watches briefly for an
    /// unexpected request, which makes the Agent-failure/no-Secrets-call assertion observable.
    struct FixtureServer {
        endpoint: String,
        request: mpsc::Receiver<Request>,
        worker: Option<JoinHandle<Option<Request>>>,
    }

    impl FixtureServer {
        fn once(reply: Reply, expected: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind literal loopback only");
            listener
                .set_nonblocking(true)
                .expect("nonblocking loopback listener");
            let address = listener.local_addr().expect("fixture address");
            let endpoint = format!("http://{address}");
            let (sender, request_receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                let watch = if expected {
                    WAIT
                } else {
                    Duration::from_millis(300)
                };
                let deadline = Instant::now()
                    .checked_add(watch)
                    .expect("fixture deadline fits the monotonic clock");
                loop {
                    match listener.accept() {
                        Ok((mut stream, peer)) => {
                            assert!(peer.ip().is_loopback(), "fixture peer was not loopback");
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .expect("bounded request read");
                            stream
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .expect("bounded response write");
                            let request = read_request(&mut stream);
                            send_response(&mut stream, &reply);
                            let _sent = sender.send(Request {
                                headers: request.headers.clone(),
                                body: request.body.clone(),
                            });
                            return Some(request);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                assert!(!expected, "expected fixture request did not arrive");
                                return None;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("loopback fixture accept failed: {error}"),
                    }
                }
            });
            Self {
                endpoint,
                request: request_receiver,
                worker: Some(worker),
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        fn take_request(&self) -> Request {
            self.request
                .recv_timeout(WAIT)
                .expect("fixture captured one request")
        }

        #[expect(
            clippy::unwrap_in_result,
            reason = "a broken fixture or panicked thread must fail the test, never become a normal absence"
        )]
        fn finish(mut self) -> Option<Request> {
            self.worker
                .take()
                .expect("fixture worker")
                .join()
                .expect("fixture thread")
        }
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        let mut bytes = Vec::new();
        let mut scratch = [0_u8; 2048];
        let header_end = loop {
            let count = stream.read(&mut scratch).expect("read request");
            assert_ne!(count, 0, "peer closed before request headers");
            let received = scratch
                .get(..count)
                .expect("read count does not exceed scratch buffer");
            bytes.extend_from_slice(received);
            assert!(bytes.len() <= 64 * 1024, "bounded fixture request");
            if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break offset
                    .checked_add(4)
                    .expect("header delimiter offset fits request");
            }
        };
        let header_bytes = bytes
            .get(..header_end)
            .expect("header terminator is within received request");
        let headers = String::from_utf8_lossy(header_bytes).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (_, value) = line
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))?;
                value.trim().parse::<usize>().ok()
            })
            .unwrap_or(0);
        let body_end = header_end
            .checked_add(content_length)
            .expect("bounded request body end fits usize");
        while bytes.len() < body_end {
            let count = stream.read(&mut scratch).expect("read bounded body");
            assert_ne!(count, 0, "peer closed before request body");
            let received = scratch
                .get(..count)
                .expect("read count does not exceed scratch buffer");
            bytes.extend_from_slice(received);
            assert!(
                bytes.len() <= body_end,
                "request body stays within declared length"
            );
        }
        let body_bytes = bytes
            .get(header_end..body_end)
            .expect("declared request body is within received bytes");
        Request {
            headers: headers.to_ascii_lowercase(),
            body: String::from_utf8_lossy(body_bytes).into_owned(),
        }
    }

    fn send_response(stream: &mut TcpStream, reply: &Reply) {
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.reason,
            reply.content_type,
            reply.body.len()
        )
        .expect("write response headers");
        stream
            .write_all(reply.body.as_bytes())
            .expect("write response body");
    }

    struct ChildHost {
        child: Child,
        address: SocketAddr,
        output: Arc<Mutex<Vec<u8>>>,
        readers: Vec<JoinHandle<()>>,
    }

    impl ChildHost {
        fn spawn(root: &TempDir, bindings: &Path, secrets_endpoint: &str) -> Self {
            let home = root.path().join("home");
            let profiles = root.path().join("profiles");
            fs::create_dir_all(&home).expect("isolated HOME");
            fs::create_dir_all(&profiles).expect("isolated profile directory");
            let config = profiles.join("config");
            let credentials = profiles.join("credentials");
            fs::write(&config, "").expect("empty AWS config; no profiles");
            fs::write(&credentials, "").expect("empty AWS credentials; no profiles");
            let address = reserve_address();
            let mut child = Command::new(env!("CARGO_BIN_EXE_edgezero-native-aws-host"))
                .env_clear()
                .current_dir(root.path())
                .env("HOME", home)
                .env("AWS_CONFIG_FILE", config)
                .env("AWS_SHARED_CREDENTIALS_FILE", credentials)
                .env("AWS_REGION", "us-east-1")
                .env("AWS_EC2_METADATA_DISABLED", "true")
                .env("AWS_ACCESS_KEY_ID", "DUMMY_LOOPBACK_ACCESS_KEY")
                .env("AWS_SECRET_ACCESS_KEY", "DUMMY_LOOPBACK_SECRET_KEY")
                .env("AWS_ENDPOINT_URL", secrets_endpoint)
                .env("AWS_ENDPOINT_URL_SECRETS_MANAGER", secrets_endpoint)
                .env("EDGEZERO__STORE_BINDINGS_FILE", bindings)
                .env("EDGEZERO__ADAPTER__HOST", "127.0.0.1")
                .env("EDGEZERO__ADAPTER__PORT", address.port().to_string())
                .env("EDGEZERO__LOGGING__LEVEL", "off")
                .env("APPCONFIG_AGENT_TOKEN", AGENT_TOKEN)
                .env("LOCAL_FIXTURE_SECRET", LOCAL_SENTINEL)
                .env("token", FALLBACK_SENTINEL)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start production host fixture");
            let output = Arc::new(Mutex::new(Vec::new()));
            let streams: [Box<dyn Read + Send>; 2] = [
                Box::new(child.stdout.take().expect("child stdout")),
                Box::new(child.stderr.take().expect("child stderr")),
            ];
            let readers = streams
                .into_iter()
                .map(|mut stream| {
                    let captured_output = Arc::clone(&output);
                    thread::spawn(move || {
                        let mut captured = Vec::new();
                        stream
                            .read_to_end(&mut captured)
                            .expect("capture child output");
                        captured_output
                            .lock()
                            .expect("captured output")
                            .extend(captured);
                    })
                })
                .collect();
            Self {
                child,
                address,
                output,
                readers,
            }
        }

        fn wait_http(&mut self, path: &str) -> String {
            let deadline = Instant::now()
                .checked_add(WAIT)
                .expect("host readiness deadline fits the monotonic clock");
            loop {
                if let Some(status) = self.child.try_wait().expect("child status") {
                    self.child.wait().expect("reap exited child");
                    let output = self.collect_output();
                    panic!("fixture exited before {path} (status {status}): {output}");
                }
                if let Ok(mut stream) =
                    TcpStream::connect_timeout(&self.address, Duration::from_millis(40))
                {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("bounded app response read");
                    write!(
                        stream,
                        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                    )
                    .expect("write app request");
                    let mut response = String::new();
                    stream
                        .read_to_string(&mut response)
                        .expect("read complete app response");
                    if response.starts_with("HTTP/1.1 200") {
                        return response;
                    }
                }
                assert!(Instant::now() < deadline, "host readiness deadline");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn wait_failure(&mut self) -> String {
            let deadline = Instant::now()
                .checked_add(WAIT)
                .expect("startup failure deadline fits the monotonic clock");
            loop {
                if let Some(status) = self.child.try_wait().expect("child status") {
                    assert!(!status.success(), "expected startup failure");
                    self.child.wait().expect("reap failed child");
                    return self.collect_output();
                }
                assert!(Instant::now() < deadline, "failed startup stayed alive");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn stop(&mut self) {
            if self.child.try_wait().expect("child status").is_none() {
                self.child.kill().expect("stop loopback host");
            }
            self.child.wait().expect("reap host");
            let _output = self.collect_output();
        }

        fn collect_output(&mut self) -> String {
            for reader in self.readers.drain(..) {
                reader.join().expect("child output reader");
            }
            String::from_utf8_lossy(&self.output.lock().expect("captured output")).into_owned()
        }

        fn assert_no_listener(&self) {
            TcpStream::connect_timeout(&self.address, Duration::from_millis(50))
                .expect_err("startup failure must happen before listener acceptance");
        }
    }

    impl Drop for ChildHost {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _killed = self.child.kill();
            }
            let _status = self.child.wait();
            for reader in self.readers.drain(..) {
                let _joined = reader.join();
            }
        }
    }

    fn reserve_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve literal loopback port");
        listener.local_addr().expect("reserved address")
    }

    fn response_body(response: &str) -> &str {
        response
            .split_once("\r\n\r\n")
            .expect("HTTP response header terminator")
            .1
    }

    fn envelope(token_reference: &str, greeting: &str) -> String {
        serde_json::to_string(&BlobEnvelope::new(
            json!({"greeting": greeting, "token": token_reference}),
            "2026-10-07T12:00:00Z".to_owned(),
        ))
        .expect("serialize actual core BlobEnvelope")
    }

    fn agent_reply(document: &str, status: u16) -> Reply {
        if status == 200 {
            Reply::ok(document.to_owned(), "application/json")
        } else {
            Reply {
                status,
                reason: "Service Unavailable",
                body: "AGENT_BODY_SENTINEL".to_owned(),
                content_type: "text/plain",
            }
        }
    }

    fn secrets_reply(value: &str) -> Reply {
        Reply::ok(
            json!({
                "ARN": ARN,
                "Name": "native-fixture",
                "VersionId": "12345678901234567890123456789012",
                "SecretString": value,
            })
            .to_string(),
            "application/x-amz-json-1.1",
        )
    }

    fn denied_secrets_reply() -> Reply {
        Reply {
            status: 400,
            reason: "Bad Request",
            body: json!({
                "__type": "AccessDeniedException",
                "message": "AWS_ERROR_SENTINEL ARN_SENTINEL CREDENTIAL_SENTINEL",
            })
            .to_string(),
            content_type: "application/x-amz-json-1.1",
        }
    }

    fn bindings_file(path: &Path, agent: &str, aws_limits: Option<(usize, usize, usize)>) {
        let mut content = format!(
            r#"version = 1

[config.app]
provider = "aws-appconfig-agent"
default_key = "settings"
endpoint = "{agent}"
access_token_env = "APPCONFIG_AGENT_TOKEN"

[config.app.documents.settings]
application = "native-fixture"
environment = "test"
profile = "startup-proof"

[secrets.vault]
provider = "aws-secrets-manager"
region = "us-east-1"

[secrets.vault.entries.token]
secret_id = "{ARN}"
"#
        );
        if let Some((snapshot, config, secret)) = aws_limits {
            write!(content, "\n[limits.aws]\nmax_snapshot_bytes = {snapshot}\nmax_config_document_bytes = {config}\nmax_secret_value_bytes = {secret}\n").expect("format fixture limits");
        }
        fs::write(path, content).expect("write explicit fictional bindings");
    }

    fn assert_safe_error(text: &str) {
        for sentinel in [
            TOKEN_SENTINEL,
            LOCAL_SENTINEL,
            AGENT_TOKEN,
            FALLBACK_SENTINEL,
            "AGENT_BODY_SENTINEL",
            "AWS_ERROR_SENTINEL",
            "ARN_SENTINEL",
            "CREDENTIAL_SENTINEL",
            "DUMMY_LOOPBACK_ACCESS_KEY",
            "DUMMY_LOOPBACK_SECRET_KEY",
        ] {
            assert!(!text.contains(sentinel), "startup output leaked {sentinel}");
        }
    }

    fn request_body_is_serialized_get_secret(request: &Request) {
        assert!(
            request
                .headers
                .contains("x-amz-target: secretsmanager.getsecretvalue"),
            "real SDK request target observed"
        );
        assert!(
            request
                .headers
                .contains("credential=dummy_loopback_access_key/")
        );
        let body: Value = serde_json::from_str(&request.body).expect("SDK JSON request body");
        assert_eq!(body["SecretId"], ARN);
        assert!(body.get("VersionStage").is_none());
        assert!(body.get("VersionId").is_none());
    }

    #[test]
    fn production_runner_loads_envelope_and_real_sdk_secret_before_serving_mixed_stores() {
        let root = tempfile::tempdir().expect("temporary process root");
        let document = envelope("token", "startup-greeting");
        let agent = FixtureServer::once(agent_reply(&document, 200), true);
        let secrets = FixtureServer::once(secrets_reply(TOKEN_SENTINEL), true);
        let bindings = root.path().join("native-stores.toml");
        bindings_file(&bindings, agent.endpoint(), None);
        let mut child = ChildHost::spawn(&root, &bindings, secrets.endpoint());

        // The app initializer loads the actual serialized BlobEnvelope through core AppConfig,
        // resolves its #[secret] field from the SDK-backed snapshot, and inserts typed state.
        let first = child.wait_http("/snapshot");
        assert_eq!(
            response_body(&first),
            "startup-greeting|native-fixture-secret-value"
        );
        let second = child.wait_http("/snapshot");
        assert_eq!(
            response_body(&second),
            "startup-greeting|native-fixture-secret-value"
        );
        let local = child.wait_http("/local-secret");
        assert_eq!(response_body(&local), "native-fixture-local-secret");

        let agent_request = agent.take_request();
        assert!(agent_request.headers.starts_with(
            "get /applications/native-fixture/environments/test/configurations/startup-proof "
        ));
        assert!(
            agent_request
                .headers
                .contains(&format!("authorization: bearer {AGENT_TOKEN}"))
        );
        assert_eq!(agent_request.body, "");
        assert!(agent_request.headers.contains("accept-encoding: identity"));
        let secret_request = secrets.take_request();
        request_body_is_serialized_get_secret(&secret_request);
        assert_eq!(
            agent.finish().expect("Agent call").headers,
            agent_request.headers
        );
        assert_eq!(
            secrets.finish().expect("SDK call").body,
            secret_request.body
        );

        // Each fixture accepted exactly one startup GET. Both request-time reads still succeeded
        // after their fixture listeners closed, so no request-time AWS call occurred.
        child.stop();
    }

    #[test]
    fn agent_startup_error_stops_before_secret_client_request_and_listener() {
        let root = tempfile::tempdir().expect("temporary process root");
        let agent = FixtureServer::once(agent_reply("AGENT_BODY_SENTINEL", 503), true);
        let secrets = FixtureServer::once(denied_secrets_reply(), false);
        let bindings = root.path().join("native-stores.toml");
        bindings_file(&bindings, agent.endpoint(), None);
        let mut child = ChildHost::spawn(&root, &bindings, secrets.endpoint());
        let output = child.wait_failure();
        assert_safe_error(&output);
        child.assert_no_listener();
        assert!(agent.finish().is_some(), "Agent request observed");
        assert!(
            secrets.finish().is_none(),
            "no SDK request after Agent failure"
        );
    }

    #[test]
    fn explicit_secret_denial_fails_without_local_fallback_or_listener() {
        let root = tempfile::tempdir().expect("temporary process root");
        let document = envelope("token", "startup-greeting");
        let agent = FixtureServer::once(agent_reply(&document, 200), true);
        let secrets = FixtureServer::once(denied_secrets_reply(), true);
        let bindings = root.path().join("native-stores.toml");
        bindings_file(&bindings, agent.endpoint(), None);
        let mut child = ChildHost::spawn(&root, &bindings, secrets.endpoint());
        let output = child.wait_failure();
        assert_safe_error(&output);
        child.assert_no_listener();
        assert!(agent.finish().is_some());
        let request = secrets.finish().expect("real SDK request captured");
        request_body_is_serialized_get_secret(&request);
    }

    #[test]
    fn nested_secret_miss_fails_initializer_before_listener() {
        let root = tempfile::tempdir().expect("temporary process root");
        let document = envelope("unmapped-secret-key", "startup-greeting");
        let agent = FixtureServer::once(agent_reply(&document, 200), true);
        let secrets = FixtureServer::once(secrets_reply(TOKEN_SENTINEL), true);
        let bindings = root.path().join("native-stores.toml");
        bindings_file(&bindings, agent.endpoint(), None);
        let mut child = ChildHost::spawn(&root, &bindings, secrets.endpoint());
        let output = child.wait_failure();
        assert_safe_error(&output);
        child.assert_no_listener();
        assert!(agent.finish().is_some());
        assert!(secrets.finish().is_some());
    }

    #[test]
    fn shared_payload_quota_spans_appconfig_and_secrets_before_readiness() {
        let root = tempfile::tempdir().expect("temporary process root");
        let document = envelope("token", "startup-greeting");
        let secret = TOKEN_SENTINEL;
        let aggregate_cap = document.len().max(secret.len());
        let combined_payload_bytes = document
            .len()
            .checked_add(secret.len())
            .expect("fixture payload lengths fit usize");
        assert!(aggregate_cap < combined_payload_bytes);
        let agent = FixtureServer::once(agent_reply(&document, 200), true);
        let secrets = FixtureServer::once(secrets_reply(secret), true);
        let bindings = root.path().join("native-stores.toml");
        bindings_file(
            &bindings,
            agent.endpoint(),
            Some((aggregate_cap, document.len(), secret.len())),
        );
        let mut child = ChildHost::spawn(&root, &bindings, secrets.endpoint());
        let output = child.wait_failure();
        assert_safe_error(&output);
        assert!(
            output.contains("size limit"),
            "aggregate quota category: {output}"
        );
        child.assert_no_listener();
        assert!(agent.finish().is_some(), "config snapshot charged first");
        assert!(
            secrets.finish().is_some(),
            "secret response fetched before aggregate charge"
        );
    }
}
