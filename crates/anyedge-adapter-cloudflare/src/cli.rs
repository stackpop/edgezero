use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyedge_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use anyedge_adapter::{register_adapter, Adapter, AdapterAction};
use ctor::ctor;
use walkdir::WalkDir;

const TARGET_TRIPLE: &str = "wasm32-unknown-unknown";

pub fn build() -> Result<PathBuf, String> {
    let manifest = find_wrangler_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_string())?;
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
    let manifest = find_wrangler_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_string())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_string())?;

    let status = Command::new("wrangler")
        .args(["deploy", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run wrangler CLI: {e}"))?;
    if !status.success() {
        return Err(format!("wrangler deploy failed with status {status}"));
    }

    Ok(())
}

pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest = find_wrangler_manifest(
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .as_path(),
    )?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_string())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_string())?;

    let status = Command::new("wrangler")
        .args(["dev", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run wrangler CLI: {e}"))?;
    if !status.success() {
        return Err(format!("wrangler dev failed with status {status}"));
    }

    Ok(())
}

struct CloudflareCliAdapter;

static CLOUDFLARE_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "cf_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_wrangler_toml",
        contents: include_str!("templates/wrangler.toml.hbs"),
    },
];

static CLOUDFLARE_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "cf_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "cf_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "cf_cargo_config_toml",
        output: ".cargo/config.toml",
    },
    AdapterFileSpec {
        template: "cf_wrangler_toml",
        output: "wrangler.toml",
    },
];

static CLOUDFLARE_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_anyedge_core_cloudflare",
        repo_crate: "crates/anyedge-core",
        fallback: "anyedge-core = { git = \"ssh://git@github.com/stackpop/anyedge.git\", package = \"anyedge-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_anyedge_adapter_cloudflare",
        repo_crate: "crates/anyedge-adapter-cloudflare",
        fallback:
            "anyedge-adapter-cloudflare = { git = \"ssh://git@github.com/stackpop/anyedge.git\", package = \"anyedge-adapter-cloudflare\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_anyedge_adapter_cloudflare_wasm",
        repo_crate: "crates/anyedge-adapter-cloudflare",
        fallback:
            "anyedge-adapter-cloudflare = { git = \"ssh://git@github.com/stackpop/anyedge.git\", package = \"anyedge-adapter-cloudflare\", default-features = false, features = [\"cloudflare\"] }",
        features: &["cloudflare"],
    },
];

static CLOUDFLARE_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "cloudflare",
    display_name: "Cloudflare Workers",
    crate_suffix: "adapter-cloudflare",
    dependency_crate: "anyedge-adapter-cloudflare",
    dependency_repo_path: "crates/anyedge-adapter-cloudflare",
    template_registrations: CLOUDFLARE_TEMPLATE_REGISTRATIONS,
    files: CLOUDFLARE_FILE_SPECS,
    extra_dirs: &["src", ".cargo"],
    dependencies: CLOUDFLARE_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "wrangler.toml",
        build_target: "wasm32-unknown-unknown",
        build_profile: "release",
        build_features: &["cloudflare"],
    },
    commands: CommandTemplates {
        build: "cargo build --release --target wasm32-unknown-unknown -p {crate}",
        serve: "wrangler dev --config {crate_dir}/wrangler.toml",
        deploy: "wrangler publish --config {crate_dir}/wrangler.toml",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["cd {crate_dir}", "wrangler dev"],
    },
    run_module: "anyedge_adapter_cloudflare",
};

static CLOUDFLARE_ADAPTER: CloudflareCliAdapter = CloudflareCliAdapter;

impl Adapter for CloudflareCliAdapter {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            AdapterAction::Build => build().map(|artifact| {
                println!(
                    "[anyedge] Cloudflare build artifact -> {}",
                    artifact.display()
                );
            }),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
        }
    }
}

pub fn register() {
    register_adapter(&CLOUDFLARE_ADAPTER);
    register_adapter_blueprint(&CLOUDFLARE_BLUEPRINT);
}

#[ctor]
fn register_ctor() {
    register();
}

fn find_wrangler_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start) {
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
                .map(|n| n == "wrangler.toml")
                .unwrap_or(false)
                && path
                    .parent()
                    .map(|dir| dir.join("Cargo.toml").exists())
                    .unwrap_or(false)
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate wrangler.toml".to_string());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn find_manifest_upwards(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join("wrangler.toml");
        if candidate.exists() && dir.join("Cargo.toml").exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn read_package_name(manifest: &Path) -> Result<String, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|e| format!("failed to read {}: {e}", manifest.display()))?;
    let table: toml::Value = toml::from_str(&contents)
        .map_err(|e| format!("failed to parse {}: {e}", manifest.display()))?;
    if let Some(name) = table
        .get("package")
        .and_then(|pkg| pkg.get("name"))
        .and_then(|name| name.as_str())
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

fn find_workspace_root(dir: &Path) -> PathBuf {
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
        "compiled artifact not found for {} (looked in manifest and workspace target directories)",
        crate_name
    ))
}

fn path_distance(a: &Path, b: &Path) -> usize {
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
