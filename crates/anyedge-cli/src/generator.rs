use crate::args::NewArgs;
use crate::scaffold::{register_templates, resolve_dep_line, sanitize_crate_name, write_tmpl};
use anyedge_adapter::scaffold;
use anyedge_adapter::scaffold::AdapterBlueprint;
use handlebars::Handlebars;
use serde_json::{Map, Value};
use std::fmt::Write as _;
use std::path::PathBuf;

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

    // Resolve path dependencies to anyedge crates if building inside this repo
    let cwd = std::env::current_dir().unwrap();
    let dep_core_lib = resolve_dep_line(
        &core_dir,
        &cwd,
        "crates/anyedge-core",
        "anyedge-core = \"0.1\"",
        &[],
    );
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
            let resolved = resolve_dep_line(
                adapter_dir.as_path(),
                &cwd,
                dep.repo_crate,
                dep.fallback,
                dep.features,
            );
            data_entries.push((dep.key.to_string(), resolved));
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
        if let Some(endpoint) = blueprint.logging.endpoint {
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
    data.insert("name".into(), Value::String(name));
    data.insert("proj_core".into(), Value::String(core_name.clone()));
    data.insert("dep_anyedge_core".into(), Value::String(dep_core_lib));

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

    println!(
        "[anyedge] created new multi-crate app at {}",
        out_dir.display()
    );
    Ok(())
}
