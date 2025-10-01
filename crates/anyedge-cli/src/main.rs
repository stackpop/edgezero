//! AnyEdge CLI.

#[cfg(feature = "cli")]
mod adapter;
#[cfg(feature = "cli")]
mod args;
#[cfg(feature = "cli")]
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
            dev_server::run_dev();
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
