//! Real local processes, public entrypoints and ordinary main termination output.
#![cfg(all(feature = "axum", feature = "test-utils", unix))]

#[cfg(test)]
#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "subprocess fixtures group setup, process ownership and acceptance scenarios"
)]
mod tests {
    use std::fs;
    use std::io::{self, BufRead as _, BufReader, Read, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
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
