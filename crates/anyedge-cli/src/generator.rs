use crate::args::NewArgs;
use crate::scaffold::{
    register_templates, resolve_dep_line, sanitize_crate_name, write_tmpl, ResolvedDependency,
};
use anyedge_adapter::scaffold;
use anyedge_adapter::scaffold::AdapterBlueprint;
use handlebars::Handlebars;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

struct AdapterContext<'a> {
    blueprint: &'a AdapterBlueprint,
    dir: PathBuf,
    data_entries: Vec<(String, String)>,
}

struct ProjectLayout {
    name: String,
    out_dir: PathBuf,
    crates_dir: PathBuf,
    core_name: String,
    core_dir: PathBuf,
    project_mod: String,
    core_mod: String,
}

impl ProjectLayout {
    fn new(args: &NewArgs) -> std::io::Result<Self> {
        let name = sanitize_crate_name(&args.name);
        let base_dir = args
            .dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let out_dir = base_dir.join(&name);
        if out_dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("directory '{}' already exists", out_dir.display()),
            ));
        }

        println!("[anyedge] creating project at {}", out_dir.display());

        let crates_dir = out_dir.join("crates");
        let core_name = format!("{}-core", name);
        let core_dir = crates_dir.join(&core_name);
        std::fs::create_dir_all(core_dir.join("src"))?;

        Ok(ProjectLayout {
            project_mod: name.replace('-', "_"),
            core_mod: core_name.replace('-', "_"),
            core_name,
            core_dir,
            crates_dir,
            out_dir,
            name,
        })
    }
}

struct AdapterArtifacts {
    contexts: Vec<AdapterContext<'static>>,
    adapter_ids: Vec<String>,
    workspace_members: Vec<String>,
    manifest_sections: String,
    readme_adapter_crates: String,
    readme_adapter_dev: String,
}

pub fn generate_new(args: NewArgs) -> std::io::Result<()> {
    let layout = ProjectLayout::new(&args)?;

    let mut workspace_dependencies = seed_workspace_dependencies();
    let cwd = std::env::current_dir().unwrap();
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

    println!(
        "[anyedge] created new multi-crate app at {}",
        layout.out_dir.display()
    );

    Ok(())
}

fn seed_workspace_dependencies() -> BTreeMap<String, String> {
    let mut deps = BTreeMap::new();
    deps.insert("bytes".to_string(), "bytes = \"1\"".to_string());
    deps.insert("anyhow".to_string(), "anyhow = \"1\"".to_string());
    deps.insert(
        "futures".to_string(),
        "futures = { version = \"0.3\", default-features = false, features = [\"std\", \"executor\"] }"
            .to_string(),
    );
    deps.insert("axum".to_string(), "axum = \"0.7\"".to_string());
    deps.insert(
        "serde".to_string(),
        "serde = { version = \"1\", features = [\"derive\"] }".to_string(),
    );
    deps.insert("log".to_string(), "log = \"0.4\"".to_string());
    deps.insert(
        "simple_logger".to_string(),
        "simple_logger = \"4\"".to_string(),
    );
    deps.insert(
        "worker".to_string(),
        "worker = { version = \"0.6\", default-features = false, features = [\"http\"] }"
            .to_string(),
    );
    deps.insert("fastly".to_string(), "fastly = \"0.11\"".to_string());
    deps.insert("once_cell".to_string(), "once_cell = \"1\"".to_string());
    deps.insert(
        "tokio".to_string(),
        "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }".to_string(),
    );
    deps.insert("tracing".to_string(), "tracing = \"0.1\"".to_string());
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
        "crates/anyedge-core",
        "anyedge-core = { git = \"ssh://git@github.com/stackpop/anyedge.git\", package = \"anyedge-core\", default-features = false }",
        &[],
    );

    workspace_dependencies.entry(name).or_insert(workspace_line);
    crate_line
}

fn collect_adapter_data(
    layout: &ProjectLayout,
    cwd: &Path,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> std::io::Result<AdapterArtifacts> {
    let mut contexts = Vec::new();
    let mut adapter_ids = Vec::new();
    let mut workspace_members = Vec::new();
    let mut manifest_sections = String::new();
    let mut readme_adapter_crates = String::new();
    let mut readme_adapter_dev = String::new();

    let blueprints = scaffold::registered_blueprints();

    for blueprint in blueprints.iter().copied() {
        let crate_name = format!("{}-{}", layout.name, blueprint.crate_suffix);
        let adapter_dir = layout.crates_dir.join(&crate_name);
        std::fs::create_dir_all(&adapter_dir)?;
        for dir_name in blueprint.extra_dirs {
            std::fs::create_dir_all(adapter_dir.join(dir_name))?;
        }

        let mut data_entries: Vec<(String, String)> = Vec::new();
        data_entries.push((format!("proj_{}", blueprint.id), crate_name.clone()));

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
            data_entries.push((dep.key.to_string(), crate_line));
        }

        let crate_dir_rel = format!("crates/{}", crate_name);

        let build_cmd = blueprint
            .commands
            .build
            .replace("{crate}", &crate_name)
            .replace("{crate_dir}", &crate_dir_rel);
        let serve_cmd = blueprint
            .commands
            .serve
            .replace("{crate}", &crate_name)
            .replace("{crate_dir}", &crate_dir_rel);
        let deploy_cmd = blueprint
            .commands
            .deploy
            .replace("{crate}", &crate_name)
            .replace("{crate_dir}", &crate_dir_rel);

        let mut manifest_section = String::new();
        writeln!(
            manifest_section,
            "[adapters.{}.adapter]\ncrate = \"crates/{}\"\nmanifest = \"crates/{}/{}\"\n",
            blueprint.id, crate_name, crate_name, blueprint.manifest.manifest_filename
        )
        .unwrap();
        writeln!(
            manifest_section,
            "[adapters.{}.build]\ntarget = \"{}\"\nprofile = \"{}\"",
            blueprint.id, blueprint.manifest.build_target, blueprint.manifest.build_profile
        )
        .unwrap();
        if !blueprint.manifest.build_features.is_empty() {
            let joined = blueprint
                .manifest
                .build_features
                .iter()
                .map(|f| format!("\"{}\"", f))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(manifest_section, "features = [{}]", joined).unwrap();
        }
        manifest_section.push('\n');
        writeln!(
            manifest_section,
            "[adapters.{}.commands]\nbuild = \"{}\"\nserve = \"{}\"\ndeploy = \"{}\"\n",
            blueprint.id, build_cmd, serve_cmd, deploy_cmd
        )
        .unwrap();

        manifest_section.push('\n');
        writeln!(manifest_section, "[adapters.{}.logging]", blueprint.id).unwrap();
        if blueprint.id == "fastly" {
            writeln!(
                manifest_section,
                "endpoint = \"{}_log\"",
                layout.project_mod
            )
            .unwrap();
        } else if let Some(endpoint) = blueprint.logging.endpoint {
            writeln!(manifest_section, "endpoint = \"{}\"", endpoint).unwrap();
        }
        writeln!(manifest_section, "level = \"{}\"", blueprint.logging.level).unwrap();
        if let Some(echo_stdout) = blueprint.logging.echo_stdout {
            writeln!(
                manifest_section,
                "echo_stdout = {}",
                if echo_stdout { "true" } else { "false" }
            )
            .unwrap();
        }
        manifest_section.push('\n');

        let description = blueprint
            .readme
            .description
            .replace("{display}", blueprint.display_name);
        readme_adapter_crates.push_str(&format!("- `crates/{}`: {}\n", crate_name, description));

        let heading = blueprint
            .readme
            .dev_heading
            .replace("{display}", blueprint.display_name);
        readme_adapter_dev.push_str(&format!("- {}:\n", heading));
        for step in blueprint.readme.dev_steps {
            let formatted = step
                .replace("{crate}", &crate_name)
                .replace("{crate_dir}", &crate_dir_rel);
            readme_adapter_dev.push_str(&format!("  - `{}`\n", formatted));
        }
        readme_adapter_dev.push('\n');

        manifest_sections.push_str(&manifest_section);
        workspace_members.push(format!("  \"crates/{}\",", crate_name));
        adapter_ids.push(blueprint.id.to_string());

        contexts.push(AdapterContext {
            blueprint,
            dir: adapter_dir,
            data_entries,
        });
    }

    Ok(AdapterArtifacts {
        contexts,
        adapter_ids,
        workspace_members,
        manifest_sections,
        readme_adapter_crates,
        readme_adapter_dev,
    })
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
        "dep_anyedge_core".into(),
        Value::String(core_crate_line.to_string()),
    );

    let adapter_list_str = artifacts
        .adapter_ids
        .iter()
        .map(|id| format!("\"{}\"", id))
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
) -> std::io::Result<()> {
    let mut hbs = Handlebars::new();
    register_templates(&mut hbs);

    println!("[anyedge] writing workspace files");
    write_tmpl(
        &hbs,
        "root_Cargo_toml",
        data_value,
        &layout.out_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "root_anyedge_toml",
        data_value,
        &layout.out_dir.join("anyedge.toml"),
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

    println!("[anyedge] writing core crate {}", layout.core_name);
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
        println!(
            "[anyedge] writing adapter crate {}",
            context.dir.file_name().unwrap().to_string_lossy()
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
    println!("[anyedge] initializing git repository");
    match Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(out_dir)
        .status()
    {
        Ok(status) if status.success() => {
            println!(
                "[anyedge] initialized empty Git repository in {}/.git/",
                out_dir.display()
            );
        }
        Ok(status) => {
            eprintln!("[anyedge] warning: git init exited with status {status}");
        }
        Err(err) => {
            eprintln!(
                "[anyedge] warning: failed to initialize git repository: {}",
                err
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    struct PathOverride {
        original: Option<String>,
    }

    impl PathOverride {
        fn prepend(path: &Path) -> Self {
            let original = std::env::var("PATH").ok();
            let sep = if cfg!(windows) { ";" } else { ":" };
            let prefix = path.to_string_lossy();
            let new_path = match &original {
                Some(existing) if !existing.is_empty() => format!("{prefix}{sep}{existing}"),
                _ => prefix.into_owned(),
            };
            std::env::set_var("PATH", &new_path);
            Self { original }
        }
    }

    impl Drop for PathOverride {
        fn drop(&mut self) {
            if let Some(ref original) = self.original {
                std::env::set_var("PATH", original);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn generate_new_scaffolds_workspace_layout() {
        let temp = TempDir::new().expect("temp dir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let git_path = if cfg!(windows) {
            bin_dir.join("git.cmd")
        } else {
            bin_dir.join("git")
        };

        if cfg!(windows) {
            std::fs::write(&git_path, b"@echo off\r\nexit /b 0\r\n").expect("write git stub");
        } else {
            std::fs::write(&git_path, b"#!/bin/sh\nexit 0\n").expect("write git stub");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&git_path)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&git_path, perms).expect("chmod");
        }

        let _path_guard = PathOverride::prepend(&bin_dir);

        let args = NewArgs {
            name: "demo-app".into(),
            dir: Some(temp.path().to_string_lossy().into_owned()),
            local_core: false,
        };

        generate_new(args).expect("scaffold succeeds");

        let project_dir = temp.path().join("demo-app");
        assert!(project_dir.is_dir(), "project directory created");
        assert!(project_dir.join("Cargo.toml").exists());
        assert!(project_dir.join("anyedge.toml").exists());
        assert!(project_dir.join(".gitignore").exists());
        assert!(project_dir.join("README.md").exists());
        assert!(project_dir.join("crates/demo-app-core/src/lib.rs").exists());

        let cargo_toml =
            std::fs::read_to_string(project_dir.join("Cargo.toml")).expect("read Cargo.toml");
        assert!(cargo_toml.contains("crates/demo-app-core"));
        assert!(cargo_toml.contains("crates/demo-app-adapter-cloudflare"));
        assert!(cargo_toml.contains("crates/demo-app-adapter-fastly"));

        let manifest =
            std::fs::read_to_string(project_dir.join("anyedge.toml")).expect("read anyedge.toml");
        assert!(manifest.contains("[adapters.cloudflare.adapter]"));
        assert!(manifest.contains("[adapters.fastly.adapter]"));

        let gitignore =
            std::fs::read_to_string(project_dir.join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.contains("target/"));
    }
}
