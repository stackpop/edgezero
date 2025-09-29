//! AnyEdge CLI.

#[cfg(feature = "cli")]
mod args;
#[cfg(feature = "cli")]
mod dev_server;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod provider;
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
        Command::Build { provider } => {
            if let Err(err) = handle_build(&provider) {
                eprintln!("[anyedge] build error: {err}");
                std::process::exit(1);
            }
        }
        Command::Deploy { provider } => {
            if let Err(err) = handle_deploy(&provider) {
                eprintln!("[anyedge] deploy error: {err}");
                std::process::exit(1);
            }
        }
        Command::Serve { provider } => {
            if let Err(err) = handle_serve(&provider) {
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
fn handle_build(provider: &str) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_provider_defined(provider, manifest.as_ref())?;
    provider::execute(provider, provider::Action::Build, manifest.as_ref())
}

#[cfg(feature = "cli")]
fn handle_deploy(provider: &str) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_provider_defined(provider, manifest.as_ref())?;
    provider::execute(provider, provider::Action::Deploy, manifest.as_ref())
}

#[cfg(feature = "cli")]
fn handle_serve(provider: &str) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_provider_defined(provider, manifest.as_ref())?;
    provider::execute(provider, provider::Action::Serve, manifest.as_ref())
}

#[cfg(feature = "cli")]
fn ensure_provider_defined(
    provider: &str,
    manifest: Option<&ManifestLoader>,
) -> Result<(), String> {
    if let Some(manifest) = manifest {
        if manifest.manifest().providers.contains_key(provider) {
            return Ok(());
        }
        let available: Vec<String> = manifest.manifest().providers.keys().cloned().collect();
        if available.is_empty() {
            Err(format!(
                "provider `{}` is not configured in anyedge.toml (no providers defined)",
                provider
            ))
        } else {
            Err(format!(
                "provider `{}` is not configured in anyedge.toml (available: {})",
                provider,
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
