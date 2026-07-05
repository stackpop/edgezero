use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use walkdir::WalkDir;

/// # Errors
/// Returns an error if the Fastly CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip1",
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
/// Returns an error if the Fastly CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute deploy failed with status {status}"));
    }

    Ok(())
}

/// # Errors
/// Returns an error if the Fastly CLI serve command (Viceroy) fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "serve"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute serve failed with status {status}"));
    }

    Ok(())
}

fn find_fastly_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "fastly.toml") {
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
            path.file_name().is_some_and(|n| n == "fastly.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate fastly.toml".to_owned());
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
    let target_triple = "wasm32-wasip1";
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(target_triple)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(target_triple)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(target_triple)
        .join("release")
        .join(&release_name);
    if workspace_target.exists() {
        return Ok(workspace_target);
    }

    Err(format!(
        "compiled artifact not found (looked in {} and workspace target)",
        manifest_dir.display()
    ))
}

/// Synthesised baseline `fastly.toml` for clean clones. Built via
/// `toml_edit::DocumentMut` (NOT raw `format!`) so any legal
/// `[app].name` — including names with TOML-significant characters
/// like `"`, `\`, or newlines — is escaped correctly. Manifest
/// validation today only length-bounds the name; raw interpolation
/// would produce invalid TOML for legal inputs.
///
/// `service_id` from `[adapters.fastly.deployed]` is threaded
/// through as `Option<&str>`; when `None` the key is OMITTED so the
/// operator's first `fastly compute deploy` populates it (per spec
/// §"Writeback ownership" — we deliberately don't emit
/// `service_id = ""`).
pub(crate) fn synthesise_fastly_toml(app_name: &str, service_id: Option<&str>) -> String {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix("# edgezero-provision: v1\n");
    // `Table::insert` returns the previous value (if any). We build a
    // fresh document from `DocumentMut::new()`, so nothing to displace
    // -- but the return is discarded intentionally. Using `insert`
    // instead of `doc["..."] = ...` sidesteps `clippy::indexing_slicing`
    // (the index form panics if the key is missing; `insert` doesn't).
    doc.insert("manifest_version", value(3));
    doc.insert("name", value(app_name));
    doc.insert("language", value("rust"));
    if let Some(sid) = service_id {
        doc.insert("service_id", value(sid));
    }
    // `[scripts]` and `[local_server]` are the standard Fastly Compute
    // scaffold tables. `scripts.build` pins the cargo target so
    // `fastly compute build` reproduces the wasm artifact; the empty
    // `[local_server]` header is a placeholder the operator fills in
    // when seeding local viceroy state (config-store contents,
    // per-request backends, etc.).
    let mut scripts = Table::new();
    scripts.insert(
        "build",
        value("cargo build --profile release --target wasm32-wasip1"),
    );
    doc.insert("scripts", Item::Table(scripts));
    doc.insert("local_server", Item::Table(Table::new()));
    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    use tempfile::tempdir;

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("fastly.toml"), "name=\"first\"").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("fastly.toml"), "name=\"second\"").unwrap();

        let found = find_fastly_manifest(&second).unwrap();
        assert_eq!(found, second.join("fastly.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("fastly.toml"), "name = \"demo\"").unwrap();

        let manifest = find_fastly_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("fastly.toml"));
    }

    #[test]
    fn locate_artifact_considers_workspace_target() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(manifest_dir.join("target/wasm32-wasip1/release")).unwrap();
        let artifact = workspace.join("target/wasm32-wasip1/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "demo").unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn read_package_falls_back_to_name() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "name = \"demo\"").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    #[test]
    fn read_package_prefers_package_table() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"demo\"\n").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    // ---------- synthesise_fastly_toml ----------

    #[test]
    fn synthesises_minimal_fastly_toml_with_header_and_no_service_id() {
        let out = synthesise_fastly_toml("demo", None);
        assert!(out.starts_with("# edgezero-provision: v1"));
        assert!(out.contains("manifest_version = 3"));
        assert!(out.contains(r#"name = "demo""#));
        assert!(out.contains(r#"language = "rust""#));
        assert!(out.contains("[scripts]"));
        assert!(out.contains("[local_server]"));
        assert!(
            !out.contains("service_id"),
            "no service_id key when None: {out}"
        );
    }

    #[test]
    fn synthesises_fastly_toml_pins_service_id_when_deployed_present() {
        let out = synthesise_fastly_toml("demo", Some("SVC1"));
        // Reparse-and-index: substring `service_id = "SVC1"` passes
        // for both the correct root form AND the shipped bug where
        // service_id landed inside `[local_server]`. Explicitly assert
        // it's at the ROOT of the doc.
        let doc: toml_edit::DocumentMut = out.parse().expect("re-parse synthesised fastly.toml");
        assert_eq!(
            doc.get("service_id").and_then(toml_edit::Item::as_str),
            Some("SVC1"),
            "service_id must live at the TOML root, not nested under a section: {out}"
        );
        // Also assert no `local_server.service_id` -- that would be
        // the exact silent-drift bug we're guarding against.
        let local_server_carries_it = doc
            .get("local_server")
            .and_then(|item| item.as_table())
            .and_then(|tbl| tbl.get("service_id"))
            .is_some();
        assert!(
            !local_server_carries_it,
            "service_id must NOT appear under `[local_server]`: {out}"
        );
    }

    #[test]
    fn synthesise_fastly_toml_escapes_pathological_app_names() {
        for name in [
            r#"has"quote"#,
            r"has\backslash",
            "has\nnewline",
            "has = equals",
        ] {
            let out = synthesise_fastly_toml(name, None);
            // Re-parsing must succeed AND round-trip the name.
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(doc["name"].as_str(), Some(name), "input: {name:?}");
        }
    }

    #[test]
    fn synthesise_fastly_toml_escapes_pathological_service_ids() {
        // `fastly compute deploy` may return arbitrary strings.
        for sid in [r#"has"quote"#, r"has\slash", "has\nnewline"] {
            let out = synthesise_fastly_toml("demo", Some(sid));
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(doc["service_id"].as_str(), Some(sid), "input: {sid:?}");
        }
    }
}
