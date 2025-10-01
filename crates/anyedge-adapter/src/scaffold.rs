use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::RwLock;

/// Static handlebars template registration provided by an adapter.
#[derive(Clone, Copy)]
pub struct TemplateRegistration {
    pub name: &'static str,
    pub contents: &'static str,
}

/// Specifies which template renders to a given adapter-relative output file.
#[derive(Clone, Copy)]
pub struct AdapterFileSpec {
    pub template: &'static str,
    pub output: &'static str,
}

/// Describes a dependency entry inserted into an adapter crate manifest.
#[derive(Clone, Copy)]
pub struct DependencySpec {
    pub key: &'static str,
    pub repo_crate: &'static str,
    pub fallback: &'static str,
    pub features: &'static [&'static str],
}

/// Provides manifest and build configuration defaults for an adapter.
#[derive(Clone, Copy)]
pub struct ManifestSpec {
    pub manifest_filename: &'static str,
    pub build_target: &'static str,
    pub build_profile: &'static str,
    pub build_features: &'static [&'static str],
}

/// Defines CLI command templates for adapter actions.
#[derive(Clone, Copy)]
pub struct CommandTemplates {
    pub build: &'static str,
    pub serve: &'static str,
    pub deploy: &'static str,
}

/// Specifies default logging configuration for a scaffolded adapter crate.
#[derive(Clone, Copy)]
pub struct LoggingDefaults {
    pub endpoint: Option<&'static str>,
    pub level: &'static str,
    pub echo_stdout: Option<bool>,
}

/// Supplies README snippets inserted for an adapter when scaffolding.
#[derive(Clone, Copy)]
pub struct ReadmeInfo {
    pub description: &'static str,
    pub dev_heading: &'static str,
    pub dev_steps: &'static [&'static str],
}

/// Complete blueprint describing how the CLI should scaffold the adapter.
pub struct AdapterBlueprint {
    pub id: &'static str,
    pub display_name: &'static str,
    pub crate_suffix: &'static str,
    pub dependency_crate: &'static str,
    pub dependency_repo_path: &'static str,
    pub template_registrations: &'static [TemplateRegistration],
    pub files: &'static [AdapterFileSpec],
    pub extra_dirs: &'static [&'static str],
    pub dependencies: &'static [DependencySpec],
    pub manifest: ManifestSpec,
    pub commands: CommandTemplates,
    pub logging: LoggingDefaults,
    pub readme: ReadmeInfo,
    pub run_module: &'static str,
}

static BLUEPRINT_REGISTRY: Lazy<RwLock<HashMap<String, &'static AdapterBlueprint>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Registers the blueprint for an adapter. Latest registration wins.
pub fn register_adapter_blueprint(blueprint: &'static AdapterBlueprint) {
    let mut registry = BLUEPRINT_REGISTRY
        .write()
        .expect("anyedge blueprint registry lock poisoned");
    registry.insert(blueprint.id.to_ascii_lowercase(), blueprint);
}

/// Returns the known adapter blueprints sorted by adapter id.
pub fn registered_blueprints() -> Vec<&'static AdapterBlueprint> {
    let registry = BLUEPRINT_REGISTRY
        .read()
        .expect("anyedge blueprint registry lock poisoned");
    let mut values: Vec<&'static AdapterBlueprint> = registry.values().copied().collect();
    values.sort_by(|a, b| a.id.cmp(b.id));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static FIRST_TEMPLATE: TemplateRegistration = TemplateRegistration {
        name: "first",
        contents: "a",
    };

    static SECOND_TEMPLATE: TemplateRegistration = TemplateRegistration {
        name: "second",
        contents: "b",
    };

    static BLUEPRINT_ALPHA: AdapterBlueprint = AdapterBlueprint {
        id: "alpha",
        display_name: "Alpha",
        crate_suffix: "adapter-alpha",
        dependency_crate: "anyedge-adapter-alpha",
        dependency_repo_path: "crates/anyedge-adapter-alpha",
        template_registrations: &[FIRST_TEMPLATE],
        files: &[AdapterFileSpec {
            template: "first",
            output: "Cargo.toml",
        }],
        extra_dirs: &["src"],
        dependencies: &[DependencySpec {
            key: "dep_alpha",
            repo_crate: "crates/alpha",
            fallback: "alpha = \"0.1\"",
            features: &[],
        }],
        manifest: ManifestSpec {
            manifest_filename: "alpha.toml",
            build_target: "wasm32",
            build_profile: "release",
            build_features: &[],
        },
        commands: CommandTemplates {
            build: "build",
            serve: "serve",
            deploy: "deploy",
        },
        logging: LoggingDefaults {
            endpoint: Some("stdout"),
            level: "info",
            echo_stdout: Some(true),
        },
        readme: ReadmeInfo {
            description: "desc",
            dev_heading: "heading",
            dev_steps: &["step"],
        },
        run_module: "module",
    };

    static BLUEPRINT_BETA: AdapterBlueprint = AdapterBlueprint {
        id: "beta",
        display_name: "Beta",
        crate_suffix: "adapter-beta",
        dependency_crate: "anyedge-adapter-beta",
        dependency_repo_path: "crates/anyedge-adapter-beta",
        template_registrations: &[SECOND_TEMPLATE],
        files: &[AdapterFileSpec {
            template: "second",
            output: "src/main.rs",
        }],
        extra_dirs: &[],
        dependencies: &[],
        manifest: ManifestSpec {
            manifest_filename: "beta.toml",
            build_target: "wasm32",
            build_profile: "release",
            build_features: &[],
        },
        commands: CommandTemplates {
            build: "build",
            serve: "serve",
            deploy: "deploy",
        },
        logging: LoggingDefaults {
            endpoint: None,
            level: "info",
            echo_stdout: None,
        },
        readme: ReadmeInfo {
            description: "desc",
            dev_heading: "heading",
            dev_steps: &[],
        },
        run_module: "module",
    };

    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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
}
