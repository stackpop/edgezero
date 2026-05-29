//! Opt-in integration test: a freshly scaffolded project compiles.
//!
//! Ignored by default — it runs `cargo check` on a generated workspace
//! (host plus each adapter's wasm target), which recompiles the edgezero
//! stack and may fetch crates (minutes, not milliseconds). The fast
//! `generator` unit tests assert that the scaffold resolves edgezero crates
//! to local path dependencies; this test additionally proves the generated
//! workspace — the CLI crate that imports `edgezero_cli`, and the
//! target-gated adapter entrypoints — compiles end to end.
//!
//! Run it explicitly (and in CI):
//!
//! ```sh
//! cargo test -p edgezero-cli --test generated_project_builds -- --ignored
//! ```

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    /// Targets installed for the toolchain that builds `project`. A wasm
    /// check is skipped when its target is absent (e.g. a local run where
    /// the project sits outside a checkout that pins the wasm targets); CI
    /// installs both wasm targets, so the full set always runs there.
    fn installed_targets(project: &Path) -> String {
        Command::new("rustup")
            .args(["target", "list", "--installed"])
            .current_dir(project)
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default()
    }

    #[test]
    #[ignore = "compiles a generated workspace and may fetch crates; run explicitly"]
    #[expect(
        clippy::print_stderr,
        reason = "an opt-in test surfacing a skipped wasm check"
    )]
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

        // The scaffold's `edgezero.toml` + `<name>.toml` + AppConfig
        // must be internally consistent (no `#[secret]` field
        // without a matching `[stores.secrets]`, no env-overlay
        // mismatches). `edgezero config validate` exercises the
        // typed config validator end-to-end. We do this BEFORE
        // `cargo check` so a manifest/config drift surfaces as a
        // fast, clear error -- not as a compilation cascade from
        // a downstream macro tripping over the bad config.
        let validate = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .args(["config", "validate"])
            .current_dir(&project)
            .status()
            .expect("run `edgezero config validate` on the generated workspace");
        assert!(
            validate.success(),
            "generated workspace should pass `edgezero config validate`",
        );

        // Also exercise --strict so the capability matrix
        // (`strict_capability_completeness`) and the handler-path
        // rule (`strict_handler_paths`) fire against a freshly
        // generated project. A scaffold that emits a triggers list
        // with a malformed handler or a manifest that violates the
        // adapter capability matrix would silently pass plain
        // validate but fail under strict.
        let validate_strict = Command::new(env!("CARGO_BIN_EXE_edgezero"))
            .args(["config", "validate", "--strict"])
            .current_dir(&project)
            .status()
            .expect("run `edgezero config validate --strict` on the generated workspace");
        assert!(
            validate_strict.success(),
            "generated workspace should pass `edgezero config validate --strict`",
        );

        // Host target: the whole workspace, including the generated CLI
        // crate that imports `edgezero_cli`.
        let host = Command::new(env!("CARGO"))
            .args(["check", "--workspace"])
            .current_dir(&project)
            .status()
            .expect("run `cargo check` on the generated workspace");
        assert!(
            host.success(),
            "generated workspace should compile for the host target",
        );

        // Per-adapter wasm targets: where target-gated template code lives
        // (entrypoint signatures, macro-generated unsafe exports).
        let targets = installed_targets(&project);
        for (adapter, target) in [
            ("cloudflare", "wasm32-unknown-unknown"),
            ("fastly", "wasm32-wasip1"),
            ("spin", "wasm32-wasip1"),
        ] {
            if !targets.contains(target) {
                eprintln!("skipping {adapter} wasm check: target {target} not installed");
                continue;
            }
            let crate_name = format!("scaffold-probe-adapter-{adapter}");
            let wasm = Command::new(env!("CARGO"))
                .args([
                    "check",
                    "-p",
                    &crate_name,
                    "--target",
                    target,
                    "--features",
                    adapter,
                ])
                .current_dir(&project)
                .status()
                .expect("run `cargo check` for a wasm adapter target");
            assert!(
                wasm.success(),
                "generated {adapter} adapter should compile for {target}",
            );
        }
    }
}
