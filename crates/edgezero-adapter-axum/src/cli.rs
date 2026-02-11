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
use toml::Value;
use walkdir::WalkDir;

static AXUM_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "axum_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "axum_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "axum_axum_toml",
        contents: include_str!("templates/axum.toml.hbs"),
    },
];

static AXUM_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "axum_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "axum_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "axum_axum_toml",
        output: "axum.toml",
    },
];

static AXUM_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_axum",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\" }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_axum",
        repo_crate: "crates/edgezero-adapter-axum",
        fallback:
            "edgezero-adapter-axum = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-axum\", default-features = false }",
        features: &["axum"],
    },
];

static AXUM_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "axum",
    display_name: "Axum",
    crate_suffix: "adapter-axum",
    dependency_crate: "edgezero-adapter-axum",
    dependency_repo_path: "crates/edgezero-adapter-axum",
    template_registrations: AXUM_TEMPLATE_REGISTRATIONS,
    files: AXUM_FILE_SPECS,
    extra_dirs: &["src"],
    dependencies: AXUM_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "axum.toml",
        build_target: "native",
        build_profile: "dev",
        build_features: &[],
    },
    commands: CommandTemplates {
        build: "cargo build -p {crate}",
        serve: "cargo run -p {crate}",
        deploy: "# configure deployment for Axum",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: Some(true),
    },
    readme: ReadmeInfo {
        description: "{display} adapter entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &[
            "`cd {crate_dir}`",
            "`cargo run` or `edgezero-cli serve --adapter axum`",
        ],
    },
    run_module: "edgezero_adapter_axum",
};

struct AxumCliAdapter;

static AXUM_ADAPTER: AxumCliAdapter = AxumCliAdapter;

impl Adapter for AxumCliAdapter {
    fn name(&self) -> &'static str {
        "axum"
    }

    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            AdapterAction::Build => build(args),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
        }
    }
}

pub fn register() {
    register_adapter(&AXUM_ADAPTER);
    register_adapter_blueprint(&AXUM_BLUEPRINT);
}

#[ctor]
fn register_ctor() {
    register();
}

fn build(extra_args: &[String]) -> Result<(), String> {
    let project = locate_project()?;
    run_cargo(&project, "build", extra_args)
}

fn serve(extra_args: &[String]) -> Result<(), String> {
    let project = locate_project()?;
    run_cargo(&project, "run", extra_args)
}

fn deploy(_extra_args: &[String]) -> Result<(), String> {
    Err("Axum adapter does not define a deploy command. Extend your workspace manifest with one if needed.".into())
}

struct AxumProject {
    crate_dir: PathBuf,
    cargo_manifest: PathBuf,
    crate_name: String,
    port: u16,
}

fn locate_project() -> Result<AxumProject, String> {
    let cwd = std::env::current_dir().map_err(|err| err.to_string())?;
    let manifest = find_axum_manifest(&cwd)?;
    read_axum_project(&manifest)
}

fn run_cargo(project: &AxumProject, subcommand: &str, extra_args: &[String]) -> Result<(), String> {
    let display = project.crate_dir.display();
    println!(
        "[edgezero] Axum {subcommand} ({}) in {} (port: {})",
        project.crate_name, display, project.port
    );
    let mut command = Command::new("cargo");
    command.arg(subcommand);
    command.arg("--manifest-path");
    command.arg(
        project
            .cargo_manifest
            .to_str()
            .ok_or_else(|| format!("invalid manifest path {}", project.cargo_manifest.display()))?,
    );
    command.args(extra_args);
    command.current_dir(&project.crate_dir);
    let status = command
        .status()
        .map_err(|err| format!("failed to run cargo {subcommand}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {subcommand} failed with status {}", status))
    }
}

fn find_axum_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "axum.toml") {
        return Ok(found);
    }

    let root = find_workspace_root(start);
    let mut candidates: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(true)
        .max_depth(8)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.file_name()
                .map(|name| name == "axum.toml")
                .unwrap_or(false)
                && path
                    .parent()
                    .map(|dir| dir.join("Cargo.toml").exists())
                    .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate axum.toml".into());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn read_axum_project(manifest: &Path) -> Result<AxumProject, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|err| format!("failed to read {}: {err}", manifest.display()))?;
    let value: Value = toml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.display()))?;

    let adapter = value
        .get("adapter")
        .and_then(Value::as_table)
        .ok_or_else(|| format!("adapter table missing in {}", manifest.display()))?;

    let crate_dir = adapter
        .get("crate_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("adapter.crate_dir missing in {}", manifest.display()))?;

    let manifest_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let crate_dir = manifest_dir.join(crate_dir);
    let cargo_manifest = crate_dir.join("Cargo.toml");
    if !cargo_manifest.exists() {
        return Err(format!(
            "Cargo.toml missing at {} referenced by {}",
            cargo_manifest.display(),
            manifest.display()
        ));
    }

    let crate_name = adapter
        .get("crate")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            read_package_name(&cargo_manifest).unwrap_or_else(|_| {
                crate_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("axum-adapter")
                    .to_string()
            })
        });

    let port = match adapter.get("port").and_then(Value::as_integer) {
        Some(value) => {
            if !(1..=u16::MAX as i64).contains(&value) {
                return Err(format!(
                    "adapter.port in {} must be between 1 and 65535",
                    manifest.display()
                ));
            }
            value as u16
        }
        None => 8787,
    };

    Ok(AxumProject {
        crate_dir,
        cargo_manifest,
        crate_name,
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::find_manifest_upwards;
    use tempfile::tempdir;

    #[test]
    fn read_axum_project_loads_defaults() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.crate_name, "demo");
        assert_eq!(project.crate_dir, root);
        assert_eq!(project.cargo_manifest, root.join("Cargo.toml"));
        assert_eq!(project.port, 8787);
    }

    #[test]
    fn find_manifest_upwards_locates_parent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("nested/level");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        let found = find_manifest_upwards(&nested, "axum.toml").expect("manifest");
        assert_eq!(found, root.join("axum.toml"));
    }

    #[test]
    fn read_axum_project_uses_custom_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 4001\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.port, 4001);
    }

    #[test]
    fn read_axum_project_rejects_invalid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 70000\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project(&root.join("axum.toml"));
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("must be between 1 and 65535")),
        }
    }

    #[test]
    fn read_axum_project_rejects_zero_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 0\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project(&root.join("axum.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn read_axum_project_rejects_negative_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = -1\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project(&root.join("axum.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn read_axum_project_rejects_missing_adapter_table() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("axum.toml"), "[other]\nkey = \"value\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project(&root.join("axum.toml"));
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("adapter table missing")),
        }
    }

    #[test]
    fn read_axum_project_rejects_missing_crate_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("axum.toml"), "[adapter]\ncrate = \"demo\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project(&root.join("axum.toml"));
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("crate_dir missing")),
        }
    }

    #[test]
    fn read_axum_project_rejects_missing_cargo_toml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \"subdir\"\n",
        )
        .unwrap();
        // No Cargo.toml in subdir

        let result = read_axum_project(&root.join("axum.toml"));
        match result {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.contains("Cargo.toml missing")),
        }
    }

    #[test]
    fn read_axum_project_falls_back_to_package_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No crate key in adapter table
        fs::write(root.join("axum.toml"), "[adapter]\ncrate_dir = \".\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-package\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.crate_name, "my-package");
    }

    #[test]
    fn read_axum_project_with_relative_crate_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let adapter_dir = root.join("crates/my-adapter");
        fs::create_dir_all(&adapter_dir).unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"my-adapter\"\ncrate_dir = \"crates/my-adapter\"\n",
        )
        .unwrap();
        fs::write(
            adapter_dir.join("Cargo.toml"),
            "[package]\nname = \"my-adapter\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.crate_name, "my-adapter");
        assert_eq!(project.crate_dir, adapter_dir);
    }

    #[test]
    fn read_axum_project_accepts_max_valid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 65535\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.port, 65535);
    }

    #[test]
    fn read_axum_project_accepts_min_valid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 1\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project = read_axum_project(&root.join("axum.toml")).expect("project");
        assert_eq!(project.port, 1);
    }

    #[test]
    fn find_axum_manifest_returns_error_when_not_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Create an empty directory with a Cargo.toml but no axum.toml
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let result = find_axum_manifest(root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("could not locate axum.toml"));
    }

    #[test]
    fn find_axum_manifest_finds_in_current_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        let found = find_axum_manifest(root).expect("manifest");
        assert_eq!(found, root.join("axum.toml"));
    }

    #[test]
    fn find_axum_manifest_finds_closest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("level1/level2");
        fs::create_dir_all(&nested).unwrap();

        // Create axum.toml at root
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"root\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        // Create axum.toml at level1
        fs::write(
            root.join("level1/Cargo.toml"),
            "[package]\nname = \"level1\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("level1/axum.toml"),
            "[adapter]\ncrate = \"level1\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        // Search from level2, should find level1's axum.toml (closer)
        let found = find_axum_manifest(&nested).expect("manifest");
        assert_eq!(found, root.join("level1/axum.toml"));
    }

    #[test]
    fn deploy_returns_error() {
        let result = deploy(&[]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("does not define a deploy command"));
    }

    #[test]
    fn adapter_name_is_axum() {
        assert_eq!(AXUM_ADAPTER.name(), "axum");
    }

    #[test]
    fn blueprint_has_correct_id() {
        assert_eq!(AXUM_BLUEPRINT.id, "axum");
        assert_eq!(AXUM_BLUEPRINT.display_name, "Axum");
    }
}
