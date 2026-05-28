//! `provision` command.
//!
//! Thin delegate to the adapter registry. The CLI loads the manifest,
//! resolves the named adapter, hands it the declared store ids per
//! kind, and prints the human-readable status lines the adapter
//! returns. All the platform-specific work — `wrangler kv namespace
//! create`, `fastly *-store create`, `spin.toml` editing — lives in
//! each `edgezero-adapter-*` crate's `Adapter::provision` impl, not
//! here.

use std::path::Path;

use crate::args::ProvisionArgs;
use crate::ensure_adapter_defined;
use edgezero_adapter::registry::{self as adapter_registry, ProvisionStores};
use edgezero_core::manifest::ManifestLoader;

/// # Errors
///
/// Returns an error string if the manifest can't be loaded, the
/// adapter isn't declared in `[adapters.*]`, the adapter isn't
/// registered in this build, or the adapter's `provision` impl
/// reports a failure.
#[inline]
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String> {
    let manifest_loader = ManifestLoader::from_path(&args.manifest)
        .map_err(|err| format!("failed to load {}: {err}", args.manifest.display()))?;
    let manifest = manifest_loader.manifest();

    // Declared in `edgezero.toml`? (Catches typos before we try to
    // look the adapter up in the registry.)
    ensure_adapter_defined(&args.adapter, Some(&manifest_loader))?;
    let adapter_cfg = manifest.adapters.get(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is not declared in {}",
            args.adapter,
            args.manifest.display()
        )
    })?;

    // Linked in this build? Adapters are feature-gated; a release
    // built without `--features cloudflare` won't have it
    // registered even if the manifest declares it.
    let adapter = adapter_registry::get_adapter(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is declared in {} but not registered in this build (rebuild `edgezero-cli` with its feature enabled)",
            args.adapter,
            args.manifest.display()
        )
    })?;

    let manifest_root = args
        .manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let stores = ProvisionStores {
        config: manifest
            .stores
            .config
            .as_ref()
            .map_or(&[][..], |decl| decl.ids.as_slice()),
        kv: manifest
            .stores
            .kv
            .as_ref()
            .map_or(&[][..], |decl| decl.ids.as_slice()),
        secrets: manifest
            .stores
            .secrets
            .as_ref()
            .map_or(&[][..], |decl| decl.ids.as_slice()),
    };

    let lines = adapter.provision(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
        &stores,
        args.dry_run,
    )?;

    if args.dry_run {
        log::info!("[edgezero] provision --dry-run for `{}`:", args.adapter);
    }
    for line in lines {
        log::info!("{line}");
    }
    Ok(())
}
