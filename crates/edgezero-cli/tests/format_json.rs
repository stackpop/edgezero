//! End-to-end `--format` stream contract, against the real `edgezero` binary.
//!
//! Under `--format json`, stdout must be exactly one JSON envelope, even when a
//! child process (here: a manifest shell command, or a fake `curl`) writes to
//! its inherited stdout. That output, and every log line, must land on stderr.
//! `--format text` (the default) must keep today's bytes on stdout.
//!
//! Hermetic: manifest commands are `echo` / `false`, and the Fastly commands
//! talk to a fake `curl` on `PATH` (health probes and the versions API). No
//! network, no platform credentials.

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

    /// A Fastly app whose production deploy is a manifest command, so `deploy`
    /// runs it through the capturing (tee) path.
    const FASTLY_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"

[adapters.fastly.commands]
deploy = "echo 'SUCCESS: Deployed package (service SVC1, version 12)'"
"#;

    /// A Fastly app with no config stores, so a staged deploy needs only
    /// `fastly compute update` to answer (no runtime-env relink).
    const FASTLY_STAGING_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "fastly.toml"
"#;

    /// A Fastly app with one config store, for `config gc`.
    const FASTLY_GC_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "fastly.toml"

[stores.config]
ids = ["app_config"]
"#;

    /// An app whose axum session probe succeeds, and a cloudflare adapter
    /// with no override (its probe is the native `wrangler whoami`).
    const AUTH_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"

[adapters.axum.commands]
auth-status = "echo logged-in"

[adapters.cloudflare.adapter]
crate = "crates/demo-cloudflare"
"#;

    /// The versions API answer: 6 inactive, 7 active, 8 staged.
    const VERSIONS: &str = r#"[{"number":6,"active":false,"locked":true,"staging":false,"deployed":true,"environments":[]},{"number":7,"active":true,"locked":true,"staging":false,"deployed":true,"environments":[]},{"number":8,"active":false,"locked":true,"staging":false,"deployed":false,"environments":[{"active_version":8,"name":"staging","service_id":"SVC1"}]}]"#;

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

    const ACTIVE_VERSION: [&str; 5] = [
        "active-version",
        "--adapter",
        "fastly",
        "--service-id",
        "SVC1",
    ];

    const ROLLBACK: [&str; 9] = [
        "rollback",
        "--adapter",
        "fastly",
        "--service-id",
        "SVC1",
        "--version",
        "7",
        "--rollback-to",
        "6",
    ];

    const DEPLOY: [&str; 5] = ["deploy", "--adapter", "fastly", "--service-id", "SVC1"];

    /// A temp project holding [`MANIFEST`] as `edgezero.toml`.
    fn project() -> TempDir {
        project_with(MANIFEST)
    }

    /// A temp project holding `manifest` as `edgezero.toml`.
    fn project_with(manifest: &str) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        fs::write(dir.path().join("edgezero.toml"), manifest).expect("write manifest");
        dir
    }

    /// `edgezero <args>` in `dir`, with every variable that could change a
    /// result removed: the Fastly credentials, and the `EDGEZERO__*` and
    /// `DEMO_APP__*` overlays that `provision`, `config validate` and
    /// `config gc` read.
    fn command(dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_edgezero"));
        command
            .args(args)
            .current_dir(dir)
            .env("EDGEZERO_MANIFEST", dir.join("edgezero.toml"))
            .env_remove("FASTLY_API_TOKEN")
            .env_remove("FASTLY_SERVICE_ID");
        for (key, _) in env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("EDGEZERO__") || name.starts_with("DEMO_APP__") {
                command.env_remove(&key);
            }
        }
        command
    }

    /// Put `bin_dir` first on the command's `PATH`.
    fn prepend_path(command: &mut Command, bin_dir: &Path) {
        let path = env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin_dir.to_path_buf()];
        paths.extend(env::split_paths(&path));
        command.env("PATH", env::join_paths(paths).expect("join PATH"));
    }

    /// Run `edgezero <args>` in `dir`, optionally with `bin_dir` first on `PATH`.
    fn edgezero(dir: &Path, args: &[&str], bin_dir: Option<&Path>) -> Output {
        edgezero_with_token(dir, args, bin_dir, None)
    }

    /// [`edgezero`], with `FASTLY_API_TOKEN` set to `token` (removed when `None`).
    fn edgezero_with_token(
        dir: &Path,
        args: &[&str],
        bin_dir: Option<&Path>,
        token: Option<&str>,
    ) -> Output {
        let mut command = command(dir, args);
        if let Some(value) = token {
            command.env("FASTLY_API_TOKEN", value);
        }
        if let Some(bin) = bin_dir {
            prepend_path(&mut command, bin);
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
        fake_curl_script(&format!("echo {code}"))
    }

    /// A fake `curl` that also serves the Fastly API: a request read from
    /// `--config -` gets [`VERSIONS`] (GET) or a bare 200 (PUT), and is logged
    /// to `api.log` as `GET` / `PUT`. A health probe gets HTTP `code`.
    fn fake_curl_api(code: u16) -> TempDir {
        fake_curl_script(&format!(
            "case \" $* \" in *\" --config \"*)\n\
             \x20 cfg=$(cat)\n\
             \x20 case \"$cfg\" in *'request = \"PUT\"'*) echo PUT >> \"$(dirname \"$0\")/api.log\"; printf 200;;\n\
             \x20 *) echo GET >> \"$(dirname \"$0\")/api.log\"; printf '%s\\n200' '{VERSIONS}';; esac\n\
             \x20 exit 0;;\n\
             esac\n\
             echo {code}"
        ))
    }

    fn fake_curl_script(body: &str) -> TempDir {
        fake_binary("curl", body)
    }

    /// A temp dir holding an executable `name` that runs the shell `body`.
    fn fake_binary(name: &str, body: &str) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        let script = dir.path().join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write fake binary");
        let mut perms = fs::metadata(&script).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod +x");
        dir
    }

    /// A fake `fastly` for `config gc`: one config store (`app_config`, id
    /// `store-1`) holding only a plain `greeting` entry, so there is nothing to
    /// reclaim. Any `delete` is logged to `deletes.log` next to the script.
    fn fake_fastly_gc() -> TempDir {
        fake_binary(
            "fastly",
            "case \"$1 $2\" in\n\
             \x20 'config-store list') echo '[{\"name\":\"app_config\",\"id\":\"store-1\"}]';;\n\
             \x20 'config-store-entry list') echo '[{\"item_key\":\"greeting\",\"item_value\":\"hello\",\"created_at\":\"2026-07-01T00:00:00Z\"}]';;\n\
             \x20 'config-store-entry delete') echo \"$*\" >> \"$(dirname \"$0\")/deletes.log\";;\n\
             esac",
        )
    }

    /// A fake `fastly` whose `compute update` makes draft version 8.
    fn fake_fastly_staging() -> TempDir {
        fake_binary(
            "fastly",
            "if [ \"$1 $2\" = 'compute update' ]; then \
             echo 'SUCCESS: Updated package (service SVC1, version 8)'; fi",
        )
    }

    /// A project holding `manifest` and an empty `fastly.toml`.
    fn fastly_project(manifest: &str) -> TempDir {
        let dir = project_with(manifest);
        fs::write(dir.path().join("fastly.toml"), "name = \"app\"\n").expect("fastly.toml");
        dir
    }

    /// The text stdout of a manifest command: the `[edgezero] executing …`
    /// line, then what the command printed (`child_output`).
    fn executed(command: &str, adapter: &str, dir: &Path, child_output: &str) -> String {
        format!(
            "[edgezero] executing `{command}` for adapter `{adapter}` in {}\n{child_output}",
            dir.display()
        )
    }

    /// The Fastly API calls a [`fake_curl_api`] served, in order.
    fn api_calls(curl: &TempDir) -> Vec<String> {
        fs::read_to_string(curl.path().join("api.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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
        assert_eq!(
            stdout_of(&output),
            executed(
                "echo child-wrote-to-stdout",
                "axum",
                dir.path(),
                "child-wrote-to-stdout\n"
            )
        );
        assert!(output.stderr.is_empty(), "stderr: {}", stderr_of(&output));
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
                "app_config": "demo-app.toml", "app_name": "demo-app", "manifest": "edgezero.toml",
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

    #[test]
    fn json_build_rejects_a_format_after_passthrough_args() {
        let dir = project();
        let output = edgezero(
            dir.path(),
            &[
                "build",
                "--adapter",
                "axum",
                "--release",
                "--format",
                "json",
            ],
            None,
        );
        assert_eq!(output.status.code(), Some(1_i32));
        // The late `--format` was never read, so this is a text-mode failure:
        // nothing on stdout, and the build command never ran.
        assert!(output.stdout.is_empty(), "stdout: {:?}", stdout_of(&output));
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("`--format` after a passthrough argument"),
            "stderr: {stderr}"
        );
        assert!(
            !stderr.contains("child-wrote-to-stdout"),
            "stderr: {stderr}"
        );
    }

    #[test]
    fn build_rejects_every_passthrough_format_spelling() {
        let dir = project();
        for args in [
            &["build", "--adapter", "axum", "--release", "--format=json"][..],
            // clap drops a leading `--`, so this cannot be told apart from a
            // misplaced flag: `build` never forwards `--format`.
            &["build", "--adapter", "axum", "--", "--format", "json"][..],
        ] {
            let output = edgezero(dir.path(), args, None);
            assert_eq!(output.status.code(), Some(1_i32), "{args:?}");
            assert!(
                output.stdout.is_empty(),
                "{args:?}: {:?}",
                stdout_of(&output)
            );
            assert!(
                stderr_of(&output).contains("after a passthrough argument"),
                "{args:?}: {}",
                stderr_of(&output)
            );
        }
    }

    #[test]
    fn json_config_validate_failure_has_no_result() {
        // No `demo-app.toml`: the raw validator requires it.
        let dir = project();
        let output = edgezero(
            dir.path(),
            &["config", "validate", "--format", "json"],
            None,
        );
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert!(envelope["result"].is_null(), "{envelope}");
        let message = envelope["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.contains("demo-app.toml"), "{message}");
    }

    #[test]
    fn json_healthcheck_with_a_token_verifies_the_version() {
        let dir = project();
        let curl = fake_curl_api(200);
        let mut args = HEALTHCHECK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("tok"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["result"]["healthy"], json!(true));
        assert_eq!(envelope["result"]["version_verified"], json!(true));
        assert_eq!(
            api_calls(&curl),
            ["GET", "GET"],
            "before and after the probe"
        );
    }

    #[test]
    fn json_unhealthy_healthcheck_with_a_token_is_not_verified() {
        let dir = project();
        let curl = fake_curl_api(503);
        let mut args = HEALTHCHECK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("tok"));
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["result"]["healthy"], json!(false));
        assert_eq!(
            envelope["result"]["version_verified"],
            json!(false),
            "the after-probe check never ran"
        );
        assert_eq!(api_calls(&curl), ["GET"]);
    }

    #[test]
    fn active_version_text_and_json() {
        let dir = project();
        let curl = fake_curl_api(200);
        let text = edgezero_with_token(dir.path(), &ACTIVE_VERSION, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(stdout_of(&text), "version=7\n");

        let mut args = ACTIVE_VERSION.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "version": 7_i32})
        );
        assert!(stderr_of(&output).contains("version=7\n"));
    }

    #[test]
    fn active_version_with_none_active_reports_null() {
        let dir = project();
        // Overwrite the API answer: no version is active yet.
        let curl = fake_curl_script(
            "case \" $* \" in *\" --config \"*) cat >/dev/null; \
             printf '%s\\n200' '[{\"number\":1,\"active\":false,\"locked\":false,\"staging\":false,\"deployed\":false,\"environments\":[]}]'; exit 0;; esac",
        );
        let text = edgezero_with_token(dir.path(), &ACTIVE_VERSION, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(
            stdout_of(&text),
            "version=\nservice SVC1 has no active version yet; emitting an empty rollback target\n"
        );

        let mut args = ACTIVE_VERSION.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"]["version"],
            json!(null)
        );
    }

    #[test]
    fn rollback_text_and_json() {
        let dir = project();
        let curl = fake_curl_api(200);
        let text = edgezero_with_token(dir.path(), &ROLLBACK, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(stdout_of(&text), "rolled-back-to=6\n");

        let mut args = ROLLBACK.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({
                "adapter": "fastly", "rolled_back_to": 6_i32, "service_id": "SVC1",
                "staging": false, "version": 7_i32
            })
        );
        assert!(stderr_of(&output).contains("rolled-back-to=6\n"));
    }

    #[test]
    fn deploy_reads_the_version_from_the_captured_output() {
        let dir = project_with(FASTLY_MANIFEST);
        // The version parsed from the output is verified active via the API.
        let curl = fake_curl_script(
            "case \" $* \" in *\" --config \"*) cat >/dev/null; \
             printf '%s\\n200' '[{\"number\":12,\"active\":true,\"locked\":true,\"staging\":false,\"deployed\":true,\"environments\":[]}]'; exit 0;; esac",
        );
        let text = edgezero_with_token(dir.path(), &DEPLOY, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        // The manifest command receives `--service-id SVC1` as passthrough. The
        // tee echoes the child live, then the CLI prints the version line.
        assert_eq!(
            stdout_of(&text),
            executed(
                "echo 'SUCCESS: Deployed package (service SVC1, version 12)'",
                "fastly",
                dir.path(),
                "SUCCESS: Deployed package (service SVC1, version 12) --service-id SVC1\n\
                 version=12\n"
            )
        );

        let mut args = DEPLOY.to_vec();
        args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "staging": false, "version": 12_i32})
        );
        // The tee sends the captured child's stdout to stderr under JSON.
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("SUCCESS: Deployed package (service SVC1, version 12) --service-id"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("version=12\n"), "stderr: {stderr}");
    }

    #[test]
    fn deploy_that_went_live_without_a_version_keeps_its_result() {
        let manifest = FASTLY_MANIFEST.replace(
            "echo 'SUCCESS: Deployed package (service SVC1, version 12)'",
            "echo deployed",
        );
        let dir = project_with(&manifest);
        let mut args = DEPLOY.to_vec();
        args.extend(["--format", "json"]);
        // No token: the API fallback cannot resolve the version either.
        let output = edgezero(dir.path(), &args, None);
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(
            envelope["result"],
            json!({"adapter": "fastly", "service_id": "SVC1", "staging": false, "version": null}),
            "the deploy went live, so the result is present"
        );
        let message = envelope["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.starts_with("deploy succeeded but"), "{message}");
    }

    #[test]
    fn auth_status_text_failure_is_unchanged() {
        let dir = project();
        let output = edgezero(dir.path(), &["auth", "status", "--adapter", "axum"], None);
        assert_eq!(output.status.code(), Some(1_i32));
        assert_eq!(
            stdout_of(&output),
            executed(
                "echo child-wrote-to-stdout; false",
                "axum",
                dir.path(),
                "child-wrote-to-stdout\n"
            )
        );
        assert_eq!(
            stderr_of(&output),
            "[edgezero] auth status command `echo child-wrote-to-stdout; false` exited with \
             status exit status: 1\n"
        );
    }

    #[test]
    fn auth_status_text_and_json_when_authenticated() {
        let dir = project_with(AUTH_MANIFEST);
        let text = edgezero(dir.path(), &["auth", "status", "--adapter", "axum"], None);
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(
            stdout_of(&text),
            executed("echo logged-in", "axum", dir.path(), "logged-in\n")
        );

        let output = edgezero(
            dir.path(),
            &["auth", "status", "--adapter", "axum", "--format", "json"],
            None,
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output),
            json!({
                "command": "auth status",
                "error": null,
                "ok": true,
                "result": {"adapter": "axum", "state": "authenticated"},
                "schema_version": 1_i32
            })
        );
        assert!(stderr_of(&output).contains("logged-in\n"));
    }

    #[test]
    fn auth_status_without_the_native_cli_has_no_result() {
        let dir = project_with(AUTH_MANIFEST);
        let empty_path = TempDir::new().expect("temp dir");
        let output = command(
            dir.path(),
            &[
                "auth",
                "status",
                "--adapter",
                "cloudflare",
                "--format",
                "json",
            ],
        )
        .env("PATH", empty_path.path())
        .output()
        .expect("run edgezero");
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["ok"], json!(false));
        assert!(envelope["result"].is_null(), "{envelope}");
        let message = envelope["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(
            message.contains("`wrangler` not found on PATH"),
            "{message}"
        );
    }

    #[test]
    fn provision_text_is_unchanged() {
        let dir = project();
        let output = edgezero(dir.path(), &["provision", "--adapter", "axum"], None);
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            stdout_of(&output),
            "axum KV store `sessions` is in-memory; nothing to provision\n"
        );
        assert!(output.stderr.is_empty(), "stderr: {}", stderr_of(&output));
    }

    #[test]
    fn config_validate_text_is_unchanged() {
        let dir = project();
        // No `demo-app.toml` yet: the failure goes to stderr only.
        let failed = edgezero(dir.path(), &["config", "validate"], None);
        assert_eq!(failed.status.code(), Some(1_i32));
        assert!(failed.stdout.is_empty(), "stdout: {:?}", stdout_of(&failed));
        assert_eq!(
            stderr_of(&failed),
            "[edgezero] failed to read demo-app.toml: No such file or directory (os error 2)\n"
        );

        fs::write(dir.path().join("demo-app.toml"), "").expect("write app config");
        let output = edgezero(dir.path(), &["config", "validate"], None);
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            stdout_of(&output),
            "[edgezero] config validate (raw): edgezero.toml OK\n"
        );
    }

    /// The `config gc` report lines for the [`fake_fastly_gc`] store, with the
    /// `older_than` window `secs`.
    fn gc_report_lines(secs: u64) -> String {
        format!(
            "[edgezero] fastly config-store `app_config` (id=store-1): 1 entries, 1 root(s), 0 \
             referenced chunk(s), 0 orphan(s) in 0 generation(s) older than {secs}s, 0 orphan(s) \
             too recent\n\
             [edgezero] keeping 1 retained root(s) (0 referenced chunk(s) held by them):\n\
             [edgezero]   keeping `greeting`\n\
             [edgezero] nothing to reclaim\n"
        )
    }

    #[test]
    fn config_gc_dry_run_text_and_json() {
        let dir = fastly_project(FASTLY_GC_MANIFEST);
        let fastly = fake_fastly_gc();
        let text = edgezero(
            dir.path(),
            &["config", "gc", "--adapter", "fastly"],
            Some(fastly.path()),
        );
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        let stdout = stdout_of(&text);
        assert!(
            stdout.starts_with(&gc_report_lines(0)),
            "the report, then the dry-run advisory: {stdout:?}"
        );
        let advisory = stdout.strip_prefix(&gc_report_lines(0)).unwrap_or_default();
        assert!(
            advisory.starts_with("[edgezero] dry-run: previewing ALL orphans")
                && advisory.ends_with("(a bare `--yes` is rejected).\n")
                && advisory.lines().count() == 1,
            "one advisory line: {advisory:?}"
        );

        // No `--yes`: a dry run by default, with no `--older-than` window.
        let output = edgezero(
            dir.path(),
            &["config", "gc", "--adapter", "fastly", "--format", "json"],
            Some(fastly.path()),
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({
                "adapter": "fastly", "deleted": null, "dry_run": true, "failed": [],
                "kept_roots": ["greeting"], "older_than_secs": null, "planned_deletions": [],
                "store": {"id": "store-1", "logical": "app_config", "platform": "app_config"},
                "stranded": [],
                "summary": {
                    "entries": 1_i32, "generations_planned": 0_i32, "orphans_planned": 0_i32,
                    "orphans_too_recent": 0_i32, "referenced_chunks": 0_i32, "roots": 1_i32,
                    "unprovable": 0_i32
                },
                "uncertain": [], "warnings": []
            })
        );
        assert!(stderr_of(&output).contains(&gc_report_lines(0)));
        assert!(
            !fastly.path().join("deletes.log").exists(),
            "nothing deleted"
        );
    }

    #[test]
    fn config_gc_real_run_with_nothing_to_reclaim_reports_zero_deleted() {
        let dir = fastly_project(FASTLY_GC_MANIFEST);
        let fastly = fake_fastly_gc();
        let args = [
            "config",
            "gc",
            "--adapter",
            "fastly",
            "--yes",
            "--older-than",
            "1h",
        ];
        let text = edgezero(dir.path(), &args, Some(fastly.path()));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(stdout_of(&text), gc_report_lines(3600));

        let mut json_args = args.to_vec();
        json_args.extend(["--format", "json"]);
        let output = edgezero(dir.path(), &json_args, Some(fastly.path()));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        let result = &sole_json_document(&output)["result"];
        assert_eq!(result["dry_run"], json!(false));
        assert_eq!(result["deleted"], json!(0_i32), "a real run, not a dry run");
        assert_eq!(result["older_than_secs"], json!(3600_i32));
        assert_eq!(stderr_of(&output), gc_report_lines(3600));
        assert!(
            !fastly.path().join("deletes.log").exists(),
            "nothing deleted"
        );
    }

    #[test]
    fn staged_deploy_without_a_release_fails_with_a_json_envelope() {
        let dir = fastly_project(FASTLY_STAGING_MANIFEST);
        let fastly = fake_fastly_staging();
        // A staged deploy is adapter-managed and needs an immutable release;
        // without one it fails before any provider call, so no result exists.
        let mut command = command(
            dir.path(),
            &[
                "deploy",
                "--adapter",
                "fastly",
                "--staging",
                "--format",
                "json",
            ],
        );
        prepend_path(&mut command, fastly.path());
        let output = command
            .env("FASTLY_API_TOKEN", "t")
            .env("FASTLY_SERVICE_ID", "SVC1")
            .output()
            .expect("run edgezero");
        assert_eq!(output.status.code(), Some(1_i32));
        let envelope = sole_json_document(&output);
        assert_eq!(envelope["command"], json!("deploy"));
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["result"], json!(null));
        let message = envelope["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.contains("--application-release"), "{message}");
    }

    #[test]
    fn staged_rollback_text_and_json() {
        let dir = project();
        let curl = fake_curl_api(200);
        let args = [
            "rollback",
            "--adapter",
            "fastly",
            "--service-id",
            "SVC1",
            "--version",
            "8",
            "--staging",
        ];
        let line = "[edgezero] deactivated staged version 8 on Fastly service SVC1\n";
        let text = edgezero_with_token(dir.path(), &args, Some(curl.path()), Some("t"));
        assert!(text.status.success(), "stderr: {}", stderr_of(&text));
        assert_eq!(stdout_of(&text), line);
        // The staged version is confirmed from the version list before deactivating.
        assert_eq!(api_calls(&curl), ["GET", "PUT"]);

        let mut json_args = args.to_vec();
        json_args.extend(["--format", "json"]);
        let output = edgezero_with_token(dir.path(), &json_args, Some(curl.path()), Some("t"));
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
        assert_eq!(
            sole_json_document(&output)["result"],
            json!({
                "adapter": "fastly", "rolled_back_to": null, "service_id": "SVC1",
                "staging": true, "version": 8_i32
            })
        );
        assert_eq!(stderr_of(&output), line);
    }
}
