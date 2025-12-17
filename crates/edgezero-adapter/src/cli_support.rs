#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

/// Walks up the directory tree looking for `manifest_name` alongside a `Cargo.toml`.
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

/// Returns the nearest ancestor containing a `Cargo.toml`, defaulting to `dir` when none are found.
pub fn find_workspace_root(dir: &Path) -> PathBuf {
    let mut current: Option<&Path> = Some(dir);
    let mut candidate: Option<PathBuf> = None;

    while let Some(path) = current {
        if path.join("Cargo.toml").exists() {
            candidate = Some(path.to_path_buf());
        }
        current = path.parent();
    }

    candidate.unwrap_or_else(|| dir.to_path_buf())
}

/// Calculates the path distance between two directories based on shared leading components.
pub fn path_distance(a: &Path, b: &Path) -> usize {
    let a_components: Vec<_> = a.components().collect();
    let b_components: Vec<_> = b.components().collect();

    let mut common = 0;
    for (ac, bc) in a_components.iter().zip(&b_components) {
        if ac == bc {
            common += 1;
        } else {
            break;
        }
    }

    (a_components.len() - common) + (b_components.len() - common)
}

/// Reads the crate name from a `Cargo.toml`, supporting both the inline and `[package]` forms.
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
        return Ok(name.to_string());
    }

    if let Some(name) = table.get("name").and_then(|value| value.as_str()) {
        return Ok(name.to_string());
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
    fn path_distance_counts_divergence() {
        let a = Path::new("/a/b/c");
        let b = Path::new("/a/b/d/e");
        assert_eq!(path_distance(a, b), 3);
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
}
