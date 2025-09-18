use crate::args::NewArgs;
use crate::scaffold::{register_templates, resolve_dep_line, sanitize_crate_name, write_tmpl};
use handlebars::Handlebars;
use serde_json::json;
use std::path::PathBuf;

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
    let fastly_name = format!("{}-fastly", name);
    let cloudflare_name = format!("{}-cloudflare", name);

    let core_dir = crates_dir.join(&core_name);
    let fastly_dir = crates_dir.join(&fastly_name);
    let cloudflare_dir = crates_dir.join(&cloudflare_name);

    fs::create_dir_all(core_dir.join("src"))?;
    fs::create_dir_all(fastly_dir.join("src"))?;
    fs::create_dir_all(fastly_dir.join(".cargo"))?;
    fs::create_dir_all(cloudflare_dir.join("src"))?;
    fs::create_dir_all(cloudflare_dir.join(".cargo"))?;

    // Resolve path dependencies to anyedge crates if building inside this repo
    let cwd = std::env::current_dir().unwrap();
    let dep_core = resolve_dep_line(
        &core_dir,
        &cwd,
        "crates/anyedge-core",
        "anyedge-core = \"0.1\"",
    );
    let dep_controller = resolve_dep_line(
        &core_dir,
        &cwd,
        "crates/anyedge-controller",
        "anyedge-controller = \"0.1\"",
    );
    let dep_fastly = resolve_dep_line(
        &fastly_dir,
        &cwd,
        "crates/anyedge-fastly",
        "anyedge-fastly = { version = \"0.1\", features = [\"fastly\"] }",
    );
    let dep_cloudflare = resolve_dep_line(
        &cloudflare_dir,
        &cwd,
        "crates/anyedge-cloudflare",
        "anyedge-cloudflare = { version = \"0.1\", features = [\"cloudflare\"] }",
    );

    // Prepare template data
    let data = json!({
        "name": name,
        "proj_core": core_name,
        "proj_fastly": fastly_name,
        "proj_cloudflare": cloudflare_name,
        "dep_anyedge_core": dep_core,
        "dep_anyedge_controller": dep_controller,
        "dep_anyedge_fastly": dep_fastly,
        "dep_anyedge_cloudflare": dep_cloudflare,
    });

    // Render all templates
    let mut hbs = Handlebars::new();
    register_templates(&mut hbs);

    // Root workspace files
    write_tmpl(&hbs, "root_Cargo_toml", &data, &out_dir.join("Cargo.toml"))?;
    write_tmpl(&hbs, "root_README_md", &data, &out_dir.join("README.md"))?;

    // Core crate
    write_tmpl(&hbs, "core_Cargo_toml", &data, &core_dir.join("Cargo.toml"))?;
    write_tmpl(&hbs, "core_src_lib_rs", &data, &core_dir.join("src/lib.rs"))?;

    // Fastly crate
    write_tmpl(
        &hbs,
        "fastly_Cargo_toml",
        &data,
        &fastly_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "fastly_src_main_rs",
        &data,
        &fastly_dir.join("src/main.rs"),
    )?;
    write_tmpl(
        &hbs,
        "fastly_cargo_config_toml",
        &data,
        &fastly_dir.join(".cargo/config.toml"),
    )?;
    write_tmpl(
        &hbs,
        "fastly_fastly_toml",
        &data,
        &fastly_dir.join("fastly.toml"),
    )?;

    // Cloudflare crate
    write_tmpl(
        &hbs,
        "cf_Cargo_toml",
        &data,
        &cloudflare_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "cf_src_main_rs",
        &data,
        &cloudflare_dir.join("src/main.rs"),
    )?;
    write_tmpl(
        &hbs,
        "cf_cargo_config_toml",
        &data,
        &cloudflare_dir.join(".cargo/config.toml"),
    )?;
    write_tmpl(
        &hbs,
        "cf_wrangler_toml",
        &data,
        &cloudflare_dir.join("wrangler.toml"),
    )?;

    println!(
        "[anyedge] created new multi-crate app at {}",
        out_dir.display()
    );
    Ok(())
}
