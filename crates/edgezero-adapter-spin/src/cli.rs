use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use edgezero_adapter::{register_adapter, Adapter, AdapterAction};
use walkdir::WalkDir;

const TARGET_TRIPLE: &str = "wasm32-wasip1";

pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest = find_spin_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_string())?;
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
        .map_err(|e| format!("failed to run cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }

    let workspace_root = find_workspace_root(manifest_dir);
    let artifact = locate_artifact(&workspace_root, manifest_dir, &crate_name)?;
    let pkg_dir = workspace_root.join("pkg");
    fs::create_dir_all(&pkg_dir)
        .map_err(|e| format!("failed to create {}: {e}", pkg_dir.display()))?;
    let dest = pkg_dir.join(format!("{crate_name}.wasm"));
    fs::copy(&artifact, &dest)
        .map_err(|e| format!("failed to copy artifact to {}: {e}", dest.display()))?;

    Ok(dest)
}

pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest = find_spin_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_string())?;

    let status = Command::new("spin")
        .args(["deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run spin CLI: {e}"))?;
    if !status.success() {
        return Err(format!("spin deploy failed with status {status}"));
    }

    Ok(())
}

pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest = find_spin_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_string())?;

    let status = Command::new("spin")
        .args(["up"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run spin CLI: {e}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

struct SpinCliAdapter;

static SPIN_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "spin_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_src_lib_rs",
        contents: include_str!("templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "spin_spin_toml",
        contents: include_str!("templates/spin.toml.hbs"),
    },
];

static SPIN_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "spin_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "spin_src_lib_rs",
        output: "src/lib.rs",
    },
    AdapterFileSpec {
        template: "spin_spin_toml",
        output: "spin.toml",
    },
];

static SPIN_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_spin",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin_wasm",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false, features = [\"spin\"] }",
        features: &["spin"],
    },
];

static SPIN_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "spin",
    display_name: "Spin (Fermyon)",
    crate_suffix: "adapter-spin",
    dependency_crate: "edgezero-adapter-spin",
    dependency_repo_path: "crates/edgezero-adapter-spin",
    template_registrations: SPIN_TEMPLATE_REGISTRATIONS,
    files: SPIN_FILE_SPECS,
    extra_dirs: &["src"],
    dependencies: SPIN_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "spin.toml",
        build_target: "wasm32-wasip1",
        build_profile: "release",
        build_features: &["spin"],
    },
    commands: CommandTemplates {
        build: "cargo build --target wasm32-wasip1 --release -p {crate_name}",
        deploy: "spin deploy --from {crate_dir}",
        serve: "spin up --from {crate_dir}",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`edgezero-cli serve --adapter spin`"],
    },
    run_module: "edgezero_adapter_spin",
};

static SPIN_ADAPTER: SpinCliAdapter = SpinCliAdapter;

impl Adapter for SpinCliAdapter {
    fn name(&self) -> &'static str {
        "spin"
    }

    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            AdapterAction::Build => {
                let artifact = build(args)?;
                println!("[edgezero] Spin build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
        }
    }
}

pub fn register() {
    register_adapter(&SPIN_ADAPTER);
    register_adapter_blueprint(&SPIN_BLUEPRINT);
}

#[ctor]
fn register_ctor() {
    register();
}

fn find_spin_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "spin.toml") {
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
            path.file_name().map(|n| n == "spin.toml").unwrap_or(false)
                && path
                    .parent()
                    .map(|dir| dir.join("Cargo.toml").exists())
                    .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate spin.toml".to_string());
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
    let release_name = format!("{crate_name}.wasm");

    if let Some(custom) = std::env::var_os("CARGO_TARGET_DIR") {
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
        "compiled artifact not found (looked in {} and workspace target)",
        manifest_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    use tempfile::tempdir;

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let manifest = find_spin_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("spin.toml"));
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
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let found = find_spin_manifest(&second).unwrap();
        assert_eq!(found, second.join("spin.toml"));
    }
}
