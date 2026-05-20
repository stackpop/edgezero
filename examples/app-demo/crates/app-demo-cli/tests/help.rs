//! Smoke test: the `app-demo-cli` binary parses its CLI without panicking
//! and `--help` lists every built-in command.

#[cfg(test)]
mod tests {
    use std::process::Command;

    #[test]
    fn help_lists_all_builtin_commands() {
        let output = Command::new(env!("CARGO_BIN_EXE_app-demo-cli"))
            .arg("--help")
            .output()
            .expect("run app-demo-cli --help");

        assert!(
            output.status.success(),
            "`app-demo-cli --help` should exit 0"
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        for command in ["build", "deploy", "demo", "new", "serve"] {
            assert!(
                stdout.contains(command),
                "`--help` output should list the `{command}` command"
            );
        }
    }
}
