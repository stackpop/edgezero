#![allow(
    dead_code,
    reason = "helpers consumed conditionally via the `cli` feature in adapter crates"
)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Walks up the directory tree looking for `manifest_name` alongside a `Cargo.toml`.
#[inline]
#[must_use]
pub fn find_manifest_upwards(start: &Path, manifest_name: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(manifest_name);
        if candidate.exists() && dir.join("Cargo.toml").exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// Returns the workspace root for `dir` by walking upward and stopping at the
/// first `Cargo.toml` that contains a `[workspace]` table.  If no workspace
/// table is found, falls back to the highest ancestor containing a `Cargo.toml`,
/// and finally to `dir` itself.
#[inline]
#[must_use]
pub fn find_workspace_root(dir: &Path) -> PathBuf {
    let mut current: Option<&Path> = Some(dir);
    let mut candidate: Option<PathBuf> = None;

    while let Some(path) = current {
        let cargo = path.join("Cargo.toml");
        if cargo.exists() {
            candidate = Some(path.to_path_buf());
            if fs::read_to_string(&cargo).is_ok_and(|contents| contents.contains("[workspace]")) {
                break;
            }
        }
        current = path.parent();
    }

    candidate.unwrap_or_else(|| dir.to_path_buf())
}

/// Calculates the path distance between two directories based on shared leading components.
#[inline]
#[must_use]
pub fn path_distance(left: &Path, right: &Path) -> usize {
    let left_components: Vec<_> = left.components().collect();
    let right_components: Vec<_> = right.components().collect();

    let common = left_components
        .iter()
        .zip(&right_components)
        .take_while(|&(lhs, rhs)| lhs == rhs)
        .count();

    left_components
        .len()
        .saturating_sub(common)
        .saturating_add(right_components.len().saturating_sub(common))
}

/// Spawn `program args…` inheriting parent stdio, returning a
/// human-readable error message.
///
/// Used by every adapter's auth dispatch (`wrangler login`,
/// `fastly profile create`, `spin cloud login`, …). The
/// `install_hint` is appended to the not-found message so the
/// adapter can point operators at the right install instructions
/// (`npm install -g wrangler`, the Fastly CLI download page, etc.).
///
/// # Errors
/// Returns an error string if the binary is missing from `PATH`,
/// the child fails to spawn, or it exits non-zero.
#[inline]
pub fn run_native_cli(program: &str, args: &[&str], install_hint: &str) -> Result<(), String> {
    let status = Command::new(program).args(args).status().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            format!("`{program}` not found on PATH; {install_hint}")
        } else {
            format!("failed to spawn `{program}`: {err}")
        }
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {}` exited with status {status}",
            args.join(" ")
        ))
    }
}

/// Reads the crate name from a `Cargo.toml`, supporting both the inline and `[package]` forms.
///
/// # Errors
/// Returns an error if the manifest cannot be read or its `[package].name` field is missing.
#[inline]
pub fn read_package_name(manifest: &Path) -> Result<String, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|err| format!("failed to read {}: {err}", manifest.display()))?;
    let table: toml::Value = toml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.display()))?;

    if let Some(name) = table
        .get("package")
        .and_then(|pkg| pkg.get("name"))
        .and_then(|value| value.as_str())
    {
        return Ok(name.to_owned());
    }

    if let Some(name) = table.get("name").and_then(|value| value.as_str()) {
        return Ok(name.to_owned());
    }

    Err(format!(
        "package.name or name missing from {}",
        manifest.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn workspace_root_defaults_to_dir_when_no_cargo_toml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested");
        fs::create_dir_all(&child).unwrap();

        assert_eq!(find_workspace_root(&child), child);
    }

    #[test]
    fn workspace_root_finds_nearest_manifest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        assert_eq!(find_workspace_root(&child), root);
    }

    #[test]
    fn workspace_root_stops_at_workspace_table() {
        let dir = tempdir().unwrap();
        let outer = dir.path();

        // Outer repo root with a Cargo.toml
        fs::write(
            outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"examples/*\"]",
        )
        .unwrap();

        // Inner workspace (e.g. examples/app-demo)
        let inner = outer.join("examples/app-demo");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            inner.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]",
        )
        .unwrap();

        // Crate inside the inner workspace
        let crate_dir = inner.join("crates/my-adapter");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"my-adapter\"",
        )
        .unwrap();

        // Should resolve to the inner workspace, not the outer repo root.
        assert_eq!(find_workspace_root(&crate_dir), inner);
    }

    #[test]
    fn path_distance_counts_divergence() {
        let left = Path::new("/a/b/c");
        let right = Path::new("/a/b/d/e");
        assert_eq!(path_distance(left, right), 3);
    }

    #[test]
    fn read_package_prefers_package_table() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"demo\"\n").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
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
    fn find_manifest_upwards_matches_manifest_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested/level");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("demo.toml"), "[cfg]\n").unwrap();

        let found = find_manifest_upwards(&child, "demo.toml").expect("manifest");
        assert_eq!(found, root.join("demo.toml"));
    }

    #[test]
    fn run_native_cli_missing_program_surfaces_install_hint() {
        let err = run_native_cli("edgezero-no-such-program-xyz", &[], "install the thing")
            .expect_err("missing program must error");
        assert!(err.contains("install the thing"), "got: {err}");
    }

    #[test]
    fn run_native_cli_nonzero_exit_is_error() {
        // `false` exits non-zero on every supported CI host (unix/macOS).
        let err = run_native_cli("false", &[], "hint").expect_err("non-zero exit must error");
        // Pin the exit-status branch specifically — `!is_empty()` would
        // also pass for the wrong (not-found / spawn) branch.
        assert!(
            err.contains("exited with status"),
            "expected the exit-status branch, got: {err}"
        );
    }
}
