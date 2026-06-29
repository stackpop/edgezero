//! External-consumer integration test.
//!
//! Exercises the `edgezero_cli` public API exactly as a downstream
//! binary would — proving the library surface (`args::BuildArgs`,
//! `run_build`) is usable from outside the crate.
//!
//! This module deliberately contains exactly one `#[test]`: it mutates
//! the process-global `EDGEZERO_MANIFEST` env var, and a single test
//! means no in-binary parallelism on it. If a second env-touching test
//! is ever added here, gate both with a shared `Mutex` guard.

#[cfg(test)]
mod tests {
    use edgezero_cli::args::BuildArgs;
    use edgezero_cli::run_build;
    use std::env;
    use std::fs;
    use tempfile::TempDir;

    const BASIC_MANIFEST: &str = r#"
[app]
name = "consumer-app"
entry = "crates/consumer-core"

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
"#;

    /// RAII guard that restores `EDGEZERO_MANIFEST` to its prior value on drop.
    struct EnvOverride {
        original: Option<String>,
    }

    impl Drop for EnvOverride {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => env::set_var("EDGEZERO_MANIFEST", value),
                None => env::remove_var("EDGEZERO_MANIFEST"),
            }
        }
    }

    impl EnvOverride {
        fn set(value: &str) -> Self {
            let original = env::var("EDGEZERO_MANIFEST").ok();
            env::set_var("EDGEZERO_MANIFEST", value);
            Self { original }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn external_consumer_can_call_run_build() {
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let _env = EnvOverride::set(&manifest_path.to_string_lossy());

        // Construct via `Default` + field mutation — the path that works for
        // an external crate even though `BuildArgs` is `#[non_exhaustive]`.
        let mut args = BuildArgs::default();
        args.adapter = "fastly".to_owned();

        run_build(&args).expect("external consumer can run_build");
    }
}
