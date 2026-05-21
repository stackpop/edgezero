use std::env;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use edgezero_adapter::registry::{register_adapter, Adapter, AdapterAction};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use edgezero_core::addr;
use edgezero_core::manifest::ManifestLoader;
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
            "`cargo run` or `edgezero serve --adapter axum`",
        ],
    },
    run_module: "edgezero_adapter_axum",
};

static AXUM_ADAPTER: AxumCliAdapter = AxumCliAdapter;

struct AxumCliAdapter;

#[derive(Debug)]
struct AxumProject {
    addr: SocketAddr,
    axum_host: Option<String>,
    axum_manifest: PathBuf,
    axum_port: Option<u16>,
    cargo_manifest: PathBuf,
    crate_dir: PathBuf,
    crate_name: String,
    env_host: Option<String>,
    env_port: Option<String>,
}

#[derive(Debug, Default)]
struct EdgezeroAxumConfig {
    host: Option<String>,
    port: Option<u16>,
}

impl Adapter for AxumCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            AdapterAction::Build => build(args),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("axum adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "axum"
    }
}

#[inline]
pub fn register() {
    register_adapter(&AXUM_ADAPTER);
    register_adapter_blueprint(&AXUM_BLUEPRINT);
}

#[ctor(unsafe)]
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

fn locate_project() -> Result<AxumProject, String> {
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    let manifest = find_axum_manifest(&cwd)?;
    read_axum_project(&manifest)
}

fn run_cargo(project: &AxumProject, subcommand: &str, extra_args: &[String]) -> Result<(), String> {
    let resolution = resolve_subprocess_addr(project)?;
    for warning in &resolution.warnings {
        log::warn!("[edgezero] {warning}");
    }

    let bind_addr = resolution.addr;
    let display = project.crate_dir.display();
    log::info!(
        "[edgezero] Axum {subcommand} ({}) in {display} ({bind_addr})",
        project.crate_name
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
    command.env("EDGEZERO_HOST", bind_addr.ip().to_string());
    command.env("EDGEZERO_PORT", bind_addr.port().to_string());
    let status = command
        .status()
        .map_err(|err| format!("failed to run cargo {subcommand}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {subcommand} failed with status {status}"))
    }
}

fn resolve_subprocess_addr(project: &AxumProject) -> Result<addr::BindAddrResolution, String> {
    let axum_only = resolve_subprocess_addr_from_parts(
        project.env_host.as_deref(),
        project.env_port.as_deref(),
        None,
        None,
        project.axum_host.as_deref(),
        project.axum_port,
    );
    debug_assert_eq!(
        project.addr, axum_only.addr,
        "cached AxumProject.addr must match a fresh axum-only resolution"
    );

    let edgezero = load_edgezero_axum_config(&project.axum_manifest)?;
    Ok(resolve_subprocess_addr_from_parts(
        project.env_host.as_deref(),
        project.env_port.as_deref(),
        edgezero.as_ref().and_then(|cfg| cfg.host.as_deref()),
        edgezero.as_ref().and_then(|cfg| cfg.port),
        project.axum_host.as_deref(),
        project.axum_port,
    ))
}

fn resolve_subprocess_addr_from_parts(
    env_host: Option<&str>,
    env_port: Option<&str>,
    edgezero_host: Option<&str>,
    edgezero_port: Option<u16>,
    axum_host: Option<&str>,
    axum_port: Option<u16>,
) -> addr::BindAddrResolution {
    let mut warnings = Vec::new();
    let host = resolve_subprocess_host(env_host, edgezero_host, axum_host, &mut warnings);
    let port = resolve_subprocess_port(env_port, edgezero_port, axum_port, &mut warnings);

    addr::BindAddrResolution {
        addr: SocketAddr::from((host, port)),
        warnings,
    }
}

fn resolve_subprocess_host(
    env_host: Option<&str>,
    edgezero_host: Option<&str>,
    axum_host: Option<&str>,
    warnings: &mut Vec<String>,
) -> IpAddr {
    if let Some(value) = env_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "EDGEZERO_HOST={value:?} is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    if let Some(value) = edgezero_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "configured host={value:?} in edgezero.toml is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    if let Some(value) = axum_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "configured host={value:?} in axum.toml is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    addr::DEFAULT_HOST
}

fn resolve_subprocess_port(
    env_port: Option<&str>,
    edgezero_port: Option<u16>,
    axum_port: Option<u16>,
    warnings: &mut Vec<String>,
) -> u16 {
    if let Some(value) = env_port {
        match value.parse::<u16>() {
            Ok(0) => warnings.push(
                "EDGEZERO_PORT=\"0\" is not supported (would bind to a random OS port); falling back".to_owned(),
            ),
            Ok(port) => return port,
            Err(_) => warnings.push(format!(
                "EDGEZERO_PORT={value:?} is not a valid port number; falling back"
            )),
        }
    }

    match edgezero_port {
        Some(0) => warnings.push(
            "configured port=0 in edgezero.toml is not supported (would bind to a random OS port); falling back".to_owned(),
        ),
        Some(port) => return port,
        None => {}
    }

    match axum_port {
        Some(0) => warnings.push(
            "configured port=0 in axum.toml is not supported (would bind to a random OS port); falling back".to_owned(),
        ),
        Some(port) => return port,
        None => {}
    }

    addr::DEFAULT_PORT
}

fn load_edgezero_axum_config(axum_manifest: &Path) -> Result<Option<EdgezeroAxumConfig>, String> {
    let Some(start_dir) = axum_manifest.parent() else {
        return Ok(None);
    };

    let Some(manifest_path) = find_manifest_upwards(start_dir, "edgezero.toml") else {
        return Ok(None);
    };

    let manifest = ManifestLoader::from_path(&manifest_path)
        .map_err(|err| format!("failed to load {}: {err}", manifest_path.display()))?;
    let Some(adapter) = manifest.manifest().adapters.get("axum") else {
        return Ok(None);
    };

    Ok(Some(EdgezeroAxumConfig {
        host: adapter.adapter.host.clone(),
        port: adapter.adapter.port,
    }))
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
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "axum.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
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
    let env_host = env::var("EDGEZERO_HOST").ok();
    let env_port = env::var("EDGEZERO_PORT").ok();
    read_axum_project_with_env(manifest, env_host.as_deref(), env_port.as_deref())
}

fn read_axum_project_with_env(
    manifest: &Path,
    env_host: Option<&str>,
    env_port: Option<&str>,
) -> Result<AxumProject, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|err| format!("failed to read {}: {err}", manifest.display()))?;
    let value: Value = toml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.display()))?;

    let adapter = value
        .get("adapter")
        .and_then(Value::as_table)
        .ok_or_else(|| format!("adapter table missing in {}", manifest.display()))?;

    let crate_dir_rel = adapter
        .get("crate_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("adapter.crate_dir missing in {}", manifest.display()))?;

    let manifest_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let crate_dir = manifest_dir.join(crate_dir_rel);
    let cargo_manifest = crate_dir.join("Cargo.toml");
    if !cargo_manifest.exists() {
        return Err(format!(
            "Cargo.toml missing at {} referenced by {}",
            cargo_manifest.display(),
            manifest.display()
        ));
    }

    let crate_name = adapter.get("crate").and_then(Value::as_str).map_or_else(
        || {
            read_package_name(&cargo_manifest).unwrap_or_else(|_| {
                crate_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("axum-adapter")
                    .to_owned()
            })
        },
        ToOwned::to_owned,
    );

    let config_host = adapter
        .get("host")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let config_port = match adapter.get("port").and_then(Value::as_integer) {
        Some(port_value) => Some(u16::try_from(port_value).map_err(|err| {
            format!(
                "adapter.port in {} must be between 0 and 65535 ({err})",
                manifest.display()
            )
        })?),
        None => None,
    };

    let resolution =
        addr::resolve_bind_addr(env_host, env_port, config_host.as_deref(), config_port);
    for warning in &resolution.warnings {
        log::warn!("[edgezero] {warning} (in {})", manifest.display());
    }

    Ok(AxumProject {
        addr: resolution.addr,
        axum_host: config_host,
        axum_manifest: manifest.to_path_buf(),
        axum_port: config_port,
        cargo_manifest,
        crate_dir,
        crate_name,
        env_host: env_host.map(str::to_owned),
        env_port: env_port.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::find_manifest_upwards;
    use std::net::Ipv6Addr;
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.crate_name, "demo");
        assert_eq!(project.crate_dir, root);
        assert_eq!(project.cargo_manifest, root.join("Cargo.toml"));
        assert_eq!(project.addr.port(), addr::DEFAULT_PORT);
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), 4001);
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

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("must be between 0 and 65535")),
        }
    }

    #[test]
    fn read_axum_project_zero_port_falls_back_to_default() {
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), addr::DEFAULT_PORT);
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

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        result.unwrap_err();
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

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("adapter table missing")),
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

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("crate_dir missing")),
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

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("Cargo.toml missing")),
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), u16::MAX);
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), 1);
    }

    #[test]
    fn read_axum_project_defaults_host_to_localhost() {
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

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
    }

    #[test]
    fn read_axum_project_uses_custom_host() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"0.0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), IpAddr::from([0, 0, 0, 0]));
    }

    #[test]
    fn read_axum_project_invalid_host_falls_back_to_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"not-an-ip\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
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
    fn read_axum_project_env_overrides_config() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"127.0.0.1\"\nport = 3000\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result =
            read_axum_project_with_env(&root.join("axum.toml"), Some("0.0.0.0"), Some("9999"));

        let project = result.expect("project");
        assert_eq!(project.addr.ip(), IpAddr::from([0, 0, 0, 0]));
        assert_eq!(project.addr.port(), 9999);
    }

    #[test]
    fn resolve_subprocess_addr_prefers_edgezero_manifest_over_axum_manifest() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("0.0.0.0"),
            Some(4000),
            Some("127.0.0.1"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 4000)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_falls_back_to_axum_manifest_when_edgezero_missing() {
        let resolution =
            resolve_subprocess_addr_from_parts(None, None, None, None, Some("0.0.0.0"), Some(3000));

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 3000)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_env_overrides_both_manifests() {
        let resolution = resolve_subprocess_addr_from_parts(
            Some("::1"),
            Some("9000"),
            Some("0.0.0.0"),
            Some(4000),
            Some("127.0.0.1"),
            Some(3000),
        );

        assert_eq!(
            resolution.addr,
            SocketAddr::from((Ipv6Addr::LOCALHOST, 9000))
        );
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_invalid_edgezero_host_falls_back_to_axum_host() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("invalid-host"),
            Some(4000),
            Some("0.0.0.0"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 4000)));
        assert_eq!(resolution.warnings.len(), 1);
    }

    #[test]
    fn resolve_subprocess_addr_edgezero_zero_port_falls_back_to_axum_port() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("127.0.0.1"),
            Some(0),
            Some("0.0.0.0"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([127, 0, 0, 1], 3000)));
        assert_eq!(resolution.warnings.len(), 1);
    }

    #[test]
    fn blueprint_has_correct_id() {
        assert_eq!(AXUM_BLUEPRINT.id, "axum");
        assert_eq!(AXUM_BLUEPRINT.display_name, "Axum");
    }
}
