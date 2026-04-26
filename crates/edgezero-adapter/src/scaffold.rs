use std::collections::HashMap;
use std::sync::{LazyLock, PoisonError, RwLock};

static BLUEPRINT_REGISTRY: LazyLock<RwLock<HashMap<String, &'static AdapterBlueprint>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Complete blueprint describing how the CLI should scaffold the adapter.
pub struct AdapterBlueprint {
    pub commands: CommandTemplates,
    pub crate_suffix: &'static str,
    pub dependencies: &'static [DependencySpec],
    pub dependency_crate: &'static str,
    pub dependency_repo_path: &'static str,
    pub display_name: &'static str,
    pub extra_dirs: &'static [&'static str],
    pub files: &'static [AdapterFileSpec],
    pub id: &'static str,
    pub logging: LoggingDefaults,
    pub manifest: ManifestSpec,
    pub readme: ReadmeInfo,
    pub run_module: &'static str,
    pub template_registrations: &'static [TemplateRegistration],
}

/// Specifies which template renders to a given adapter-relative output file.
#[derive(Clone, Copy)]
pub struct AdapterFileSpec {
    pub output: &'static str,
    pub template: &'static str,
}

/// Defines CLI command templates for adapter actions.
#[derive(Clone, Copy)]
pub struct CommandTemplates {
    pub build: &'static str,
    pub deploy: &'static str,
    pub serve: &'static str,
}

/// Describes a dependency entry inserted into an adapter crate manifest.
#[derive(Clone, Copy)]
pub struct DependencySpec {
    pub fallback: &'static str,
    pub features: &'static [&'static str],
    pub key: &'static str,
    pub repo_crate: &'static str,
}

/// Specifies default logging configuration for a scaffolded adapter crate.
#[derive(Clone, Copy)]
pub struct LoggingDefaults {
    pub echo_stdout: Option<bool>,
    pub endpoint: Option<&'static str>,
    pub level: &'static str,
}

/// Provides manifest and build configuration defaults for an adapter.
#[derive(Clone, Copy)]
pub struct ManifestSpec {
    pub build_features: &'static [&'static str],
    pub build_profile: &'static str,
    pub build_target: &'static str,
    pub manifest_filename: &'static str,
}

/// Supplies README snippets inserted for an adapter when scaffolding.
#[derive(Clone, Copy)]
pub struct ReadmeInfo {
    pub description: &'static str,
    pub dev_heading: &'static str,
    pub dev_steps: &'static [&'static str],
}

/// Static handlebars template registration provided by an adapter.
#[derive(Clone, Copy)]
pub struct TemplateRegistration {
    pub contents: &'static str,
    pub name: &'static str,
}

/// Registers the blueprint for an adapter. Latest registration wins.
#[inline]
pub fn register_adapter_blueprint(blueprint: &'static AdapterBlueprint) {
    let mut registry = BLUEPRINT_REGISTRY
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    registry.insert(blueprint.id.to_ascii_lowercase(), blueprint);
}

/// Returns the known adapter blueprints sorted by adapter id.
#[inline]
pub fn registered_blueprints() -> Vec<&'static AdapterBlueprint> {
    let registry = BLUEPRINT_REGISTRY
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    let mut values: Vec<&'static AdapterBlueprint> = registry.values().copied().collect();
    values.sort_by(|left, right| left.id.cmp(right.id));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static BLUEPRINT_ALPHA: AdapterBlueprint = AdapterBlueprint {
        commands: CommandTemplates {
            build: "build",
            deploy: "deploy",
            serve: "serve",
        },
        crate_suffix: "adapter-alpha",
        dependencies: &[DependencySpec {
            fallback: "alpha = \"0.1\"",
            features: &[],
            key: "dep_alpha",
            repo_crate: "crates/alpha",
        }],
        dependency_crate: "edgezero-adapter-alpha",
        dependency_repo_path: "crates/edgezero-adapter-alpha",
        display_name: "Alpha",
        extra_dirs: &["src"],
        files: &[AdapterFileSpec {
            output: "Cargo.toml",
            template: "first",
        }],
        id: "alpha",
        logging: LoggingDefaults {
            echo_stdout: Some(true),
            endpoint: Some("stdout"),
            level: "info",
        },
        manifest: ManifestSpec {
            build_features: &[],
            build_profile: "release",
            build_target: "wasm32",
            manifest_filename: "alpha.toml",
        },
        readme: ReadmeInfo {
            description: "desc",
            dev_heading: "heading",
            dev_steps: &["step"],
        },
        run_module: "module",
        template_registrations: &[FIRST_TEMPLATE],
    };

    static BLUEPRINT_BETA: AdapterBlueprint = AdapterBlueprint {
        commands: CommandTemplates {
            build: "build",
            deploy: "deploy",
            serve: "serve",
        },
        crate_suffix: "adapter-beta",
        dependencies: &[],
        dependency_crate: "edgezero-adapter-beta",
        dependency_repo_path: "crates/edgezero-adapter-beta",
        display_name: "Beta",
        extra_dirs: &[],
        files: &[AdapterFileSpec {
            output: "src/main.rs",
            template: "second",
        }],
        id: "beta",
        logging: LoggingDefaults {
            echo_stdout: None,
            endpoint: None,
            level: "info",
        },
        manifest: ManifestSpec {
            build_features: &[],
            build_profile: "release",
            build_target: "wasm32",
            manifest_filename: "beta.toml",
        },
        readme: ReadmeInfo {
            description: "desc",
            dev_heading: "heading",
            dev_steps: &[],
        },
        run_module: "module",
        template_registrations: &[SECOND_TEMPLATE],
    };

    static FIRST_TEMPLATE: TemplateRegistration = TemplateRegistration {
        contents: "a",
        name: "first",
    };

    static SECOND_TEMPLATE: TemplateRegistration = TemplateRegistration {
        contents: "b",
        name: "second",
    };

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn latest_blueprint_wins() {
        let _guard = TEST_LOCK.lock().expect("lock");
        super::BLUEPRINT_REGISTRY.write().expect("lock").clear();
        register_adapter_blueprint(&BLUEPRINT_ALPHA);
        register_adapter_blueprint(&BLUEPRINT_ALPHA);
        let blueprints = registered_blueprints();
        assert_eq!(blueprints.len(), 1);
        assert_eq!(blueprints[0].id, "alpha");
    }

    #[test]
    fn registered_blueprints_sorted() {
        let _guard = TEST_LOCK.lock().expect("lock");
        super::BLUEPRINT_REGISTRY.write().expect("lock").clear();
        register_adapter_blueprint(&BLUEPRINT_BETA);
        register_adapter_blueprint(&BLUEPRINT_ALPHA);
        let ids: Vec<&'static str> = registered_blueprints()
            .into_iter()
            .map(|bp| bp.id)
            .collect();
        assert_eq!(ids, vec!["alpha", "beta"]);
    }
}
