use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{register_adapter, Adapter, AdapterAction, ProvisionStores};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use walkdir::WalkDir;

static SPIN_ADAPTER: SpinCliAdapter = SpinCliAdapter;

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
        build: "cargo build --target wasm32-wasip1 --release -p {crate}",
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
        dev_steps: &["`edgezero serve --adapter spin`"],
    },
    run_module: "edgezero_adapter_spin",
};

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

const TARGET_TRIPLE: &str = "wasm32-wasip1";

const SPIN_INSTALL_HINT: &str = "install the Spin CLI (https://spinframework.dev/) and try again";

struct SpinCliAdapter;

impl Adapter for SpinCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `spin cloud {login|logout|info}` is the native sign-in
            // surface for Fermyon Cloud. EdgeZero stores no
            // credentials — this is a thin shell-out (spec §11).
            AdapterAction::AuthLogin => {
                run_native_cli("spin", &["cloud", "login"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("spin", &["cloud", "logout"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("spin", &["cloud", "info"], SPIN_INSTALL_HINT)
            }
            AdapterAction::Build => {
                let artifact = build(args)?;
                log::info!("[edgezero] Spin build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("spin adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "spin"
    }

    fn provision(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _stores: &ProvisionStores<'_>,
        _dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Stage 6.4 will edit spin.toml's `key_value_stores`
        // array on the resolved `[component.<component>]`. No
        // shell-out — Spin KV labels are runtime-resolved.
        Err("spin provision is not yet implemented; landing in a follow-up commit".to_owned())
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        // §6.7: Multi for KV (label-backed); Single for Config and
        // Secrets (flat-variable namespace).
        &["config", "secrets"]
    }

    fn validate_adapter_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
    ) -> Result<(), String> {
        // §6.7 check 3: spin.toml must exist and either declare
        // exactly one `[component.*]` or carry an explicit selector
        // that matches one of the declared ids.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for Spin component discovery".to_owned()
            );
        };
        let spin_path = manifest_root.join(rel);
        let raw = fs::read_to_string(&spin_path).map_err(|err| {
            format!(
                "failed to read spin manifest at {}: {err}",
                spin_path.display()
            )
        })?;
        let parsed: toml::Value = toml::from_str(&raw)
            .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
        let component_ids = collect_spin_component_ids(&parsed);

        if component_ids.is_empty() {
            return Err(format!(
                "{}: no [component.*] declarations found",
                spin_path.display()
            ));
        }

        if let Some(selector) = component_selector {
            if component_ids.iter().any(|id| id == selector) {
                return Ok(());
            }
            return Err(format!(
                "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
                selector,
                spin_path.display(),
                component_ids.join(", ")
            ));
        }

        if component_ids.len() == 1 {
            return Ok(());
        }
        Err(format!(
            "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
            spin_path.display(),
            component_ids.len(),
            component_ids.join(", ")
        ))
    }

    fn validate_app_config_keys(&self, keys: &[&str]) -> Result<(), String> {
        // §6.7 check 1: each dotted config key, translated `.→__`,
        // must match `^[a-z][a-z0-9_]*$` — Spin's flat variable
        // namespace has no other escaping.
        for key in keys {
            let spin_var = key.replace('.', "__");
            if !is_valid_spin_key(&spin_var) {
                return Err(format!(
                    "config key `{key}` translates to Spin variable `{spin_var}`, which does not match `^[a-z][a-z0-9_]*$`"
                ));
            }
        }
        Ok(())
    }

    fn validate_typed_secrets(
        &self,
        config_keys: &[&str],
        plain_secrets: &[(&str, &str)],
    ) -> Result<(), String> {
        // §6.7 check 2: flattened config keys ∪ `#[secret]` values
        // must be a unique set after `.→__` translation, since Spin
        // has one flat variable namespace. The CLI already filtered
        // out `#[secret(store_ref)]` entries (those are runtime
        // store ids, not Spin variables).
        let mut seen: HashSet<String> =
            HashSet::with_capacity(config_keys.len().saturating_add(plain_secrets.len()));
        for key in config_keys {
            let spin_var = key.replace('.', "__");
            if !seen.insert(spin_var.clone()) {
                return Err(format!(
                    "duplicate Spin variable `{spin_var}` derived from config key `{key}`"
                ));
            }
        }
        for (field_name, value) in plain_secrets {
            let spin_var = value.replace('.', "__");
            if !seen.insert(spin_var.clone()) {
                return Err(format!(
                    "Spin variable `{spin_var}` (from `#[secret]` field `{field_name}`) collides with a config key under the same name; Spin's flat variable namespace cannot disambiguate them"
                ));
            }
        }
        Ok(())
    }
}

fn is_valid_spin_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn collect_spin_component_ids(parsed: &toml::Value) -> Vec<String> {
    parsed
        .as_table()
        .and_then(|root| root.get("component"))
        .and_then(toml::Value::as_table)
        .map(|components| components.keys().cloned().collect())
        .unwrap_or_default()
}

/// # Errors
/// Returns an error if the Spin CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;
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
/// Returns an error if the Spin CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin deploy failed with status {status}"));
    }

    Ok(())
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
            path.file_name().is_some_and(|n| n == "spin.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate spin.toml".to_owned());
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
        "compiled artifact not found (looked in {} and workspace target)",
        manifest_dir.display()
    ))
}

#[inline]
pub fn register() {
    register_adapter(&SPIN_ADAPTER);
    register_adapter_blueprint(&SPIN_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Spin CLI up command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["up"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_valid_spin_key_accepts_lowercase_with_digits_and_underscores() {
        assert!(is_valid_spin_key("foo"));
        assert!(is_valid_spin_key("foo_bar"));
        assert!(is_valid_spin_key("foo__bar"));
        assert!(is_valid_spin_key("a1b2"));
    }

    #[test]
    fn is_valid_spin_key_rejects_bad_starts_and_chars() {
        assert!(!is_valid_spin_key(""));
        assert!(!is_valid_spin_key("FOO"));
        assert!(!is_valid_spin_key("1foo"));
        assert!(!is_valid_spin_key("foo-bar"));
        assert!(!is_valid_spin_key("_foo"));
    }

    #[test]
    fn validate_app_config_keys_rejects_uppercase() {
        let err = SpinCliAdapter
            .validate_app_config_keys(&["api_token", "GREETING"])
            .expect_err("uppercase key must error");
        assert!(
            err.contains("GREETING") && err.contains("Spin"),
            "error names the bad key + Spin: {err}"
        );
    }

    #[test]
    fn validate_app_config_keys_rejects_dashes() {
        let err = SpinCliAdapter
            .validate_app_config_keys(&["api-token"])
            .expect_err("dashed key must error");
        assert!(err.contains("api-token"), "error names the bad key: {err}");
    }

    #[test]
    fn validate_typed_secrets_detects_collision() {
        // `api_token = "greeting"` makes the config key `greeting`
        // and the Spin variable derived from the secret value
        // `greeting` collide (§6.7 check 2).
        let err = SpinCliAdapter
            .validate_typed_secrets(&["greeting"], &[("api_token", "greeting")])
            .expect_err("collision must error");
        assert!(
            err.contains("greeting") && err.contains("collides"),
            "error names the colliding name: {err}"
        );
    }

    #[test]
    fn validate_typed_secrets_passes_with_no_collision() {
        SpinCliAdapter
            .validate_typed_secrets(
                &["greeting", "service.timeout_ms"],
                &[("api_token", "demo_api_token")],
            )
            .expect("non-colliding inputs must pass");
    }

    #[test]
    fn validate_adapter_manifest_errors_on_zero_components() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), None)
            .expect_err("no [component.*] must error");
        assert!(
            err.contains("no [component.*]"),
            "error explains the absence: {err}"
        );
    }

    #[test]
    fn validate_adapter_manifest_rejects_bad_selector_against_single_component() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.actual]\nsource = \"a.wasm\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), Some("typo"))
            .expect_err("typo selector must error");
        assert!(
            err.contains("typo") && err.contains("actual"),
            "error names both the bad selector and the available id: {err}"
        );
    }

    #[test]
    fn single_store_kinds_is_config_and_secrets() {
        assert_eq!(SpinCliAdapter.single_store_kinds(), &["config", "secrets"]);
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
    fn locate_artifact_converts_hyphens_to_underscores() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("crates/my-cool-crate");
        fs::create_dir_all(&manifest_dir).unwrap();

        // Cargo emits underscored filenames for hyphenated crate names.
        let artifact = workspace.join("target/wasm32-wasip1/release/my_cool_crate.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "my-cool-crate").unwrap();
        assert_eq!(located, artifact);
    }
}
