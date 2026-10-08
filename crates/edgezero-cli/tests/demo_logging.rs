//! Subprocess probes keep application logger installation isolated from the test runner.
#![cfg(all(feature = "cli", feature = "demo-example"))]

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::process::Command;

    fn demo_startup(level: &str, host: &str) -> (String, String) {
        let directory = tempfile::tempdir().expect("isolated demo state");
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve demo port");
        let port = listener.local_addr().expect("reserved address").port();
        let output = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .arg("demo")
            .current_dir(directory.path())
            .env("EDGEZERO__LOGGING__LEVEL", level)
            .env("EDGEZERO__ADAPTER__HOST", host)
            .env("EDGEZERO__ADAPTER__PORT", port.to_string())
            .output()
            .expect("demo subprocess");
        assert!(
            !output.status.success(),
            "reserved port must prevent serving"
        );
        (
            String::from_utf8(output.stdout).expect("demo stdout"),
            String::from_utf8(output.stderr).expect("demo stderr"),
        )
    }

    #[test]
    fn demo_respects_off_without_hiding_terminal_errors() {
        let (stdout, stderr) = demo_startup("off", "not-an-ip");
        assert!(!stdout.contains("starting axum server"), "{stdout}");
        assert!(stdout.contains("[edgezero::boot]"), "{stdout}");
        assert!(stdout.contains("not a valid IP address"), "{stdout}");
        assert!(
            stderr.contains("[edgezero] demo server error: failed to bind"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("initialize application logger"),
            "{stderr}"
        );
    }

    #[test]
    fn demo_uses_application_logger_not_the_cli_backend() {
        let (stdout, stderr) = demo_startup("info", "127.0.0.1");
        assert!(stdout.contains("INFO"), "{stdout}");
        assert!(
            stdout.contains("[edgezero_adapter_axum::dev_server]"),
            "{stdout}"
        );
        assert!(stdout.contains("starting axum server"), "{stdout}");
        assert!(
            stderr.contains("[edgezero] demo server error: failed to bind"),
            "{stderr}"
        );
    }
}
