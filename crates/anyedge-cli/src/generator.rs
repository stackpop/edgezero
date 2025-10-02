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
use std::path::PathBuf;
use std::process::Command;

struct AdapterContext<'a> {
    blueprint: &'a AdapterBlueprint,
    dir: PathBuf,
    data_entries: Vec<(String, String)>,
}

pub fn generate_new(args: NewArgs) -> std::io::Result<()> {
    use std::fs;

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

    // Create base dirs for multi-crate workspace
    let crates_dir = out_dir.join("crates");
    let core_name = format!("{}-core", name);
    let core_dir = crates_dir.join(&core_name);

    fs::create_dir_all(core_dir.join("src"))?;

    let mut workspace_dependencies: BTreeMap<String, String> = BTreeMap::new();
    workspace_dependencies
        .entry("bytes".to_string())
        .or_insert_with(|| "bytes = \"1\"".to_string());
    workspace_dependencies
        .entry("futures".to_string())
        .or_insert_with(|| {
            "futures = { version = \"0.3\", default-features = false, features = [\"std\", \"executor\"] }"
                .to_string()
        });
    workspace_dependencies
        .entry("serde".to_string())
        .or_insert_with(|| "serde = { version = \"1\", features = [\"derive\"] }".to_string());
    workspace_dependencies
        .entry("log".to_string())
        .or_insert_with(|| "log = \"0.4\"".to_string());
    workspace_dependencies
        .entry("worker".to_string())
        .or_insert_with(|| {
            "worker = { version = \"0.6\", default-features = false, features = [\"http\"] }"
                .to_string()
        });
    workspace_dependencies
        .entry("fastly".to_string())
        .or_insert_with(|| "fastly = \"0.11\"".to_string());
    workspace_dependencies
        .entry("once_cell".to_string())
        .or_insert_with(|| "once_cell = \"1\"".to_string());

    // Resolve path dependencies to anyedge crates if building inside this repo
    let cwd = std::env::current_dir().unwrap();
    let ResolvedDependency {
        name: core_dep_name,
        workspace_line: core_workspace_line,
        crate_line: core_crate_line,
    } = resolve_dep_line(
        &out_dir,
        &cwd,
        "crates/anyedge-core",
        "anyedge-core = { git = \"ssh://git@github.com/stackpop/anyedge.git\", package = \"anyedge-core\", default-features = false }",
        &[],
    );
    workspace_dependencies
        .entry(core_dep_name)
        .or_insert(core_workspace_line);
    let project_module_name = name.replace('-', "_");
    let mut adapter_contexts = Vec::new();
    let mut adapter_ids = Vec::new();
    let mut workspace_members = Vec::new();
    let mut manifest_sections = String::new();
    let mut logging_sections = String::new();
    let mut readme_adapter_crates = String::new();
    let mut readme_adapter_dev = String::new();

    let blueprints = scaffold::registered_blueprints();

    for blueprint in blueprints.iter().copied() {
        let crate_name = format!("{}-{}", name, blueprint.crate_suffix);
        let adapter_dir = crates_dir.join(&crate_name);
        for dir_name in blueprint.extra_dirs {
            fs::create_dir_all(adapter_dir.join(dir_name))?;
        }

        let mut data_entries: Vec<(String, String)> = Vec::new();
        data_entries.push((format!("proj_{}", blueprint.id), crate_name.clone()));

        for dep in blueprint.dependencies {
            let ResolvedDependency {
                name,
                workspace_line,
                crate_line,
            } = resolve_dep_line(&out_dir, &cwd, dep.repo_crate, dep.fallback, dep.features);
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

        let mut logging_section = String::new();
        writeln!(logging_section, "[logging.{}]", blueprint.id).unwrap();
        if blueprint.id == "fastly" {
            writeln!(
                logging_section,
                "endpoint = \"{}_log\"",
                project_module_name
            )
            .unwrap();
        } else if let Some(endpoint) = blueprint.logging.endpoint {
            writeln!(logging_section, "endpoint = \"{}\"", endpoint).unwrap();
        }
        writeln!(logging_section, "level = \"{}\"", blueprint.logging.level).unwrap();
        if let Some(echo_stdout) = blueprint.logging.echo_stdout {
            writeln!(
                logging_section,
                "echo_stdout = {}",
                if echo_stdout { "true" } else { "false" }
            )
            .unwrap();
        }
        logging_section.push('\n');

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
        logging_sections.push_str(&logging_section);

        workspace_members.push(format!("  \"crates/{}\",", crate_name));
        adapter_ids.push(blueprint.id.to_string());

        adapter_contexts.push(AdapterContext {
            blueprint,
            dir: adapter_dir,
            data_entries,
        });
    }

    // Prepare template data map
    let mut data = Map::new();
    data.insert("name".into(), Value::String(name.clone()));
    let core_module_name = core_name.replace('-', "_");
    let project_module_name = name.replace('-', "_");
    data.insert("proj_core".into(), Value::String(core_name.clone()));
    data.insert("proj_core_mod".into(), Value::String(core_module_name));
    data.insert("proj_mod".into(), Value::String(project_module_name));
    data.insert("dep_anyedge_core".into(), Value::String(core_crate_line));

    let adapter_list_str = adapter_ids
        .iter()
        .map(|id| format!("\"{}\"", id))
        .collect::<Vec<_>>()
        .join(", ");
    data.insert("adapter_list".into(), Value::String(adapter_list_str));
    data.insert(
        "workspace_members".into(),
        Value::String(workspace_members.join("\n")),
    );
    data.insert(
        "adapter_manifest_sections".into(),
        Value::String(manifest_sections),
    );
    data.insert("logging_sections".into(), Value::String(logging_sections));
    data.insert(
        "readme_adapter_crates".into(),
        Value::String(readme_adapter_crates),
    );
    data.insert(
        "readme_adapter_dev".into(),
        Value::String(readme_adapter_dev),
    );

    for context in &adapter_contexts {
        for (key, value) in &context.data_entries {
            data.insert(key.clone(), Value::String(value.clone()));
        }
    }

    let workspace_dep_lines = workspace_dependencies
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    data.insert(
        "workspace_dependencies".into(),
        Value::String(workspace_dep_lines),
    );

    let data_value = Value::Object(data.clone());

    // Render all templates
    let mut hbs = Handlebars::new();
    register_templates(&mut hbs);

    // Root workspace files
    write_tmpl(
        &hbs,
        "root_Cargo_toml",
        &data_value,
        &out_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "root_anyedge_toml",
        &data_value,
        &out_dir.join("anyedge.toml"),
    )?;
    write_tmpl(
        &hbs,
        "root_README_md",
        &data_value,
        &out_dir.join("README.md"),
    )?;
    write_tmpl(
        &hbs,
        "root_gitignore",
        &data_value,
        &out_dir.join(".gitignore"),
    )?;

    // Core crate
    write_tmpl(
        &hbs,
        "core_Cargo_toml",
        &data_value,
        &core_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "core_src_lib_rs",
        &data_value,
        &core_dir.join("src/lib.rs"),
    )?;
    write_tmpl(
        &hbs,
        "core_src_handlers_rs",
        &data_value,
        &core_dir.join("src/handlers.rs"),
    )?;

    for context in &adapter_contexts {
        for file in context.blueprint.files {
            write_tmpl(
                &hbs,
                file.template,
                &data_value,
                &context.dir.join(file.output),
            )?;
        }
    }

    if let Err(err) = Command::new("git")
        .arg("init")
        .current_dir(&out_dir)
        .status()
    {
        eprintln!(
            "[anyedge] warning: failed to initialize git repository: {}",
            err
        );
    }

    println!(
        "[anyedge] created new multi-crate app at {}",
        out_dir.display()
    );
    Ok(())
}
