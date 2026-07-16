use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use walkdir::WalkDir;

use super::TARGET_TRIPLE;

/// # Errors
/// Returns an error if the Cloudflare wrangler build command fails.
pub(super) fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            TARGET_TRIPLE,
            "--manifest-path",
            cargo_manifest
                .to_str()
                .ok_or("invalid Cargo manifest path")?,
        ])
        .args(extra_args)
        .status()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }

    let workspace_root = find_workspace_root(manifest_dir);
    let artifact = locate_artifact(&workspace_root, manifest_dir, &crate_name)?;
    let pkg_dir = workspace_root.join("pkg");
    fs::create_dir_all(&pkg_dir)
        .map_err(|err| format!("failed to create {}: {err}", pkg_dir.display()))?;
    let dest = pkg_dir.join(format!("{}.wasm", crate_name.replace('-', "_")));
    fs::copy(&artifact, &dest)
        .map_err(|err| format!("failed to copy artifact to {}: {err}", dest.display()))?;

    Ok(dest)
}

/// # Errors
/// Returns an error if the Cloudflare wrangler deploy command fails.
pub(super) fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["deploy", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler deploy failed with status {status}"));
    }

    Ok(())
}

/// # Errors
/// Returns an error if the Cloudflare wrangler dev command fails.
pub(super) fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["dev", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler dev failed with status {status}"));
    }

    Ok(())
}

fn find_wrangler_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "wrangler.toml") {
        return Ok(found);
    }

    let root = find_workspace_root(start);
    let mut candidates: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(true)
        .max_depth(8)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| {
            path.file_name().is_some_and(|n| n == "wrangler.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate wrangler.toml".to_owned());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn locate_artifact(
    workspace_root: &Path,
    manifest_dir: &Path,
    crate_name: &str,
) -> Result<PathBuf, String> {
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(TARGET_TRIPLE)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if workspace_target.exists() {
        return Ok(workspace_target);
    }

    Err(format!(
        "compiled artifact not found for {crate_name} (looked in manifest and workspace target directories)"
    ))
}

/// Synthesised baseline `wrangler.toml` for scaffold-time and
/// clean-clone bootstrap (single source — the Cloudflare blueprint
/// has no scaffold `.hbs` template for `wrangler.toml`, so
/// `edgezero new` and clean-clone `provision --local` produce
/// byte-identical output; see the "Generated Adapter manifests"
/// note in the spec).
///
/// The `name` field spells the adapter crate's Cargo package name.
/// The caller in `cli/mod.rs` reads this from the `Cargo.toml`
/// adjacent to the adapter manifest (honouring the operator's
/// `[adapters.cloudflare.adapter].crate` rename) and falls back
/// to the scaffold convention `<app_name>-adapter-cloudflare`
/// only when no Cargo.toml is discoverable. `worker-build` reads
/// this field and expects it to match the Cargo package it builds.
///
/// Built via `toml_edit::DocumentMut` (NOT raw `format!`) so any
/// legal name — including values with TOML-significant characters
/// like `"`, `\`, or newlines — is escaped correctly.
pub(super) fn synthesise_wrangler_toml(crate_name: &str) -> String {
    use toml_edit::{DocumentMut, Item, Table, value};

    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix("# edgezero-provision: v1\n");
    // `Table::insert` returns the previous value (if any). We build a
    // fresh document from `DocumentMut::new()`, so nothing to displace
    // -- but the return is discarded intentionally. Using `insert`
    // instead of `doc["..."] = ...` sidesteps `clippy::indexing_slicing`
    // (the index form panics if the key is missing; `insert` doesn't).
    doc.insert("name", value(crate_name));
    doc.insert("main", value("build/worker/shim.mjs"));
    doc.insert("compatibility_date", value("2024-01-01"));

    // [build] — wrangler reads this to know how to compile the
    // Rust crate into a Worker bundle. Without it the operator
    // has to invoke `worker-build --release` manually before
    // every `wrangler dev` / `wrangler deploy`. Match the
    // pre-2026-07 scaffold template default.
    let mut build = Table::new();
    build.insert("command", value("worker-build --release"));
    doc.insert("build", Item::Table(build));

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- synthesise_wrangler_toml ----------

    #[test]
    fn synthesises_minimal_wrangler_toml_with_header() {
        // Caller resolves the crate name from the adapter-crate
        // Cargo.toml (see `read_adapter_crate_name` in cli_support);
        // the synth itself is a pure toml_edit shape emitter over the
        // resolved value.
        let out = synthesise_wrangler_toml("demo-adapter-cloudflare");
        assert!(out.starts_with("# edgezero-provision: v1"));
        assert!(out.contains(r#"name = "demo-adapter-cloudflare""#));
        assert!(out.contains(r#"main = "build/worker/shim.mjs""#));
        assert!(out.contains("compatibility_date = "));
        // [build] table is required so `wrangler deploy` knows how
        // to compile the Rust crate into a Worker bundle.
        assert!(
            out.contains(r#"command = "worker-build --release""#),
            "wrangler.toml must ship a [build] command: {out}"
        );
    }

    #[test]
    fn synthesise_wrangler_toml_escapes_pathological_crate_names() {
        // Adapter crate names come from Cargo.toml `[package].name`
        // — Cargo restricts them to `[A-Za-z0-9_-]`, but the synth
        // must still be defensive against TOML-hostile inputs so
        // an operator that stashes something exotic into
        // `[adapters.<name>.adapter].crate` doesn't produce
        // invalid TOML.
        for name in [
            r#"has"quote"#,
            r"has\backslash",
            "has\nnewline",
            "has = equals",
        ] {
            let out = synthesise_wrangler_toml(name);
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(doc["name"].as_str(), Some(name), "input: {name:?}");
        }
    }
}
