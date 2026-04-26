//! `EdgeZero` CLI.

#[cfg(feature = "cli")]
mod adapter;
#[cfg(feature = "cli")]
mod args;
#[cfg(all(feature = "cli", feature = "edgezero-adapter-axum"))]
mod dev_server;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod scaffold;

#[cfg(feature = "cli")]
use edgezero_core::manifest::ManifestLoader;
#[cfg(feature = "cli")]
use std::io::ErrorKind;
#[cfg(feature = "cli")]
use std::path::PathBuf;

/// Initialize a CLI logger that prints messages without timestamps or level
/// prefixes — the CLI's output IS the user-facing UX, not a debug log.
#[cfg(feature = "cli")]
fn init_cli_logger() {
    use log::LevelFilter;
    use simple_logger::SimpleLogger;
    let _logger_init = SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .without_timestamps()
        .with_module_level("edgezero_cli", LevelFilter::Info)
        .init();
}

#[cfg(feature = "cli")]
fn main() {
    use args::{Args, Command};
    use clap::Parser as _;

    init_cli_logger();
    let args = Args::parse();
    match args.cmd {
        Command::New(new_args) => {
            if let Err(e) = generator::generate_new(&new_args) {
                log::error!("[edgezero] new error: {e}");
                std::process::exit(1);
            }
        }
        Command::Build {
            adapter,
            adapter_args,
        } => {
            if let Err(err) = handle_build(&adapter, &adapter_args) {
                log::error!("[edgezero] build error: {err}");
                std::process::exit(1);
            }
        }
        Command::Deploy {
            adapter,
            adapter_args,
        } => {
            if let Err(err) = handle_deploy(&adapter, &adapter_args) {
                log::error!("[edgezero] deploy error: {err}");
                std::process::exit(1);
            }
        }
        Command::Serve { adapter } => {
            if let Err(err) = handle_serve(&adapter) {
                log::error!("[edgezero] serve error: {err}");
                std::process::exit(1);
            }
        }
        Command::Dev => {
            #[cfg(feature = "edgezero-adapter-axum")]
            {
                dev_server::run_dev();
            }

            #[cfg(not(feature = "edgezero-adapter-axum"))]
            {
                log::error!(
                    "edgezero-cli built without `edgezero-adapter-axum`; rebuild with that feature to use `edgezero dev`."
                );
                std::process::exit(1);
            }
        }
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    use log::LevelFilter;
    use simple_logger::SimpleLogger;
    let _logger_init = SimpleLogger::new()
        .with_level(LevelFilter::Error)
        .without_timestamps()
        .init();
    log::error!("edgezero-cli built without `cli` feature. Rebuild with `--features cli`.");
}

#[cfg(feature = "cli")]
fn store_bindings_message(adapter_name: &str, manifest: &ManifestLoader) -> Option<String> {
    let m = manifest.manifest();
    if !m.secret_store_enabled(adapter_name) {
        return None;
    }

    let binding_name = m.secret_store_name(adapter_name);
    let message = match adapter_name {
        "axum" => format!(
            "[edgezero] secrets enabled for axum -- ensure the required environment variables are set for local runs (configured store name: '{binding_name}')"
        ),
        "cloudflare" => format!(
            "[edgezero] secrets enabled for cloudflare -- ensure the required secret bindings exist in wrangler (configured store name: '{binding_name}' is metadata only)"
        ),
        _ => format!(
            "[edgezero] secret store '{binding_name}' enabled for {adapter_name} -- ensure it is provisioned on the target platform"
        ),
    };

    Some(message)
}

#[cfg(feature = "cli")]
fn log_store_bindings(adapter_name: &str, manifest: &ManifestLoader) {
    if let Some(message) = store_bindings_message(adapter_name, manifest) {
        log::info!("{message}");
    }
}

#[cfg(feature = "cli")]
fn handle_build(adapter_name: &str, adapter_args: &[String]) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(adapter_name, manifest.as_ref())?;
    if let Some(m) = &manifest {
        log_store_bindings(adapter_name, m);
    }
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
    manifest_loader: Option<&ManifestLoader>,
) -> Result<(), String> {
    if let Some(loader) = manifest_loader {
        if loader.manifest().adapters.contains_key(adapter_name) {
            return Ok(());
        }
        let available: Vec<String> = loader.manifest().adapters.keys().cloned().collect();
        if available.is_empty() {
            Err(format!(
                "adapter `{adapter_name}` is not configured in edgezero.toml (no adapters defined)"
            ))
        } else {
            Err(format!(
                "adapter `{}` is not configured in edgezero.toml (available: {})",
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
    let path = std::env::var("EDGEZERO_MANIFEST")
        .map_or_else(|_| PathBuf::from("edgezero.toml"), PathBuf::from);

    match ManifestLoader::from_path(&path) {
        Ok(loader) => Ok(Some(loader)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to load {}: {err}", path.display())),
    }
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;
    use edgezero_core::manifest::ManifestLoader;
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
            if let Some(original) = &self.original {
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
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let result = load_manifest_optional().expect("load result");
        assert!(result.is_none());
    }

    #[test]
    fn load_manifest_optional_reads_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let manifest = load_manifest_optional()
            .expect("load result")
            .expect("manifest present");
        assert!(manifest.manifest().adapters.contains_key("fastly"));
    }

    #[test]
    fn ensure_adapter_defined_accepts_known_adapter() {
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        ensure_adapter_defined("fastly", Some(&loader)).expect("known adapter");
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
        ensure_adapter_defined("fastly", None).expect("manifest missing -> permissive");
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_build_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args: Vec<String> = Vec::new();
        handle_build("fastly", &args).expect("build command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_deploy_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args: Vec<String> = Vec::new();
        handle_deploy("fastly", &args).expect("deploy command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn handle_serve_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        handle_serve("fastly").expect("serve command runs");
    }

    #[test]
    fn secret_store_name_is_readable_from_manifest() {
        let manifest_with_secrets = r#"
[app]
name = "demo-app"
entry = "crates/demo-core"

[stores.secrets]
name = "MY_SECRETS"

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
"#;
        let loader = ManifestLoader::load_from_str(manifest_with_secrets);
        assert_eq!(loader.manifest().secret_store_name("fastly"), "MY_SECRETS");
        assert!(loader.manifest().stores.secrets.is_some());
    }

    #[test]
    fn store_bindings_message_is_adapter_specific() {
        let loader = ManifestLoader::load_from_str(
            r#"
[stores.secrets]
name = "MY_SECRETS"
"#,
        );

        let axum = store_bindings_message("axum", &loader).expect("axum message");
        assert!(axum.contains("environment variables"));

        let cloudflare = store_bindings_message("cloudflare", &loader).expect("cloudflare message");
        assert!(cloudflare.contains("wrangler"));

        let fastly = store_bindings_message("fastly", &loader).expect("fastly message");
        assert!(fastly.contains("secret store 'MY_SECRETS'"));
    }

    #[test]
    fn store_bindings_message_respects_secret_store_enabled() {
        let loader = ManifestLoader::load_from_str(
            "
[stores.secrets]
enabled = false
",
        );
        assert!(store_bindings_message("fastly", &loader).is_none());
    }
}
