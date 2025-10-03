//! AnyEdge CLI.

#[cfg(feature = "cli")]
mod adapter;
#[cfg(feature = "cli")]
mod args;
#[cfg(all(feature = "cli", feature = "anyedge-adapter-axum"))]
mod dev_server;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod scaffold;

#[cfg(feature = "cli")]
use anyedge_core::manifest::ManifestLoader;
#[cfg(feature = "cli")]
use std::io::ErrorKind;
#[cfg(feature = "cli")]
use std::path::PathBuf;

#[cfg(feature = "cli")]
fn main() {
    use args::{Args, Command};
    use clap::Parser;

    let args = Args::parse();
    match args.cmd {
        Command::New(new_args) => {
            if let Err(e) = generator::generate_new(new_args) {
                eprintln!("[anyedge] new error: {e}");
                std::process::exit(1);
            }
        }
        Command::Build {
            adapter,
            adapter_args,
        } => {
            if let Err(err) = handle_build(&adapter, &adapter_args) {
                eprintln!("[anyedge] build error: {err}");
                std::process::exit(1);
            }
        }
        Command::Deploy {
            adapter,
            adapter_args,
        } => {
            if let Err(err) = handle_deploy(&adapter, &adapter_args) {
                eprintln!("[anyedge] deploy error: {err}");
                std::process::exit(1);
            }
        }
        Command::Serve { adapter } => {
            if let Err(err) = handle_serve(&adapter) {
                eprintln!("[anyedge] serve error: {err}");
                std::process::exit(1);
            }
        }
        Command::Dev => {
            #[cfg(feature = "anyedge-adapter-axum")]
            {
                dev_server::run_dev();
            }

            #[cfg(not(feature = "anyedge-adapter-axum"))]
            {
                eprintln!(
                    "anyedge-cli built without `anyedge-adapter-axum`; rebuild with that feature to use `anyedge dev`."
                );
                std::process::exit(1);
            }
        }
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("anyedge-cli built without `cli` feature. Rebuild with `--features cli`.");
}

#[cfg(feature = "cli")]
fn handle_build(adapter_name: &str, adapter_args: &[String]) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    adapter::execute(
        adapter_name,
        adapter::Action::Build,
        manifest.as_ref(),
        adapter_args,
    )
}

#[cfg(feature = "cli")]
fn handle_deploy(adapter_name: &str, adapter_args: &[String]) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    adapter::execute(
        adapter_name,
        adapter::Action::Deploy,
        manifest.as_ref(),
        adapter_args,
    )
}

#[cfg(feature = "cli")]
fn handle_serve(adapter_name: &str) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    adapter::execute(
        adapter_name,
        adapter::Action::Serve,
        manifest.as_ref(),
        &[] as &[String],
    )
}

#[cfg(feature = "cli")]
fn ensure_adapter_defined(
    adapter_name: &str,
    manifest: Option<&ManifestLoader>,
) -> Result<(), String> {
    if let Some(manifest) = manifest {
        if manifest.manifest().adapters.contains_key(adapter_name) {
            return Ok(());
        }
        let available: Vec<String> = manifest.manifest().adapters.keys().cloned().collect();
        if available.is_empty() {
            Err(format!(
                "adapter `{}` is not configured in anyedge.toml (no adapters defined)",
                adapter_name
            ))
        } else {
            Err(format!(
                "adapter `{}` is not configured in anyedge.toml (available: {})",
                adapter_name,
                available.join(", ")
            ))
        }
    } else {
        Ok(())
    }
}

#[cfg(feature = "cli")]
fn load_manifest_optional() -> Result<Option<ManifestLoader>, String> {
    let path = std::env::var("ANYEDGE_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("anyedge.toml"));

    match ManifestLoader::from_path(&path) {
        Ok(loader) => Ok(Some(loader)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to load {}: {err}", path.display())),
    }
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use anyedge_core::manifest::ManifestLoader;
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    const BASIC_MANIFEST: &str = r#"
[app]
name = "demo-app"
entry = "crates/demo-core"

[adapters.fastly.adapter]
crate = "crates/demo-fastly"
manifest = "crates/demo-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-unknown-unknown"
profile = "release"

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
"#;

    fn manifest_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    struct EnvOverride {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvOverride {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvOverride {
        fn drop(&mut self) {
            if let Some(ref original) = self.original {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn load_manifest_optional_returns_none_when_missing() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("missing.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("ANYEDGE_MANIFEST", &manifest_str);
        let result = load_manifest_optional().expect("load result");
        assert!(result.is_none());
    }

    #[test]
    fn load_manifest_optional_reads_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("anyedge.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("ANYEDGE_MANIFEST", &manifest_str);
        let manifest = load_manifest_optional()
            .expect("load result")
            .expect("manifest present");
        assert!(manifest.manifest().adapters.contains_key("fastly"));
    }

    #[test]
    fn ensure_adapter_defined_accepts_known_adapter() {
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        assert!(ensure_adapter_defined("fastly", Some(&loader)).is_ok());
    }

    #[test]
    fn ensure_adapter_defined_reports_unknown_adapter() {
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let err = ensure_adapter_defined("cloudflare", Some(&loader)).expect_err("should err");
        assert!(err.contains("available"));
        assert!(err.contains("fastly"));
    }

    #[test]
    fn ensure_adapter_defined_allows_when_manifest_missing() {
        assert!(ensure_adapter_defined("fastly", None).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_build_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("anyedge.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("ANYEDGE_MANIFEST", &manifest_str);
        let args: Vec<String> = Vec::new();
        handle_build("fastly", &args).expect("build command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_deploy_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("anyedge.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("ANYEDGE_MANIFEST", &manifest_str);
        let args: Vec<String> = Vec::new();
        handle_deploy("fastly", &args).expect("deploy command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_serve_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("anyedge.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("ANYEDGE_MANIFEST", &manifest_str);
        handle_serve("fastly").expect("serve command runs");
    }
}
