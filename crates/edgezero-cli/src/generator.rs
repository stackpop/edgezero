use crate::args::NewArgs;
use crate::scaffold::{
    register_templates, resolve_dep_line, sanitize_crate_name, write_tmpl, ResolvedDependency,
    ScaffoldError,
};
use edgezero_adapter::scaffold;
use edgezero_adapter::scaffold::AdapterBlueprint;
use handlebars::Handlebars;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

/// Errors produced by `edgezero new`.
#[derive(Debug, Error)]
pub enum GeneratorError {
    /// An adapter context was constructed with no terminal path component.
    /// Should be unreachable given the layout we build, but propagated rather
    /// than panicking on the request path.
    #[error("adapter context directory has no file name: {}", .0.display())]
    AdapterDirMissingFileName(PathBuf),
    /// `write!`/`writeln!` to an in-memory `String` buffer failed. In
    /// practice the only way this can fire is a malformed `Display` impl in
    /// one of the rendered values; surfaced as a typed error rather than a
    /// silent unwrap.
    #[error("failed to format generator output: {0}")]
    Format(#[from] fmt::Error),
    /// A filesystem read/write/metadata operation failed while preparing the
    /// project skeleton.
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The target output directory already exists; refusing to overwrite.
    #[error("directory '{}' already exists", .0.display())]
    OutputDirExists(PathBuf),
    /// A template under the workspace scaffold could not be rendered or
    /// written. Wraps [`ScaffoldError`] for context.
    #[error(transparent)]
    Scaffold(#[from] ScaffoldError),
}

impl GeneratorError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        GeneratorError::Io {
            path: path.into(),
            source,
        }
    }
}

struct AdapterContext<'blueprint> {
    blueprint: &'blueprint AdapterBlueprint,
    data_entries: Vec<(String, String)>,
    dir: PathBuf,
}

struct ProjectLayout {
    core_dir: PathBuf,
    core_mod: String,
    core_name: String,
    crates_dir: PathBuf,
    name: String,
    out_dir: PathBuf,
    project_mod: String,
}

impl ProjectLayout {
    fn new(args: &NewArgs) -> Result<Self, GeneratorError> {
        let name = sanitize_crate_name(&args.name);
        let base_dir = match args.dir.as_deref() {
            Some(dir) => PathBuf::from(dir),
            None => env::current_dir().map_err(|err| GeneratorError::io(".", err))?,
        };
        let out_dir = base_dir.join(&name);
        if out_dir.exists() {
            return Err(GeneratorError::OutputDirExists(out_dir));
        }

        log::info!("[edgezero] creating project at {}", out_dir.display());

        let crates_dir = out_dir.join("crates");
        let core_name = format!("{name}-core");
        let core_dir = crates_dir.join(&core_name);
        let core_src = core_dir.join("src");
        fs::create_dir_all(&core_src).map_err(|err| GeneratorError::io(&core_src, err))?;

        let project_mod = name.replace('-', "_");
        let core_mod = core_name.replace('-', "_");
        Ok(ProjectLayout {
            core_dir,
            core_mod,
            core_name,
            crates_dir,
            name,
            out_dir,
            project_mod,
        })
    }
}

struct AdapterArtifacts {
    adapter_ids: Vec<String>,
    contexts: Vec<AdapterContext<'static>>,
    manifest_sections: String,
    readme_adapter_crates: String,
    readme_adapter_dev: String,
    workspace_members: Vec<String>,
}

/// # Errors
/// Returns [`GeneratorError`] if any filesystem operation, template render,
/// or layout invariant fails.
pub fn generate_new(args: &NewArgs) -> Result<(), GeneratorError> {
    let layout = ProjectLayout::new(args)?;

    let mut workspace_dependencies = seed_workspace_dependencies();
    let cwd = env::current_dir().map_err(|err| GeneratorError::io(".", err))?;
    let core_crate_line = resolve_core_dependency(&layout, &cwd, &mut workspace_dependencies);

    let adapter_artifacts = collect_adapter_data(&layout, &cwd, &mut workspace_dependencies)?;

    let mut data_map = build_base_data(
        &layout,
        &core_crate_line,
        &adapter_artifacts,
        &workspace_dependencies,
    );

    for context in &adapter_artifacts.contexts {
        for (key, value) in &context.data_entries {
            data_map.insert(key.clone(), Value::String(value.clone()));
        }
    }

    let data_value = Value::Object(data_map);

    render_templates(&layout, &adapter_artifacts.contexts, &data_value)?;
    initialize_git_repo(&layout.out_dir);

    log::info!(
        "[edgezero] created new multi-crate app at {}",
        layout.out_dir.display()
    );

    Ok(())
}

fn seed_workspace_dependencies() -> BTreeMap<String, String> {
    let mut deps = BTreeMap::new();
    deps.insert("bytes".to_owned(), "bytes = \"1\"".to_owned());
    deps.insert("anyhow".to_owned(), "anyhow = \"1\"".to_owned());
    deps.insert(
        "futures".to_owned(),
        "futures = { version = \"0.3\", default-features = false, features = [\"std\", \"executor\"] }"
            .to_owned(),
    );
    deps.insert("axum".to_owned(), "axum = \"0.8\"".to_owned());
    deps.insert(
        "serde".to_owned(),
        "serde = { version = \"1\", features = [\"derive\"] }".to_owned(),
    );
    deps.insert("log".to_owned(), "log = \"0.4\"".to_owned());
    deps.insert(
        "simple_logger".to_owned(),
        "simple_logger = \"4\"".to_owned(),
    );
    deps.insert(
        "worker".to_owned(),
        "worker = { version = \"0.7\", default-features = false, features = [\"http\"] }"
            .to_owned(),
    );
    deps.insert("fastly".to_owned(), "fastly = \"0.11\"".to_owned());
    deps.insert("once_cell".to_owned(), "once_cell = \"1\"".to_owned());
    deps.insert(
        "tokio".to_owned(),
        "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }".to_owned(),
    );
    deps.insert("tracing".to_owned(), "tracing = \"0.1\"".to_owned());
    deps.insert(
        "spin-sdk".to_owned(),
        "spin-sdk = { version = \"5.2\", default-features = false }".to_owned(),
    );
    deps
}

fn resolve_core_dependency(
    layout: &ProjectLayout,
    cwd: &Path,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> String {
    let ResolvedDependency {
        name,
        workspace_line,
        crate_line,
    } = resolve_dep_line(
        &layout.out_dir,
        cwd,
        "crates/edgezero-core",
        "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        &[],
    );

    workspace_dependencies.entry(name).or_insert(workspace_line);
    crate_line
}

fn collect_adapter_data(
    layout: &ProjectLayout,
    cwd: &Path,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> Result<AdapterArtifacts, GeneratorError> {
    let mut contexts = Vec::new();
    let mut adapter_ids = Vec::new();
    let mut workspace_members = Vec::new();
    let mut manifest_sections = String::new();
    let mut readme_adapter_crates = String::new();
    let mut readme_adapter_dev = String::new();

    for blueprint in scaffold::registered_blueprints().iter().copied() {
        let crate_name = format!("{}-{}", layout.name, blueprint.crate_suffix);
        let adapter_dir = layout.crates_dir.join(&crate_name);
        fs::create_dir_all(&adapter_dir).map_err(|err| GeneratorError::io(&adapter_dir, err))?;
        for dir_name in blueprint.extra_dirs {
            let extra = adapter_dir.join(dir_name);
            fs::create_dir_all(&extra).map_err(|err| GeneratorError::io(&extra, err))?;
        }

        let crate_dir_rel = format!("crates/{crate_name}");
        let data_entries = blueprint_data_entries(
            layout,
            cwd,
            blueprint,
            &crate_name,
            &crate_dir_rel,
            workspace_dependencies,
        );

        manifest_sections.push_str(&render_manifest_section(
            layout,
            blueprint,
            &crate_name,
            &crate_dir_rel,
        )?);
        append_readme_entries(
            blueprint,
            &crate_name,
            &crate_dir_rel,
            &mut readme_adapter_crates,
            &mut readme_adapter_dev,
        )?;

        workspace_members.push(format!("  \"crates/{crate_name}\","));
        adapter_ids.push(blueprint.id.to_owned());

        contexts.push(AdapterContext {
            blueprint,
            data_entries,
            dir: adapter_dir,
        });
    }

    Ok(AdapterArtifacts {
        adapter_ids,
        contexts,
        manifest_sections,
        readme_adapter_crates,
        readme_adapter_dev,
        workspace_members,
    })
}

/// Build the `(key, value)` template-data entries for a single adapter blueprint,
/// resolving its dependencies and recording them in `workspace_dependencies`.
fn blueprint_data_entries(
    layout: &ProjectLayout,
    cwd: &Path,
    blueprint: &'static AdapterBlueprint,
    crate_name: &str,
    crate_dir_rel: &str,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut data_entries: Vec<(String, String)> = Vec::new();
    data_entries.push((format!("proj_{}", blueprint.id), crate_name.to_owned()));
    data_entries.push((
        format!("proj_{}_underscored", blueprint.id),
        crate_name.replace('-', "_"),
    ));

    for dep in blueprint.dependencies {
        let ResolvedDependency {
            name,
            workspace_line,
            crate_line,
        } = resolve_dep_line(
            &layout.out_dir,
            cwd,
            dep.repo_crate,
            dep.fallback,
            dep.features,
        );
        workspace_dependencies.entry(name).or_insert(workspace_line);
        data_entries.push((dep.key.to_owned(), crate_line));
    }

    // Compute the relative path from the adapter crate to the workspace
    // target directory so templates can reference build artifacts.
    let depth = crate_dir_rel.matches('/').count().saturating_add(1);
    data_entries.push((
        format!("target_dir_{}", blueprint.id),
        format!("{}target", "../".repeat(depth)),
    ));

    data_entries
}

/// Render the `[adapters.<id>.*]` TOML stanza for a single blueprint.
fn render_manifest_section(
    layout: &ProjectLayout,
    blueprint: &'static AdapterBlueprint,
    crate_name: &str,
    crate_dir_rel: &str,
) -> Result<String, fmt::Error> {
    let build_cmd = blueprint
        .commands
        .build
        .replace("{crate}", crate_name)
        .replace("{crate_dir}", crate_dir_rel);
    let serve_cmd = blueprint
        .commands
        .serve
        .replace("{crate}", crate_name)
        .replace("{crate_dir}", crate_dir_rel);
    let deploy_cmd = blueprint
        .commands
        .deploy
        .replace("{crate}", crate_name)
        .replace("{crate_dir}", crate_dir_rel);

    let mut out = String::new();
    writeln!(
        out,
        "[adapters.{}.adapter]\ncrate = \"crates/{}\"\nmanifest = \"crates/{}/{}\"\n",
        blueprint.id, crate_name, crate_name, blueprint.manifest.manifest_filename,
    )?;
    writeln!(
        out,
        "[adapters.{}.build]\ntarget = \"{}\"\nprofile = \"{}\"",
        blueprint.id, blueprint.manifest.build_target, blueprint.manifest.build_profile,
    )?;
    if !blueprint.manifest.build_features.is_empty() {
        let joined = blueprint
            .manifest
            .build_features
            .iter()
            .map(|feat| format!("\"{feat}\""))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "features = [{joined}]")?;
    }
    out.push('\n');
    writeln!(
        out,
        "[adapters.{}.commands]\nbuild = \"{}\"\ndeploy = \"{}\"\nserve = \"{}\"\n",
        blueprint.id, build_cmd, deploy_cmd, serve_cmd,
    )?;

    out.push('\n');
    writeln!(out, "[adapters.{}.logging]", blueprint.id)?;
    let endpoint_value = if blueprint.id == "fastly" {
        Some(format!("{}_log", layout.project_mod))
    } else {
        blueprint.logging.endpoint.map(str::to_owned)
    };
    if let Some(endpoint) = endpoint_value {
        writeln!(out, "endpoint = \"{endpoint}\"")?;
    }
    writeln!(out, "level = \"{}\"", blueprint.logging.level)?;
    if let Some(echo_stdout) = blueprint.logging.echo_stdout {
        writeln!(
            out,
            "echo_stdout = {}",
            if echo_stdout { "true" } else { "false" },
        )?;
    }
    out.push('\n');
    Ok(out)
}

/// Append the per-adapter README entries for crates list and dev-step list.
fn append_readme_entries(
    blueprint: &'static AdapterBlueprint,
    crate_name: &str,
    crate_dir_rel: &str,
    readme_adapter_crates: &mut String,
    readme_adapter_dev: &mut String,
) -> Result<(), fmt::Error> {
    let description = blueprint
        .readme
        .description
        .replace("{display}", blueprint.display_name);
    writeln!(
        readme_adapter_crates,
        "- `crates/{crate_name}`: {description}"
    )?;

    let heading = blueprint
        .readme
        .dev_heading
        .replace("{display}", blueprint.display_name);
    writeln!(readme_adapter_dev, "- {heading}:")?;
    for step in blueprint.readme.dev_steps {
        let formatted = step
            .replace("{crate}", crate_name)
            .replace("{crate_dir}", crate_dir_rel);
        writeln!(readme_adapter_dev, "  - {formatted}")?;
    }
    readme_adapter_dev.push('\n');
    Ok(())
}

fn build_base_data(
    layout: &ProjectLayout,
    core_crate_line: &str,
    artifacts: &AdapterArtifacts,
    workspace_dependencies: &BTreeMap<String, String>,
) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("name".into(), Value::String(layout.name.clone()));
    data.insert("proj_core".into(), Value::String(layout.core_name.clone()));
    data.insert(
        "proj_core_mod".into(),
        Value::String(layout.core_mod.clone()),
    );
    data.insert("proj_mod".into(), Value::String(layout.project_mod.clone()));
    data.insert(
        "dep_edgezero_core".into(),
        Value::String(core_crate_line.to_owned()),
    );

    let adapter_list_str = artifacts
        .adapter_ids
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    data.insert("adapter_list".into(), Value::String(adapter_list_str));
    data.insert(
        "workspace_members".into(),
        Value::String(artifacts.workspace_members.join("\n")),
    );
    data.insert(
        "adapter_manifest_sections".into(),
        Value::String(artifacts.manifest_sections.clone()),
    );
    data.insert(
        "readme_adapter_crates".into(),
        Value::String(artifacts.readme_adapter_crates.clone()),
    );
    data.insert(
        "readme_adapter_dev".into(),
        Value::String(artifacts.readme_adapter_dev.clone()),
    );

    let workspace_dep_lines = workspace_dependencies
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    data.insert(
        "workspace_dependencies".into(),
        Value::String(workspace_dep_lines),
    );

    data
}

fn render_templates(
    layout: &ProjectLayout,
    adapter_contexts: &[AdapterContext],
    data_value: &Value,
) -> Result<(), GeneratorError> {
    let mut hbs = Handlebars::new();
    register_templates(&mut hbs);

    log::info!("[edgezero] writing workspace files");
    write_tmpl(
        &hbs,
        "root_Cargo_toml",
        data_value,
        &layout.out_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "root_edgezero_toml",
        data_value,
        &layout.out_dir.join("edgezero.toml"),
    )?;
    write_tmpl(
        &hbs,
        "root_README_md",
        data_value,
        &layout.out_dir.join("README.md"),
    )?;
    write_tmpl(
        &hbs,
        "root_gitignore",
        data_value,
        &layout.out_dir.join(".gitignore"),
    )?;
    write_tmpl(
        &hbs,
        "root_clippy_toml",
        data_value,
        &layout.out_dir.join("clippy.toml"),
    )?;

    log::info!("[edgezero] writing core crate {}", layout.core_name);
    write_tmpl(
        &hbs,
        "core_Cargo_toml",
        data_value,
        &layout.core_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "core_src_lib_rs",
        data_value,
        &layout.core_dir.join("src/lib.rs"),
    )?;
    write_tmpl(
        &hbs,
        "core_src_handlers_rs",
        data_value,
        &layout.core_dir.join("src/handlers.rs"),
    )?;

    for context in adapter_contexts {
        let crate_dir_name = context
            .dir
            .file_name()
            .ok_or_else(|| GeneratorError::AdapterDirMissingFileName(context.dir.clone()))?;
        log::info!(
            "[edgezero] writing adapter crate {}",
            crate_dir_name.to_string_lossy(),
        );
        for file in context.blueprint.files {
            write_tmpl(
                &hbs,
                file.template,
                data_value,
                &context.dir.join(file.output),
            )?;
        }
    }

    Ok(())
}

fn initialize_git_repo(out_dir: &Path) {
    log::info!("[edgezero] initializing git repository");
    match Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(out_dir)
        .status()
    {
        Ok(status) if status.success() => {
            log::info!(
                "[edgezero] initialized empty Git repository in {}/.git/",
                out_dir.display()
            );
        }
        Ok(status) => {
            log::warn!("[edgezero] warning: git init exited with status {status}");
        }
        Err(err) => {
            log::warn!("[edgezero] warning: failed to initialize git repository: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // `super::*` re-exports `env` and `fs` from outer `use` lines, so they're
    // already in scope here.

    struct PathOverride {
        original: Option<String>,
    }

    impl PathOverride {
        fn prepend(path: &Path) -> Self {
            let original = env::var("PATH").ok();
            let sep = if cfg!(windows) { ";" } else { ":" };
            let prefix = path.to_string_lossy();
            let new_path = match &original {
                Some(existing) if !existing.is_empty() => format!("{prefix}{sep}{existing}"),
                _ => prefix.into_owned(),
            };
            env::set_var("PATH", &new_path);
            Self { original }
        }
    }

    impl Drop for PathOverride {
        fn drop(&mut self) {
            if let Some(original) = &self.original {
                env::set_var("PATH", original);
            } else {
                env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn generate_new_scaffolds_workspace_layout() {
        let temp = TempDir::new().expect("temp dir");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let git_path = if cfg!(windows) {
            bin_dir.join("git.cmd")
        } else {
            bin_dir.join("git")
        };

        if cfg!(windows) {
            fs::write(&git_path, b"@echo off\r\nexit /b 0\r\n").expect("write git stub");
        } else {
            fs::write(&git_path, b"#!/bin/sh\nexit 0\n").expect("write git stub");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = fs::metadata(&git_path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&git_path, perms).expect("chmod");
        };

        let _path_guard = PathOverride::prepend(&bin_dir);

        let args = NewArgs {
            name: "demo-app".into(),
            dir: Some(temp.path().to_string_lossy().into_owned()),
            local_core: false,
        };

        generate_new(&args).expect("scaffold succeeds");

        let project_dir = temp.path().join("demo-app");
        assert!(project_dir.is_dir(), "project directory created");
        assert!(project_dir.join("Cargo.toml").exists());
        assert!(project_dir.join("edgezero.toml").exists());
        assert!(project_dir.join(".gitignore").exists());
        assert!(project_dir.join("README.md").exists());
        assert!(project_dir.join("crates/demo-app-core/src/lib.rs").exists());

        let cargo_toml =
            fs::read_to_string(project_dir.join("Cargo.toml")).expect("read Cargo.toml");
        assert!(cargo_toml.contains("crates/demo-app-core"));
        assert!(cargo_toml.contains("crates/demo-app-adapter-cloudflare"));
        assert!(cargo_toml.contains("crates/demo-app-adapter-fastly"));
        assert!(
            cargo_toml.contains("crates/demo-app-adapter-spin"),
            "workspace Cargo.toml should include spin adapter"
        );

        let manifest =
            fs::read_to_string(project_dir.join("edgezero.toml")).expect("read edgezero.toml");
        assert!(manifest.contains("[adapters.cloudflare.adapter]"));
        assert!(manifest.contains("[adapters.fastly.adapter]"));
        assert!(
            manifest.contains("[adapters.spin"),
            "edgezero.toml should include spin adapter section"
        );
        assert!(
            project_dir
                .join("crates/demo-app-adapter-spin/spin.toml")
                .exists(),
            "spin.toml should be scaffolded"
        );

        let gitignore =
            fs::read_to_string(project_dir.join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.contains("target/"));

        let clippy = fs::read_to_string(project_dir.join("clippy.toml")).expect("read clippy.toml");
        assert!(clippy.contains("allow-expect-in-tests = true"));

        assert!(cargo_toml.contains("[workspace.lints.clippy]"));
        assert!(cargo_toml.contains("blanket_clippy_restriction_lints = \"allow\""));

        for crate_dir in [
            "crates/demo-app-core",
            "crates/demo-app-adapter-axum",
            "crates/demo-app-adapter-cloudflare",
            "crates/demo-app-adapter-fastly",
            "crates/demo-app-adapter-spin",
        ] {
            let path = project_dir.join(crate_dir).join("Cargo.toml");
            let body =
                fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {}", path.display()));
            assert!(
                body.contains("[lints]\nworkspace = true"),
                "{crate_dir} must inherit workspace lints",
            );
        }
    }
}
