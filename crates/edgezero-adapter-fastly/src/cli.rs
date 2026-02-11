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

pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest = find_fastly_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_string())?;
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
    let manifest = find_fastly_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_string())?;

    let status = Command::new("fastly")
        .args(["compute", "deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run fastly CLI: {e}"))?;
    if !status.success() {
        return Err(format!("fastly compute deploy failed with status {status}"));
    }

    Ok(())
}

pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest = find_fastly_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_string())?;

    let status = Command::new("fastly")
        .args(["compute", "serve"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run fastly CLI: {e}"))?;
    if !status.success() {
        return Err(format!("fastly compute serve failed with status {status}"));
    }

    Ok(())
}

struct FastlyCliAdapter;

static FASTLY_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "fastly_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "fastly_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_fastly_toml",
        contents: include_str!("templates/fastly.toml.hbs"),
    },
];

static FASTLY_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "fastly_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "fastly_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "fastly_cargo_config_toml",
        output: ".cargo/config.toml",
    },
    AdapterFileSpec {
        template: "fastly_fastly_toml",
        output: "fastly.toml",
    },
];

static FASTLY_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_fastly",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"ssh://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback:
            "edgezero-adapter-fastly = { git = \"ssh://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly_wasm",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback:
            "edgezero-adapter-fastly = { git = \"ssh://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false, features = [\"fastly\"] }",
        features: &["fastly"],
    },
];

static FASTLY_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "fastly",
    display_name: "Fastly Compute@Edge",
    crate_suffix: "adapter-fastly",
    dependency_crate: "edgezero-adapter-fastly",
    dependency_repo_path: "crates/edgezero-adapter-fastly",
    template_registrations: FASTLY_TEMPLATE_REGISTRATIONS,
    files: FASTLY_FILE_SPECS,
    extra_dirs: &["src", ".cargo"],
    dependencies: FASTLY_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "fastly.toml",
        build_target: "wasm32-wasip1",
        build_profile: "release",
        build_features: &["fastly"],
    },
    commands: CommandTemplates {
        build: "cargo build --release --target wasm32-wasip1 -p {crate}",
        serve: "fastly compute serve -C {crate_dir}",
        deploy: "fastly compute deploy -C {crate_dir}",
    },
    logging: LoggingDefaults {
        endpoint: Some("stdout"),
        level: "info",
        echo_stdout: Some(true),
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &[
            "`cd {crate_dir}`",
            "`fastly compute serve` or `edgezero-cli serve --adapter fastly`",
        ],
    },
    run_module: "edgezero_adapter_fastly",
};

static FASTLY_ADAPTER: FastlyCliAdapter = FastlyCliAdapter;

impl Adapter for FastlyCliAdapter {
    fn name(&self) -> &'static str {
        "fastly"
    }

    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            AdapterAction::Build => {
                let artifact = build(args)?;
                println!("[edgezero] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
        }
    }
}

pub fn register() {
    register_adapter(&FASTLY_ADAPTER);
    register_adapter_blueprint(&FASTLY_BLUEPRINT);
}

#[ctor]
fn register_ctor() {
    register();
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
            path.file_name()
                .map(|n| n == "fastly.toml")
                .unwrap_or(false)
                && path
                    .parent()
                    .map(|dir| dir.join("Cargo.toml").exists())
                    .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate fastly.toml".to_string());
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
    let release_name = format!("{crate_name}.wasm");

    if let Some(custom) = std::env::var_os("CARGO_TARGET_DIR") {
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
        fs::write(root.join("fastly.toml"), "name = \"demo\"").unwrap();

        let manifest = find_fastly_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("fastly.toml"));
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
        fs::write(first.join("fastly.toml"), "name=\"first\"").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("fastly.toml"), "name=\"second\"").unwrap();

        let found = find_fastly_manifest(&second).unwrap();
        assert_eq!(found, second.join("fastly.toml"));
    }
}
