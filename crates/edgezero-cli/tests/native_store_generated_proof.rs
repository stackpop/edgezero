//! Ignored end-to-end proof proposal for generated native AWS stores.
//!
//! Copy to `crates/edgezero-cli/tests/native_store_generated_proof.rs` after coordinator review.
//! It invokes the real `edgezero new` and `edgezero build --adapter axum -- --offline` commands,
//! builds the generated Axum target with default-off, Agent-only, Secrets-only and combined
//! manifest feature selections, and launches its binary.
//!
//! The untouched generated main calls `run_generated_app`, proving explicit AWS bindings fail
//! closed when services were not compiled. The success main is replaced only in the temporary
//! generated project with an application-owned public production initializer. That initializer
//! loads the real serialized envelope and resolves its nested `#[secret]` reference before
//! inserting typed state. The test holds both provider responses and the initializer on gates,
//! then confirms the generated server does not listen until they complete.
//!
//! All service traffic uses short-lived literal `127.0.0.1` fixtures. The generated child starts
//! with `env_clear`, empty profile files and HOME, dummy credentials, metadata lookup disabled,
//! and both `AWS_ENDPOINT_URL_SECRETS_MANAGER` and the global endpoint override pointing to the
//! Secrets Manager fixture. No AWS account or real credentials, DNS, external socket, image or
//! IAM action is involved. `EDGEZERO_GENERATED_TEST_TARGET_DIR` may point at a coordinator-selected target
//! directory; otherwise builds use the generated project's `target/`. Keep this test ignored;
//! the coordinator owns any Cargo command used to run it.

#[cfg(test)]
#[cfg(unix)]
mod tests {

    use std::env::var_os;
    use std::fs;
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Output, Stdio};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use toml::Value as TomlValue;

    const WAIT: Duration = Duration::from_secs(20);
    const SERVICE_GRACE: Duration = Duration::from_millis(400);
    const SECRET_ARN: &str =
        "arn:aws:secretsmanager:us-east-1:111122223333:secret:native-probe-signing-AbCdEf";
    const APP_TOKEN: &str = "generated-proof-secret-value";
    const AGENT_TOKEN: &str = "generated-proof-agent-token";
    const FALLBACK_SENTINEL: &str = "must-not-fall-back-to-environment";

    #[derive(Debug)]
    struct HttpRequest {
        body: String,
        headers: String,
    }

    struct HttpReply {
        body: String,
        content_type: &'static str,
        reason: &'static str,
        status: u16,
    }

    impl HttpReply {
        fn ok(body: String, content_type: &'static str) -> Self {
            Self {
                body,
                content_type,
                reason: "OK",
                status: 200,
            }
        }
    }

    /// A gated one-request HTTP/1 fixture. It holds the first response until the test releases it,
    /// then watches for extra calls while the generated app serves requests from its snapshots.
    struct GatedService {
        endpoint: String,
        finish_tx: Sender<()>,
        reply_tx: Sender<HttpReply>,
        request_rx: Receiver<HttpRequest>,
        worker: Option<JoinHandle<Vec<HttpRequest>>>,
    }

    impl GatedService {
        fn bind() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind literal loopback");
            listener
                .set_nonblocking(true)
                .expect("nonblocking fixture listener");
            let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
            let (request_tx, request_rx) = mpsc::channel();
            let (reply_tx, reply_rx) = mpsc::channel();
            let (finish_tx, finish_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let (mut startup_stream, _) = accept_before(&listener, deadline_after(WAIT));
                startup_stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("bounded fixture request read");
                startup_stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .expect("bounded fixture response write");
                let startup_request = read_request(&mut startup_stream);
                request_tx
                    .send(startup_request)
                    .expect("parent receives fixture request");
                let reply = reply_rx
                    .recv_timeout(WAIT)
                    .expect("test releases fixture response");
                write_reply(&mut startup_stream, &reply);
                finish_rx
                    .recv_timeout(WAIT)
                    .expect("parent completes snapshot assertions");

                let mut extra = Vec::new();
                let deadline = deadline_after(SERVICE_GRACE);
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut extra_stream, peer)) => {
                            assert!(peer.ip().is_loopback(), "only literal loopback peers");
                            extra_stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .expect("bounded extra request read");
                            let extra_request = read_request(&mut extra_stream);
                            let _sent = request_tx.send(HttpRequest {
                                body: extra_request.body.clone(),
                                headers: extra_request.headers.clone(),
                            });
                            extra.push(extra_request);
                            write_reply(
                                &mut extra_stream,
                                &HttpReply {
                                    body: "REQUEST_TIME_PROVIDER_ACCESS".to_owned(),
                                    content_type: "text/plain",
                                    reason: "Unexpected request",
                                    status: 500,
                                },
                            );
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                }
                extra
            });
            Self {
                endpoint,
                finish_tx,
                reply_tx,
                request_rx,
                worker: Some(worker),
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        fn finish(mut self) -> Vec<HttpRequest> {
            self.finish_tx
                .send(())
                .expect("fixture waits until handler assertions finish");
            self.worker
                .take()
                .expect("fixture worker")
                .join()
                .expect("fixture thread")
        }

        fn reply(&self, reply: HttpReply) {
            self.reply_tx.send(reply).expect("service awaits response");
        }

        fn request(&self) -> HttpRequest {
            self.request_rx
                .recv_timeout(WAIT)
                .expect("startup request reaches loopback service")
        }
    }

    struct RunningHost {
        address: SocketAddr,
        child: Child,
        stderr: Option<JoinHandle<Vec<u8>>>,
        stdout: Option<JoinHandle<Vec<u8>>>,
    }

    impl RunningHost {
        fn http(&mut self, path: &str) -> String {
            let deadline = deadline_after(WAIT);
            loop {
                if let Some(status) = self.child.try_wait().expect("generated process status") {
                    panic!("generated server exited before {path} ({status})");
                }
                if let Ok(mut stream) =
                    TcpStream::connect_timeout(&self.address, Duration::from_millis(40))
                {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("bounded generated response");
                    write!(
                        stream,
                        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                    )
                    .expect("send generated handler request");
                    let mut response = String::new();
                    stream
                        .read_to_string(&mut response)
                        .expect("read generated response");
                    if response.starts_with("HTTP/1.1 200") {
                        return response;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "generated listener startup timeout"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn spawn(mut command: Command, address: SocketAddr) -> Self {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawn generated server");
            let stdout_pipe = child.stdout.take().expect("capture generated stdout");
            let stderr_pipe = child.stderr.take().expect("capture generated stderr");
            let stdout_reader = thread::spawn(move || {
                let mut output = Vec::new();
                let mut pipe = stdout_pipe;
                pipe.read_to_end(&mut output)
                    .expect("read generated stdout");
                output
            });
            let stderr_reader = thread::spawn(move || {
                let mut output = Vec::new();
                let mut pipe = stderr_pipe;
                pipe.read_to_end(&mut output)
                    .expect("read generated stderr");
                output
            });
            Self {
                address,
                child,
                stderr: Some(stderr_reader),
                stdout: Some(stdout_reader),
            }
        }

        fn stop(mut self) -> Output {
            if self
                .child
                .try_wait()
                .expect("generated process state")
                .is_none()
            {
                let _killed = self.child.kill();
            }
            let status = self.child.wait().expect("reap generated app");
            let stdout = self
                .stdout
                .take()
                .expect("stdout reader")
                .join()
                .expect("stdout thread");
            let stderr = self
                .stderr
                .take()
                .expect("stderr reader")
                .join()
                .expect("stderr thread");
            Output {
                status,
                stderr,
                stdout,
            }
        }
    }

    impl Drop for RunningHost {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _killed = self.child.kill();
            }
            let _status = self.child.wait();
            if let Some(stderr) = self.stderr.take() {
                let _output = stderr.join();
            }
            if let Some(stdout) = self.stdout.take() {
                let _output = stdout.join();
            }
        }
    }

    fn deadline_after(duration: Duration) -> Instant {
        Instant::now()
            .checked_add(duration)
            .expect("deadline within representable range")
    }

    fn accept_before(listener: &TcpListener, deadline: Instant) -> (TcpStream, SocketAddr) {
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    assert!(peer.ip().is_loopback(), "fixture peer must be loopback");
                    return (stream, peer);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture request deadline");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> HttpRequest {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 2048];
        let header_end = loop {
            let read = stream.read(&mut chunk).expect("read request headers");
            assert_ne!(read, 0, "peer closed before request headers");
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= 64 * 1024, "fixture request header bound");
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index.checked_add(4).expect("header end offset");
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (_, value) = line
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))?;
                value.trim().parse::<usize>().ok()
            })
            .unwrap_or(0);
        assert!(content_length <= 64 * 1024, "fixture request body bound");
        let body_end = header_end
            .checked_add(content_length)
            .expect("request body end offset");
        while bytes.len() < body_end {
            let read = stream.read(&mut chunk).expect("read request body");
            assert_ne!(read, 0, "peer closed before request body");
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= body_end, "bounded request body");
        }
        HttpRequest {
            body: String::from_utf8_lossy(&bytes[header_end..body_end]).into_owned(),
            headers: headers.to_ascii_lowercase(),
        }
    }

    fn write_reply(stream: &mut TcpStream, reply: &HttpReply) {
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.reason,
            reply.content_type,
            reply.body.len()
        )
        .expect("write fixture response headers");
        stream
            .write_all(reply.body.as_bytes())
            .expect("write fixture response body");
    }

    fn toml_table(entries: impl IntoIterator<Item = (String, TomlValue)>) -> TomlValue {
        TomlValue::Table(entries.into_iter().collect())
    }

    fn toml_strings(values: &[&str]) -> TomlValue {
        TomlValue::Array(
            values
                .iter()
                .map(|value| TomlValue::String((*value).to_owned()))
                .collect(),
        )
    }

    fn install_store_declarations(project: &Path) {
        let manifest_path = project.join("edgezero.toml");
        let source = fs::read_to_string(&manifest_path).expect("scaffold manifest");
        let mut manifest: TomlValue = toml::from_str(&source).expect("parse scaffold TOML");
        let root = manifest.as_table_mut().expect("manifest root table");
        let stores = toml_table([
            (
                "config".to_owned(),
                toml_table([
                    ("ids".to_owned(), toml_strings(&["app_config"])),
                    (
                        "default".to_owned(),
                        TomlValue::String("app_config".to_owned()),
                    ),
                ]),
            ),
            (
                "secrets".to_owned(),
                toml_table([
                    ("ids".to_owned(), toml_strings(&["signing"])),
                    (
                        "default".to_owned(),
                        TomlValue::String("signing".to_owned()),
                    ),
                ]),
            ),
        ]);
        root.insert("stores".to_owned(), stores);
        fs::write(
            &manifest_path,
            toml::to_string(&manifest).expect("serialize manifest"),
        )
        .expect("write portable store IDs");
    }

    fn set_axum_features(project: &Path, features: &[&str]) {
        let manifest_path = project.join("edgezero.toml");
        let source = fs::read_to_string(&manifest_path).expect("generated manifest");
        let mut manifest: TomlValue = toml::from_str(&source).expect("parse generated manifest");
        let axum_build = manifest
            .as_table_mut()
            .and_then(|root| root.get_mut("adapters"))
            .and_then(TomlValue::as_table_mut)
            .and_then(|adapters| adapters.get_mut("axum"))
            .and_then(TomlValue::as_table_mut)
            .and_then(|axum| axum.get_mut("build"))
            .and_then(TomlValue::as_table_mut)
            .expect("generated Axum build settings");
        axum_build.insert("features".to_owned(), toml_strings(features));
        fs::write(
            &manifest_path,
            toml::to_string(&manifest).expect("serialize manifest"),
        )
        .expect("write selected Axum build features");
    }

    fn prepare_typed_generated_app(project: &Path) {
        let core = project.join("crates/native-probe-core/src");
        fs::write(
            core.join("config.rs"),
            concat!(
                "use serde::Deserialize;\n",
                "use validator::Validate;\n\n",
                "#[derive(Debug, Deserialize, Validate, edgezero_core::AppConfig)]\n",
                "#[serde(deny_unknown_fields)]\n",
                "pub struct NativeProbeConfig {\n",
                "    #[validate(length(min = 4_u64))]\n",
                "    pub greeting: String,\n",
                "    #[secret]\n",
                "    #[validate(length(min = 8_u64))]\n",
                "    pub api_token: String,\n",
                "}\n\n",
                "#[derive(Clone)]\n",
                "pub struct StartupState {\n",
                "    pub greeting: String,\n",
                "    pub api_token: String,\n",
                "}\n",
            ),
        )
        .expect("write typed app schema");

        let handlers_path = core.join("handlers.rs");
        let mut handlers = fs::read_to_string(&handlers_path).expect("generated handlers");
        handlers = handlers.replace(
            "use edgezero_core::extractor::{Headers, Json, Path};",
            "use edgezero_core::extractor::{Headers, Json, Path, State};\nuse std::sync::Arc;",
        );
        let start = handlers
            .find("#[action]\npub async fn root()")
            .expect("generated root handler");
        let handler_tail = handlers.get(start..).expect("root handler UTF-8 boundary");
        let relative_end = handler_tail
            .find("\n}\n")
            .expect("generated root handler end");
        let handler_end = start
            .checked_add(relative_end)
            .expect("generated root handler end offset");
        let close = handler_end
            .checked_add(3)
            .expect("generated root handler closing offset");
        handlers.replace_range(
            start..close,
            r#"#[action]
    pub async fn root(State(state): State<Arc<crate::config::StartupState>>) -> Text<String> {
        Text::new(format!("{}|{}", state.greeting, state.api_token))
    }
    "#,
        );
        fs::write(handlers_path, handlers).expect("wire typed response state");
        fs::write(
            project.join("native-probe.toml"),
            "greeting = \"local-typed-config\"\napi_token = \"signing_key\"\n",
        )
        .expect("write local typed validation fixture");
    }

    fn install_startup_initializer(project: &Path) {
        fs::write(
            project.join("crates/native-probe-adapter-axum/src/main.rs"),
            r#"use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use edgezero_adapter_axum::dev_server::run_production_app_with_initializer;
    use edgezero_core::extractor::AppConfig;
    use edgezero_core::EdgeError;
    use native_probe_core::config::{NativeProbeConfig, StartupState};

    fn main() -> anyhow::Result<()> {
        run_production_app_with_initializer::<native_probe_core::App, _>(|app, stores| {
            Box::pin(async move {
                let binding = stores
                    .config()
                    .and_then(|registry| registry.named_ref("app_config"))
                    .ok_or_else(|| EdgeError::service_unavailable("config binding missing"))?;
                let config = AppConfig::<NativeProbeConfig>::from_binding(
                    binding,
                    None,
                    stores.secrets(),
                    app.config_extraction_limits(),
                    app.monotonic_clock(),
                )
                .await?;
                if let (Ok(entered), Ok(release)) = (
                    std::env::var("NATIVE_PROBE_INITIALIZER_ENTERED"),
                    std::env::var("NATIVE_PROBE_INITIALIZER_RELEASE"),
                ) {
                    fs::write(entered, b"typed config is valid")
                        .map_err(EdgeError::internal)?;
                    let release = PathBuf::from(release);
                    let deadline = Instant::now().checked_add(Duration::from_secs(10))
                    .ok_or_else(|| EdgeError::service_unavailable("initializer deadline unavailable"))?;
                    while !release.is_file() {
                        if Instant::now() >= deadline {
                            return Err(EdgeError::internal(anyhow::anyhow!(
                                "test initializer gate timed out"
                            )));
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                app.insert_state(Arc::new(StartupState {
                    greeting: config.greeting,
                    api_token: config.api_token,
                }));
                Ok(())
            })
        })?;
        Ok(())
    }
    "#,
        )
        .expect("install app-owned typed startup check");
    }

    fn bindings_file(path: &Path, agent_endpoint: &str) {
        fs::write(
            path,
            format!(
                r#"version = 1

    [config.app_config]
    provider = "aws-appconfig-agent"
    default_key = "settings"
    endpoint = "{agent_endpoint}"
    access_token_env = "APPCONFIG_AGENT_TOKEN"

    [config.app_config.documents.settings]
    application = "native-probe"
    environment = "test"
    profile = "startup-proof"

    [secrets.signing]
    provider = "aws-secrets-manager"
    region = "us-east-1"

    [secrets.signing.entries.signing_key]
    secret_id = "{SECRET_ARN}"
    "#
            ),
        )
        .expect("write absolute fictional bindings file");
    }

    fn serialized_envelope() -> String {
        serde_json::to_string(&BlobEnvelope::new(
            json!({
                "greeting": "generated-startup-greeting",
                "api_token": "signing_key",
            }),
            "2026-10-07T12:00:00Z".to_owned(),
        ))
        .expect("serialize actual core envelope")
    }

    fn secret_reply() -> HttpReply {
        HttpReply::ok(
            json!({
                "ARN": SECRET_ARN,
                "Name": "native-probe-signing",
                "VersionId": "12345678901234567890123456789012",
                "SecretString": APP_TOKEN,
            })
            .to_string(),
            "application/x-amz-json-1.1",
        )
    }

    fn run_disabled_child(mut command: Command, address: SocketAddr) -> Output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn default-off generated app");
        let deadline = deadline_after(WAIT);
        loop {
            if child
                .try_wait()
                .expect("default-off child status")
                .is_some()
            {
                return child
                    .wait_with_output()
                    .expect("capture disabled-provider error");
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(25)).is_ok() {
                let _killed = child.kill();
                let _output = child.wait_with_output();
                panic!("default-off generated app accepted traffic for an AWS binding");
            }
            if Instant::now() >= deadline {
                let _killed = child.kill();
                let _output = child.wait_with_output();
                panic!("disabled provider did not fail closed");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_no_listener(address: SocketAddr) {
        TcpStream::connect_timeout(&address, Duration::from_millis(60))
            .expect_err("startup must not accept before provider and app checks finish");
    }

    fn reserve_app_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback app port");
        listener.local_addr().expect("app address")
    }

    fn empty_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve local closed endpoint");
        let address = listener.local_addr().expect("closed loopback endpoint");
        drop(listener);
        format!("http://{address}")
    }

    fn profiles(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let home = root.join("empty-home");
        let aws = root.join("empty-aws-profiles");
        fs::create_dir_all(&home).expect("empty HOME");
        fs::create_dir_all(&aws).expect("profile directory");
        let config = aws.join("config");
        let credentials = aws.join("credentials");
        fs::write(&config, "").expect("no AWS config profiles");
        fs::write(&credentials, "").expect("no AWS credential profiles");
        (home, config, credentials)
    }

    fn child_command(
        binary: &Path,
        root: &Path,
        bindings: &Path,
        app_address: SocketAddr,
        secrets_endpoint: &str,
        initializer_gate: Option<(&Path, &Path)>,
    ) -> Command {
        let (home, aws_config, aws_credentials) = profiles(root);
        let mut command = Command::new(binary);
        command
            .env_clear()
            .current_dir(root)
            .env("HOME", home)
            .env("AWS_CONFIG_FILE", aws_config)
            .env("AWS_SHARED_CREDENTIALS_FILE", aws_credentials)
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("AWS_ACCESS_KEY_ID", "DUMMY_GENERATED_ACCESS_KEY")
            .env("AWS_SECRET_ACCESS_KEY", "DUMMY_GENERATED_SECRET_KEY")
            .env("AWS_ENDPOINT_URL", secrets_endpoint)
            .env("AWS_ENDPOINT_URL_SECRETS_MANAGER", secrets_endpoint)
            .env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false")
            .env("EDGEZERO__STORE_BINDINGS_FILE", bindings)
            .env("EDGEZERO__ADAPTER__HOST", "127.0.0.1")
            .env("EDGEZERO__ADAPTER__PORT", app_address.port().to_string())
            .env("EDGEZERO__LOGGING__LEVEL", "off")
            .env("APPCONFIG_AGENT_TOKEN", AGENT_TOKEN)
            .env("signing_key", FALLBACK_SENTINEL);
        if let Some((entered, release)) = initializer_gate {
            command
                .env("NATIVE_PROBE_INITIALIZER_ENTERED", entered)
                .env("NATIVE_PROBE_INITIALIZER_RELEASE", release);
        }
        command
    }

    fn assert_no_sensitive_output(output: &Output) {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for sentinel in [
            APP_TOKEN,
            AGENT_TOKEN,
            FALLBACK_SENTINEL,
            "DUMMY_GENERATED_ACCESS_KEY",
            "DUMMY_GENERATED_SECRET_KEY",
            "AGENT_BODY_SENTINEL",
            "SECRET_BODY_SENTINEL",
        ] {
            assert!(!text.contains(sentinel), "child output leaked {sentinel}");
        }
    }

    fn assert_cargo_command(project: &Path, features: &[&str]) {
        set_axum_features(project, features);
        let output = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .args(["build", "--adapter", "axum", "--", "--offline"])
            .current_dir(project)
            .env("CARGO_TARGET_DIR", target_directory(project))
            .output()
            .expect("run root CLI native build");
        assert!(
            output.status.success(),
            "edgezero build with selected manifest features {features:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn target_directory(project: &Path) -> PathBuf {
        match var_os("EDGEZERO_GENERATED_TEST_TARGET_DIR") {
            Some(target_override) => {
                let target_path = PathBuf::from(target_override);
                if target_path.is_absolute() {
                    target_path
                } else {
                    project.join(target_path)
                }
            }
            None => project.join("target"),
        }
    }

    fn generated_binary(project: &Path) -> PathBuf {
        target_directory(project).join("debug/native-probe-adapter-axum")
    }

    fn wait_for_file(path: &Path) {
        let deadline = deadline_after(WAIT);
        while !path.is_file() {
            assert!(
                Instant::now() < deadline,
                "generated initializer gate timeout"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn http_body(response: &str) -> &str {
        response
            .split_once("\r\n\r\n")
            .expect("HTTP response boundary")
            .1
    }

    fn prepare_workspace(temp: &TempDir) -> PathBuf {
        let scaffold = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .args(["new", "native-probe", "--dir"])
            .arg(temp.path())
            .output()
            .expect("run real edgezero new command");
        assert!(
            scaffold.status.success(),
            "edgezero new failed:\n{}\n{}",
            String::from_utf8_lossy(&scaffold.stdout),
            String::from_utf8_lossy(&scaffold.stderr)
        );
        let project = temp.path().join("native-probe");
        assert!(
            project
                .join("crates/native-probe-adapter-axum/src/main.rs")
                .is_file()
        );
        install_store_declarations(&project);
        prepare_typed_generated_app(&project);
        project
    }

    #[test]
    #[ignore = "runs the real scaffold and five offline generated native builds; coordinator-owned Cargo check"]
    #[expect(
        clippy::too_many_lines,
        reason = "Keep this generated CLI lifecycle as one readable end-to-end proof."
    )]
    fn generated_cli_build_features_and_native_aws_startup_are_proven_together() {
        let temp = tempfile::tempdir().expect("generated project root");
        let project = prepare_workspace(&temp);
        let bindings = temp.path().join("native-stores.toml");
        let no_service_endpoint = empty_endpoint();
        bindings_file(&bindings, &no_service_endpoint);

        // Default generated Cargo features stay empty. With an explicit AWS binding selected,
        // run_generated_app must use strict startup and reject the disabled provider. Port 1 is
        // just a closed loopback port, so a broken feature guard still cannot reach AWS.
        assert_cargo_command(&project, &[]);
        let disabled_address = reserve_app_address();
        let disabled = run_disabled_child(
            child_command(
                &generated_binary(&project),
                temp.path(),
                &bindings,
                disabled_address,
                &no_service_endpoint,
                None,
            ),
            disabled_address,
        );
        assert!(
            !disabled.status.success(),
            "disabled provider must fail startup"
        );
        assert_no_listener(disabled_address);
        let disabled_stderr = String::from_utf8_lossy(&disabled.stderr);
        assert!(
            disabled_stderr.contains("provider not compiled"),
            "disabled provider must report a closed capability error: {disabled_stderr}"
        );
        assert_no_sensitive_output(&disabled);

        // The build command under test reads `[adapters.axum.build].features`, not features on
        // this CLI binary or arbitrary dependency unification in the root workspace.
        assert_cargo_command(&project, &["aws-appconfig-agent"]);
        assert_cargo_command(&project, &["aws-secrets-manager"]);
        assert_cargo_command(&project, &["aws-appconfig-agent", "aws-secrets-manager"]);

        // This app-owned check is deliberately in the generated consumer. The generated default
        // main remains run_generated_app; replace it only for the real typed-startup scenario.
        install_startup_initializer(&project);
        assert_cargo_command(&project, &["aws-appconfig-agent", "aws-secrets-manager"]);

        let agent = GatedService::bind();
        let secrets = GatedService::bind();
        let document = serialized_envelope();
        let agent_endpoint = agent.endpoint().to_owned();
        let secrets_endpoint = secrets.endpoint().to_owned();
        bindings_file(&bindings, &agent_endpoint);
        let app_address = reserve_app_address();
        let initializer_entered = temp.path().join("initializer-entered");
        let initializer_release = temp.path().join("initializer-release");
        let mut host = RunningHost::spawn(
            child_command(
                &generated_binary(&project),
                temp.path(),
                &bindings,
                app_address,
                &secrets_endpoint,
                Some((&initializer_entered, &initializer_release)),
            ),
            app_address,
        );

        // Hold each provider response. Neither listener may accept while provider setup or the
        // typed app initializer is still pending.
        let agent_request = agent.request();
        assert!(agent_request.headers.starts_with(
            "get /applications/native-probe/environments/test/configurations/startup-proof "
        ));
        assert!(
            agent_request
                .headers
                .contains(&format!("authorization: bearer {AGENT_TOKEN}"))
        );
        assert!(agent_request.headers.contains("accept-encoding: identity"));
        assert_no_listener(app_address);
        agent.reply(HttpReply::ok(document, "application/json"));

        let secret_request = secrets.request();
        assert!(
            secret_request
                .headers
                .contains("x-amz-target: secretsmanager.getsecretvalue")
        );
        assert!(
            secret_request
                .headers
                .contains("credential=dummy_generated_access_key/")
        );
        let secret_input: Value =
            serde_json::from_str(&secret_request.body).expect("actual SDK request JSON");
        assert_eq!(secret_input["SecretId"], SECRET_ARN);
        assert!(secret_input.get("VersionStage").is_none());
        assert!(secret_input.get("VersionId").is_none());
        assert_no_listener(app_address);
        secrets.reply(secret_reply());

        // The generated initializer has now parsed and validated the actual envelope and resolved
        // the nested signing_key value. It pauses before returning; the generated app must still
        // keep its listener closed until the application-owned check completes.
        wait_for_file(&initializer_entered);
        assert_no_listener(app_address);
        fs::write(&initializer_release, "continue").expect("release generated initializer");

        // AppConfig::from_binding validated the envelope, resolved `api_token = "signing_key"`
        // from the AWS secret snapshot, and inserted the typed state before the listener appeared.
        let first = host.http("/");
        assert_eq!(
            http_body(&first),
            "generated-startup-greeting|generated-proof-secret-value"
        );
        let second = host.http("/");
        assert_eq!(
            http_body(&second),
            "generated-startup-greeting|generated-proof-secret-value"
        );

        // Both loopback providers received one startup request. Request-time reads came entirely
        // from the process snapshots; an accidental remote read would be captured as an extra.
        assert!(
            agent.finish().is_empty(),
            "no Agent request after preparation"
        );
        assert!(
            secrets.finish().is_empty(),
            "no Secrets Manager request after preparation"
        );
        let output = host.stop();
        assert_no_sensitive_output(&output);
    }
}
