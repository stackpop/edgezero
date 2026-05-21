//! Opt-in integration test: a freshly scaffolded project compiles.
//!
//! Ignored by default — it runs `cargo check` on a generated workspace,
//! which recompiles the edgezero stack and may fetch crates (minutes, not
//! milliseconds). The fast `generator` unit tests assert that the scaffold
//! resolves edgezero crates to local path dependencies; this test
//! additionally proves the generated workspace — including the CLI crate
//! that imports `edgezero_cli` — compiles end to end.
//!
//! Run it explicitly (and in CI):
//!
//! ```sh
//! cargo test -p edgezero-cli --test generated_project_builds -- --ignored
//! ```

#[cfg(test)]
mod tests {
    use std::process::Command;

    #[test]
    #[ignore = "compiles a generated workspace and may fetch crates; run explicitly"]
    fn generated_workspace_compiles() {
        let temp = tempfile::tempdir().expect("temp dir");
        let new_status = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .arg("new")
            .arg("scaffold-probe")
            .arg("--dir")
            .arg(temp.path())
            .status()
            .expect("run `edgezero new`");
        assert!(new_status.success(), "`edgezero new` should succeed");

        let project = temp.path().join("scaffold-probe");
        let check_status = Command::new(env!("CARGO"))
            .args(["check", "--workspace"])
            .current_dir(&project)
            .status()
            .expect("run `cargo check` on the generated workspace");
        assert!(
            check_status.success(),
            "generated workspace should compile against the local edgezero crates",
        );
    }
}
