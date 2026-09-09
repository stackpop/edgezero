use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use edgezero_adapter::cli_support::{find_workspace_root, path_distance};
use edgezero_adapter::registry::AdapterExecutionTarget;
use edgezero_core::manifest::{Manifest, ManifestLoader, ResolvedEnvironment};

use crate::adapter::Action;

pub(crate) enum ResolvedAdapterTarget {
    Registered(AdapterExecutionTarget),
    Shell(ResolvedShellTarget),
}

pub(crate) struct ResolvedManifest {
    loader: ManifestLoader,
    path: PathBuf,
}

pub(crate) struct ResolvedRuntime {
    action: Action,
    adapter: String,
    contract: Option<ResolvedManifest>,
    target: ResolvedAdapterTarget,
}

pub(crate) struct ResolvedShellTarget {
    bind_host: Option<String>,
    bind_port: Option<u16>,
    command: String,
    environment: ResolvedEnvironment,
    root: PathBuf,
}

impl ResolvedManifest {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "read-only accessor is part of the resolved-manifest contract and exercised by focused tests"
        )
    )]
    #[inline]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl ResolvedRuntime {
    #[inline]
    pub(crate) fn action(&self) -> Action {
        self.action
    }

    #[inline]
    pub(crate) fn adapter_name(&self) -> &str {
        &self.adapter
    }

    #[inline]
    pub(crate) fn manifest(&self) -> Option<&Manifest> {
        self.contract
            .as_ref()
            .map(|contract| contract.loader.manifest())
    }

    #[inline]
    pub(crate) fn target(&self) -> &ResolvedAdapterTarget {
        &self.target
    }
}

impl ResolvedShellTarget {
    #[inline]
    pub(crate) fn bind_host(&self) -> Option<&str> {
        self.bind_host.as_deref()
    }

    #[inline]
    pub(crate) fn bind_port(&self) -> Option<u16> {
        self.bind_port
    }

    #[inline]
    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    #[inline]
    pub(crate) fn environment(&self) -> &ResolvedEnvironment {
        &self.environment
    }

    #[inline]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

pub(crate) fn resolve_runtime(adapter: &str, action: Action) -> Result<ResolvedRuntime, String> {
    let invocation_dir = env::current_dir()
        .map_err(|error| format!("failed to read invocation directory: {error}"))?;
    resolve_runtime_from(
        adapter,
        action,
        &invocation_dir,
        env::var_os("EDGEZERO_MANIFEST"),
    )
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve {label} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "{label} {} is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve {label} {}: {error}", path.display()))?;
    if !canonical.is_file() {
        return Err(format!(
            "{label} {} is not a regular file",
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[expect(
    clippy::filetype_is_file,
    reason = "manifest discovery intentionally excludes symbolic links before canonicalization"
)]
fn collect_named_files(root: &Path, name: &str, depth: usize, output: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && path.file_name().is_some_and(|file_name| file_name == name) {
            output.push(path);
        } else if file_type.is_dir()
            && !matches!(
                path.file_name().and_then(|file_name| file_name.to_str()),
                Some(".git" | "target")
            )
        {
            collect_named_files(&path, name, depth.saturating_sub(1), output);
        } else {
            // Other files and excluded directories are not manifest candidates.
        }
    }
}

fn command_for(manifest: &Manifest, adapter: &str, action: Action) -> Option<String> {
    let (_canonical, config) = manifest.adapter_entry(adapter)?;
    match action {
        Action::Build => config.commands.build.clone(),
        Action::Deploy => config.commands.deploy.clone(),
        Action::Serve => config.commands.serve.clone(),
        Action::DeployStaged
        | Action::AuthLogin
        | Action::AuthLogout
        | Action::AuthStatus
        | Action::EmitVersion
        | Action::Healthcheck
        | Action::Rollback => None,
    }
}

fn discover_default_manifest(invocation_dir: &Path) -> Result<Option<PathBuf>, String> {
    for ancestor in invocation_dir.ancestors() {
        let candidate = ancestor.join("edgezero.toml");
        if candidate.is_file() {
            return canonical_regular_file(&candidate, "application manifest").map(Some);
        }
    }

    let workspace = canonical_directory(&find_workspace_root(invocation_dir), "workspace root")?;
    let mut candidates = Vec::new();
    collect_named_files(&workspace, "edgezero.toml", 9, &mut candidates);
    choose_nearest(invocation_dir, candidates, "application manifest")
}

fn discover_platform_manifest(
    adapter: &str,
    invocation_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let name = platform_manifest_name(adapter);
    for ancestor in invocation_dir.ancestors() {
        let candidate = ancestor.join(name);
        if candidate.is_file() && ancestor.join("Cargo.toml").is_file() {
            return canonical_regular_file(&candidate, "platform manifest").map(Some);
        }
    }
    let workspace = canonical_directory(&find_workspace_root(invocation_dir), "workspace root")?;
    let mut candidates = Vec::new();
    collect_named_files(&workspace, name, 9, &mut candidates);
    candidates.retain(|path| {
        path.parent()
            .is_some_and(|parent| parent.join("Cargo.toml").is_file())
    });
    choose_nearest(invocation_dir, candidates, "platform manifest")
}

fn choose_nearest(
    invocation_dir: &Path,
    candidates: Vec<PathBuf>,
    label: &str,
) -> Result<Option<PathBuf>, String> {
    let mut ranked = candidates
        .into_iter()
        .map(|path| {
            let parent = path.parent().unwrap_or(Path::new(""));
            (path_distance(invocation_dir, parent), path)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let Some((distance, selected)) = ranked.first() else {
        return Ok(None);
    };
    if let Some((_next_distance, next)) = ranked.get(1).filter(|next| next.0 == *distance) {
        return Err(format!(
            "ambiguous {label} selection: {} and {} are equally near the invocation directory",
            selected.display(),
            next.display()
        ));
    }
    canonical_regular_file(selected, label).map(Some)
}

fn load_resolved_manifest(path: PathBuf) -> Result<ResolvedManifest, String> {
    let loader = ManifestLoader::from_path(&path)
        .map_err(|error| format!("failed to load {}: {error}", path.display()))?;
    Ok(ResolvedManifest { loader, path })
}

fn platform_manifest_from_contract(
    manifest: &Manifest,
    adapter: &str,
    app_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let Some((_canonical, config)) = manifest.adapter_entry(adapter) else {
        return Err(format!(
            "adapter `{adapter}` is not configured in {}",
            app_root.join("edgezero.toml").display()
        ));
    };
    let Some(relative) = config.adapter.manifest.as_deref() else {
        return Ok(None);
    };
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        return Err(format!(
            "adapter `{adapter}` platform manifest must be relative to the application root"
        ));
    }
    let platform = canonical_regular_file(
        &app_root.join(relative_path),
        &format!("adapter `{adapter}` platform manifest"),
    )?;
    if !platform.starts_with(app_root) {
        return Err(format!(
            "adapter `{adapter}` platform manifest {} resolves outside application root {}",
            platform.display(),
            app_root.display()
        ));
    }
    Ok(Some(platform))
}

fn platform_manifest_name(adapter: &str) -> &'static str {
    if adapter.eq_ignore_ascii_case("axum") {
        "axum.toml"
    } else if adapter.eq_ignore_ascii_case("cloudflare") {
        "wrangler.toml"
    } else if adapter.eq_ignore_ascii_case("fastly") {
        "fastly.toml"
    } else if adapter.eq_ignore_ascii_case("spin") {
        "spin.toml"
    } else {
        "edgezero-platform.toml"
    }
}

fn produces_current_runtime(action: Action) -> bool {
    match action {
        Action::Build | Action::Deploy | Action::DeployStaged | Action::Serve => true,
        Action::AuthLogin
        | Action::AuthLogout
        | Action::AuthStatus
        | Action::EmitVersion
        | Action::Healthcheck
        | Action::Rollback => false,
    }
}

fn resolve_contract_path(
    invocation_dir: &Path,
    explicit: Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    if let Some(raw) = explicit {
        let configured = PathBuf::from(raw);
        let candidate = if configured.is_absolute() {
            configured
        } else {
            invocation_dir.join(configured)
        };
        return canonical_regular_file(&candidate, "explicit application manifest").map(Some);
    }
    discover_default_manifest(invocation_dir)
}

fn resolve_runtime_from(
    adapter: &str,
    action: Action,
    invocation_dir: &Path,
    explicit: Option<OsString>,
) -> Result<ResolvedRuntime, String> {
    if !produces_current_runtime(action) {
        return Err("operational action entered outbound runtime resolver".to_owned());
    }
    let canonical_invocation_dir = canonical_directory(invocation_dir, "invocation directory")?;
    let contract_path = resolve_contract_path(&canonical_invocation_dir, explicit)?;
    let contract = contract_path.map(load_resolved_manifest).transpose()?;

    if let Some(resolved_manifest) = contract {
        let manifest = resolved_manifest.loader.manifest();
        let (canonical_adapter, config) = manifest.adapter_entry(adapter).ok_or_else(|| {
            format!(
                "adapter `{adapter}` is not configured in {}",
                resolved_manifest.path.display()
            )
        })?;
        let app_root = canonical_directory(
            manifest
                .root()
                .ok_or_else(|| "resolved manifest has no application root".to_owned())?,
            "application root",
        )?;
        let adapter_name = canonical_adapter.to_ascii_lowercase();
        let target = if let Some(command) = command_for(manifest, &adapter_name, action) {
            ResolvedAdapterTarget::Shell(ResolvedShellTarget {
                bind_host: config.adapter.host.clone(),
                bind_port: config.adapter.port,
                command,
                environment: manifest.environment_for(&adapter_name),
                root: app_root,
            })
        } else {
            let platform_manifest =
                platform_manifest_from_contract(manifest, &adapter_name, &app_root)?;
            ResolvedAdapterTarget::Registered(AdapterExecutionTarget::new(
                app_root,
                config.adapter.component.clone(),
                platform_manifest,
            ))
        };
        return Ok(ResolvedRuntime {
            action,
            adapter: adapter_name,
            contract: Some(resolved_manifest),
            target,
        });
    }

    let platform_manifest = discover_platform_manifest(adapter, &canonical_invocation_dir)?;
    let app_root = platform_manifest
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(canonical_invocation_dir);
    Ok(ResolvedRuntime {
        action,
        adapter: adapter.to_ascii_lowercase(),
        contract: None,
        target: ResolvedAdapterTarget::Registered(AdapterExecutionTarget::new(
            app_root,
            None,
            platform_manifest,
        )),
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use tempfile::TempDir;

    use super::*;

    fn write_manifest(root: &Path, adapter: &str, platform: &str, command: Option<&str>) {
        fs::create_dir_all(root).expect("create app root");
        fs::write(root.join(platform), "# platform fixture\n").expect("write platform manifest");
        let command_section = command.map_or_else(String::new, |configured_command| {
            format!("\n[adapters.{adapter}.commands]\nbuild = {configured_command:?}\n")
        });
        fs::write(
            root.join("edgezero.toml"),
            format!(
                "[app]\nname = \"fixture\"\n[adapters.{adapter}.adapter]\nmanifest = {platform:?}\n{command_section}"
            ),
        )
        .expect("write app manifest");
        fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("write workspace manifest");
    }

    #[test]
    fn explicit_manifest_is_authoritative() {
        let temp = TempDir::new().expect("temp dir");
        let invocation = temp.path().join("invocation");
        let selected = temp.path().join("selected");
        write_manifest(&invocation, "axum", "axum.toml", None);
        write_manifest(&selected, "fastly", "fastly.toml", Some("printf selected"));

        let runtime = resolve_runtime_from(
            "fastly",
            Action::Build,
            &invocation,
            Some(OsString::from("../selected/edgezero.toml")),
        )
        .expect("resolve explicit manifest");
        let expected_path =
            fs::canonicalize(selected.join("edgezero.toml")).expect("canonical selected");
        assert_eq!(runtime.adapter_name(), "fastly");
        assert_eq!(
            runtime.contract.as_ref().map(ResolvedManifest::path),
            Some(expected_path.as_path())
        );
        assert!(matches!(runtime.target(), ResolvedAdapterTarget::Shell(_)));
    }

    #[test]
    fn runtime_resolver_rejects_cross_app_pair() {
        let temp = TempDir::new().expect("temp dir");
        let selected = temp.path().join("selected");
        let other = temp.path().join("other");
        fs::create_dir_all(&selected).expect("selected root");
        fs::create_dir_all(&other).expect("other root");
        fs::write(other.join("fastly.toml"), "# other\n").expect("platform");
        fs::write(selected.join("Cargo.toml"), "[workspace]\n").expect("cargo");
        fs::write(
            selected.join("edgezero.toml"),
            "[app]\nname = \"fixture\"\n[adapters.fastly.adapter]\nmanifest = \"../other/fastly.toml\"\n",
        )
        .expect("manifest");

        let result = resolve_runtime_from(
            "fastly",
            Action::Build,
            &selected,
            Some(OsString::from("edgezero.toml")),
        );
        assert!(result.is_err_and(|error| error.contains("outside application root")));
    }

    #[test]
    fn runtime_resolver_rejects_equal_distance_ambiguity() {
        let temp = TempDir::new().expect("temp dir");
        fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").expect("workspace");
        write_manifest(&temp.path().join("left"), "axum", "axum.toml", None);
        write_manifest(&temp.path().join("right"), "axum", "axum.toml", None);

        let result = resolve_runtime_from("axum", Action::Build, temp.path(), None);
        assert!(result.is_err_and(|error| error.contains("ambiguous application manifest")));
    }

    #[test]
    fn runtime_resolver_rejects_operational_action() {
        let temp = TempDir::new().expect("temp dir");
        let result = resolve_runtime_from("axum", Action::AuthStatus, temp.path(), None);
        assert!(result.is_err_and(|error| error.contains("operational action")));
    }
}
