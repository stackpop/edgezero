//! End-to-end `--format` stream contract, against the real `edgezero` binary.
//!
//! Under `--format json`, stdout must be exactly one JSON envelope, even when a
//! child process (here: a manifest shell command, or a fake `curl`) writes to
//! its inherited stdout. That output, and every log line, must land on stderr.
//! `--format text` (the default) must keep today's bytes on stdout.
//!
//! Hermetic: manifest commands are `echo` / `false`, and the healthcheck probes
//! a fake `curl` on `PATH`. No network, no platform credentials.

#![cfg(unix)]

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Output};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    const MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"

[adapters.axum.commands]
build = "echo child-wrote-to-stdout"
auth-status = "echo child-wrote-to-stdout; false"

[stores.kv]
ids = ["sessions"]
"#;

    /// `healthcheck` against a fake `curl`: one attempt, no delay.
    const HEALTHCHECK: [&str; 13] = [
        "healthcheck",
        "--adapter",
        "fastly",
        "--domain",
        "app.example.com",
        "--service-id",
        "SVC1",
        "--version",
        "7",
        "--retry",
        "1",
        "--retry-delay",
        "0",
    ];

    /// A temp project holding [`MANIFEST`] as `edgezero.toml`.
    fn project() -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        fs::write(dir.path().join("edgezero.toml"), MANIFEST).expect("write manifest");
        dir
    }

    /// Run `edgezero <args>` in `dir`, optionally with `bin_dir` first on `PATH`.
    fn edgezero(dir: &Path, args: &[&str], bin_dir: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_edgezero"));
        command
            .args(args)
            .current_dir(dir)
            .env("EDGEZERO_MANIFEST", dir.join("edgezero.toml"))
            .env_remove("FASTLY_API_TOKEN");
        if let Some(bin) = bin_dir {
            let path = env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin.to_path_buf()];
            paths.extend(env::split_paths(&path));
            command.env("PATH", env::join_paths(paths).expect("join PATH"));
        }
        command.output().expect("run edgezero")
    }

    fn stdout_of(output: &Output) -> String {
        String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
    }

    fn stderr_of(output: &Output) -> String {
        String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
    }

    /// stdout parses as exactly ONE JSON value (nothing before or after it).
    fn sole_json_document(output: &Output) -> Value {
        let stdout = stdout_of(output);
        let mut stream = serde_json::Deserializer::from_str(&stdout).into_iter::<Value>();
        let document = stream
            .next()
            .expect("stdout holds a JSON document")
            .unwrap_or_else(|err| panic!("stdout is not clean JSON ({err}): {stdout:?}"));
        assert!(
            stream.next().is_none(),
            "stdout holds more than one JSON value: {stdout:?}"
        );
        document
    }

    /// A fake `curl` answering every probe with HTTP `code`.
    fn fake_curl(code: u16) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        let script = dir.path().join("curl");
        fs::write(&script, format!("#!/bin/sh\necho {code}\n")).expect("write curl");
        let mut perms = fs::metadata(&script).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod +x");
        dir
    }

    #[test]
    fn json_build_keeps_child_stdout_off_the_envelope_stream() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["build", "--adapter", "axum", "--format", "json"],
            None,
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let envelope = sole_json_document(&output);
        assert_eq!(
            envelope,
            json!({
                "command": "build",
                "error": null,
                "ok": true,
                "result": {"adapter": "axum", "artifact": null},
                "schema_version": 1_i32
            })
        );
        let stderr = stderr_of(&output);
        assert!(stderr.contains("child-wrote-to-stdout"), "stderr: {stderr}");
        assert!(stderr.contains("[edgezero] executing"), "stderr: {stderr}");
    }

    #[test]
    fn text_build_output_is_unchanged() {
        let dir = project();
        let output = edgezero(dir.path(), &["build", "--adapter", "axum"], None);
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let stdout = stdout_of(&output);
        assert!(
            stdout.starts_with(
                "[edgezero] executing `echo child-wrote-to-stdout` for adapter `axum` in "
            ),
            "stdout: {stdout:?}"
        );
        assert!(
            stdout.ends_with("\nchild-wrote-to-stdout\n"),
            "stdout: {stdout:?}"
        );
    }

    #[test]
    fn json_failure_still_emits_an_envelope_with_the_partial_result() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["auth", "status", "--adapter", "axum", "--format", "json"],
            None,
        );
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["command"], json!("auth status"));
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(
            envelope["result"],
            json!({"adapter": "axum", "state": "unauthenticated"})
        );
        let message = envelope["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.contains("exited with status"), "{message}");
        // The binary still logs the error to stderr, as in text mode.
        assert!(
            stderr_of(&output).contains(message),
            "stderr repeats the error"
        );
    }

    #[test]
    fn json_provision_reports_each_store() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["provision", "--adapter", "axum", "--format", "json"],
            None,
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let envelope = sole_json_document(&output);
        assert_eq!(
            envelope["result"],
            json!({
                "adapter": "axum",
                "dry_run": false,
                "entries": [{
                    "action": "not_applicable",
                    "message": "axum KV store `sessions` is in-memory; nothing to provision",
                    "store": {"kind": "kv", "logical": "sessions", "platform": "sessions"}
                }]
            })
        );
    }

    #[test]
    fn json_config_validate_reports_the_validated_inputs() {
        let dir = project();
        fs::write(dir.path().join("demo-app.toml"), "").expect("write app config");
        let output = edgezero(
            dir.path(),
            &["config", "validate", "--format", "json"],
            None,
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["command"], json!("config validate"));
        assert_eq!(
            envelope["result"],
            json!({
                "app_config": null, "app_name": "demo-app", "manifest": "edgezero.toml",
                "mode": "raw", "strict": false
            })
        );
        assert!(
            stderr_of(&output).contains("config validate (raw): edgezero.toml OK"),
            "the human summary moves to stderr"
        );
    }

    #[test]
    fn healthcheck_text_bytes_match_the_line_contract() {
        let dir = project();
        let curl = fake_curl(200);
        let output = edgezero(dir.path(), &HEALTHCHECK, Some(curl.path()));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            stdout_of(&output),
            "no FASTLY_API_TOKEN available; production healthcheck is service-level (probes the \
             live domain for service SVC1, not specifically version 7)\n\
             status-code=200\n\
             healthy=true\n"
        );
    }

    #[test]
    fn json_unhealthy_healthcheck_fails_with_its_measurements() {
        let dir = project();
        let curl = fake_curl(503);
        let mut args = HEALTHCHECK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero(dir.path(), &args, Some(curl.path()));
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(
            envelope["result"],
            json!({
                "adapter": "fastly", "attempts": 1_i32, "domain": "app.example.com",
                "healthy": false, "path": "/", "service_id": "SVC1", "staging": false,
                "staging_ip": null, "status_code": 503_i32, "version": 7_i32,
                "version_verified": false
            })
        );
        // The line contract still reaches the logs, on stderr.
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("status-code=503\nhealthy=false\n"),
            "stderr: {stderr}"
        );
    }

    #[test]
    fn usage_errors_leave_stdout_empty() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["build", "--adapter", "axum", "--format", "yaml"],
            None,
        );
        assert_eq!(output.status.code(), Some(2_i32));
        assert!(output.stdout.is_empty(), "stdout: {:?}", stdout_of(&output));
    }

    #[test]
    fn bundled_stub_ignores_format_and_leaves_stdout_empty() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["config", "diff", "--adapter", "fastly", "--format", "json"],
            None,
        );
        assert_eq!(output.status.code(), Some(2_i32));
        assert!(output.stdout.is_empty(), "stdout: {:?}", stdout_of(&output));
    }
}
