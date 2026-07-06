use crate::args::{NewArgs, ProvisionArgs};
use crate::provision::run_provision;
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
    /// The scaffold's per-adapter local-provision step failed. Emitted
    /// when [`generate_new`] calls [`crate::run_provision`] for each
    /// adapter declared in the newly generated `edgezero.toml` and one
    /// of those calls returns an error. Carries the failing adapter's
    /// id so operators can tell WHICH adapter's synthesise / line
    /// writer blew up without having to re-run the loop by hand.
    ///
    /// The wrapped payload is a `String` (matching `run_provision`'s
    /// error type) rather than a `Box<dyn Error>`; that lets us name
    /// it as a distinct field (`reason`, not `source`) so thiserror
    /// doesn't try to treat it as a nested `std::error::Error`.
    #[error("scaffold provision failed for adapter `{adapter}`: {reason}")]
    ProvisionFailed { adapter: String, reason: String },
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
    cli_dir: PathBuf,
    cli_name: String,
    core_dir: PathBuf,
    core_mod: String,
    core_name: String,
    crates_dir: PathBuf,
    /// `EnvPrefix` Handlebars key -- the project name normalised to
    /// the env-var prefix the runtime actually reads (uppercase,
    /// `-`→`_`). Mirrors `edgezero_core::app_config::app_name_prefix`
    /// EXACTLY so the scaffold's documentation comments name the
    /// real overlay key (e.g. `MY_APP__SERVICE__TIMEOUT_MS=...`),
    /// not the source-form lowercase (`my-app__...` would be
    /// silently ignored at runtime).
    env_prefix: String,
    name: String,
    out_dir: PathBuf,
    project_mod: String,
    /// `NameUpperCamel` Handlebars key — the project name converted to
    /// upper-camel-case (`my-app` → `MyApp`) and guaranteed to be a
    /// valid Rust type identifier. Used by the `<Name>Config`
    /// struct in the generated `config.rs` and reused by the stage-8
    /// `*-cli` template.
    upper_camel: String,
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

        let cli_name = format!("{name}-cli");
        let cli_dir = crates_dir.join(&cli_name);
        let cli_src = cli_dir.join("src");
        fs::create_dir_all(&cli_src).map_err(|err| GeneratorError::io(&cli_src, err))?;

        let project_mod = name.replace('-', "_");
        let core_mod = core_name.replace('-', "_");
        let upper_camel = upper_camel_from_sanitized(&name);
        let env_prefix = env_prefix_from_name(&name);
        Ok(ProjectLayout {
            cli_dir,
            cli_name,
            core_dir,
            core_mod,
            core_name,
            crates_dir,
            env_prefix,
            name,
            out_dir,
            project_mod,
            upper_camel,
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

/// Convert a sanitised crate name to upper-camel-case, guaranteed to be
/// a valid Rust type identifier.
///
/// Splits on `-` and `_`, drops empty segments (this naturally absorbs
/// a leading `_` that `sanitize_crate_name` may have inserted), then
/// upper-cases the first character of each segment. If the result
/// would be empty or start with a non-letter, it is prefixed with
/// `App` so the output is always a valid `struct` name.
fn upper_camel_from_sanitized(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for segment in name.split(['-', '_']).filter(|seg| !seg.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            for ch in chars {
                out.extend(ch.to_lowercase());
            }
        }
    }
    if out.is_empty() || !out.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
        let mut prefixed = String::with_capacity(out.len().saturating_add(3));
        prefixed.push_str("App");
        prefixed.push_str(&out);
        prefixed
    } else {
        out
    }
}

/// Derive the env-overlay prefix the runtime reads for this project.
///
/// MUST mirror `edgezero_core::app_config::app_name_prefix`
/// EXACTLY -- otherwise the scaffold's documentation comments
/// would advertise an env-var spelling the runtime ignores. The
/// runtime rule is `to_ascii_uppercase().replace('-', "_")`, so
/// `my-app` -> `MY_APP` and `app-demo` -> `APP_DEMO`.
fn env_prefix_from_name(name: &str) -> String {
    name.to_ascii_uppercase().replace('-', "_")
}

/// Locate the edgezero checkout that built this binary.
///
/// `CARGO_MANIFEST_DIR` is baked in at compile time and points at
/// `crates/edgezero-cli`; its grandparent is the workspace root. Returns
/// `None` when that path no longer holds a checkout (e.g. an installed
/// binary whose source tree was moved or removed), in which case
/// dependency resolution falls back to Git.
fn edgezero_repo_root() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent()?.parent()?;
    let is_checkout = root.join("crates/edgezero-cli/src/lib.rs").is_file()
        && root.join("crates/edgezero-core/src/lib.rs").is_file();
    is_checkout.then(|| root.to_path_buf())
}

/// # Errors
/// Returns [`GeneratorError`] if any filesystem operation, template render,
/// or layout invariant fails.
pub fn generate_new(args: &NewArgs) -> Result<(), GeneratorError> {
    let layout = ProjectLayout::new(args)?;

    let mut workspace_dependencies = seed_workspace_dependencies();
    // Resolve edgezero dependencies against the checkout that built this
    // binary so generated projects use path dependencies wherever they are
    // created. Only an installed binary detached from its source tree falls
    // back to the current directory (and then, typically, to Git).
    let repo_root = match edgezero_repo_root() {
        Some(root) => root,
        None => env::current_dir().map_err(|err| GeneratorError::io(".", err))?,
    };
    let core_crate_line = resolve_core_dependency(&layout, &repo_root, &mut workspace_dependencies);
    let cli_crate_line = resolve_cli_dependency(&layout, &repo_root, &mut workspace_dependencies);

    let adapter_artifacts = collect_adapter_data(&layout, &repo_root, &mut workspace_dependencies)?;

    let mut data_map = build_base_data(
        &layout,
        &core_crate_line,
        &cli_crate_line,
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
    provision_all_selected_adapters(&layout.out_dir, &adapter_artifacts.adapter_ids)?;
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
        "clap".to_owned(),
        "clap = { version = \"4\", features = [\"derive\"] }".to_owned(),
    );
    deps.insert(
        "serde".to_owned(),
        "serde = { version = \"1\", features = [\"derive\"] }".to_owned(),
    );
    deps.insert("log".to_owned(), "log = \"0.4\"".to_owned());
    deps.insert(
        "simple_logger".to_owned(),
        "simple_logger = \"5\"".to_owned(),
    );
    deps.insert(
        "worker".to_owned(),
        "worker = { version = \"0.8\", default-features = false, features = [\"http\"] }"
            .to_owned(),
    );
    deps.insert("fastly".to_owned(), "fastly = \"0.12\"".to_owned());
    deps.insert("once_cell".to_owned(), "once_cell = \"1\"".to_owned());
    deps.insert(
        "tokio".to_owned(),
        "tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }".to_owned(),
    );
    deps.insert("tracing".to_owned(), "tracing = \"0.1\"".to_owned());
    deps.insert(
        "spin-sdk".to_owned(),
        "spin-sdk = { version = \"6\", default-features = false }".to_owned(),
    );
    // Core depends on `validator` for `#[derive(Validate)]` on the
    // generated `<Name>Config` struct. Pinned to the same
    // major as the edgezero workspace so a `workspace = true` dep in
    // the generated core crate resolves cleanly.
    deps.insert(
        "validator".to_owned(),
        "validator = { version = \"0.20\", features = [\"derive\"] }".to_owned(),
    );
    deps
}

fn resolve_cli_dependency(
    layout: &ProjectLayout,
    repo_root: &Path,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> String {
    const CLI_GIT_FALLBACK: &str = "edgezero-cli = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-cli\" }";

    let ResolvedDependency {
        name,
        workspace_line,
        crate_line,
    } = resolve_dep_line(
        &layout.out_dir,
        repo_root,
        "crates/edgezero-cli",
        CLI_GIT_FALLBACK,
        &[],
    );

    if workspace_line == CLI_GIT_FALLBACK {
        log::warn!(
            "[edgezero] the generated CLI crate depends on `edgezero-cli` via a Git fallback; it will not build until `edgezero-cli` is available as a library on the referenced remote. Run `edgezero new` from inside an edgezero checkout to use a path dependency instead."
        );
    }

    workspace_dependencies.entry(name).or_insert(workspace_line);
    crate_line
}

fn resolve_core_dependency(
    layout: &ProjectLayout,
    repo_root: &Path,
    workspace_dependencies: &mut BTreeMap<String, String>,
) -> String {
    let ResolvedDependency {
        name,
        workspace_line,
        crate_line,
    } = resolve_dep_line(
        &layout.out_dir,
        repo_root,
        "crates/edgezero-core",
        "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        &[],
    );

    workspace_dependencies.entry(name).or_insert(workspace_line);
    crate_line
}

fn collect_adapter_data(
    layout: &ProjectLayout,
    repo_root: &Path,
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
            repo_root,
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
    repo_root: &Path,
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
            repo_root,
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
    cli_crate_line: &str,
    artifacts: &AdapterArtifacts,
    workspace_dependencies: &BTreeMap<String, String>,
) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("name".into(), Value::String(layout.name.clone()));
    data.insert("proj_core".into(), Value::String(layout.core_name.clone()));
    data.insert("proj_cli".into(), Value::String(layout.cli_name.clone()));
    data.insert(
        "proj_core_mod".into(),
        Value::String(layout.core_mod.clone()),
    );
    data.insert("proj_mod".into(), Value::String(layout.project_mod.clone()));
    data.insert(
        "NameUpperCamel".into(),
        Value::String(layout.upper_camel.clone()),
    );
    data.insert("EnvPrefix".into(), Value::String(layout.env_prefix.clone()));
    data.insert(
        "dep_edgezero_core".into(),
        Value::String(core_crate_line.to_owned()),
    );
    data.insert(
        "dep_edgezero_cli".into(),
        Value::String(cli_crate_line.to_owned()),
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

    data.insert(
        "tool_versions_contents".into(),
        Value::String(build_tool_versions(&artifacts.adapter_ids)),
    );

    data
}

/// Render the `.tool-versions` body for a scaffolded project,
/// adapter-aware.
///
/// `asdf install` reads this file to pin per-tool versions. Every
/// generated project gets `rust` pinned (it's a Rust workspace).
/// Per-adapter pins are added ONLY when the operator selected
/// that adapter at `edgezero new` time:
///
/// - `cloudflare` → `nodejs` (wrangler is a Node binary).
/// - `fastly` → `fastly` (the Fastly CLI we shell out to for
///   provision + config push) plus `viceroy` (what
///   `fastly compute serve` uses for local emulation).
/// - `spin` → no asdf pin; the Spin CLI is install-flow-managed
///   (<https://spinframework.dev/install>). A header comment points
///   the operator at the URL when `spin` is in the adapter set so
///   they don't wonder why everything else is pinned but spin.
/// - `axum` → no extra pin (uses the host Rust toolchain only).
///
/// Versions are pulled from this repo's own `.tool-versions` (see
/// the repo root). When we bump those, we bump these.
fn build_tool_versions(adapter_ids: &[String]) -> String {
    let has = |id: &str| adapter_ids.iter().any(|adapter| adapter == id);
    let mut lines: Vec<String> = Vec::new();
    if has("cloudflare") {
        lines.push("nodejs 24.12.0".to_owned());
    }
    if has("fastly") {
        lines.push("fastly 15.1.0".to_owned());
        lines.push("viceroy 0.17.0".to_owned());
    }
    lines.push("rust 1.95.0".to_owned());
    // Sort + dedup so the file is stable regardless of adapter
    // declaration order (and asdf doesn't care).
    lines.sort();
    lines.dedup();
    let mut body = lines.join("\n");
    if has("spin") {
        body.push_str(
            "\n\n# Spin is not asdf-managed in this scaffold; install via\n# https://spinframework.dev/install\n",
        );
    } else {
        body.push('\n');
    }
    body
}

/// Render the six workspace-root files (Cargo.toml, edgezero.toml,
/// README.md, .gitignore, clippy.toml, .tool-versions).
///
/// Split out of `render_templates` so the parent stays under the
/// project's `too_many_lines` clippy cap; the order of writes is
/// not load-bearing — each template is independent.
fn write_root_files(
    hbs: &Handlebars,
    layout: &ProjectLayout,
    data_value: &Value,
) -> Result<(), GeneratorError> {
    for (template, rel) in [
        ("root_Cargo_toml", "Cargo.toml"),
        ("root_edgezero_toml", "edgezero.toml"),
        ("root_README_md", "README.md"),
        ("root_gitignore", ".gitignore"),
        ("root_clippy_toml", "clippy.toml"),
        ("root_tool_versions", ".tool-versions"),
    ] {
        write_tmpl(hbs, template, data_value, &layout.out_dir.join(rel))?;
    }
    Ok(())
}

fn render_templates(
    layout: &ProjectLayout,
    adapter_contexts: &[AdapterContext],
    data_value: &Value,
) -> Result<(), GeneratorError> {
    let mut hbs = Handlebars::new();
    register_templates(&mut hbs).map_err(ScaffoldError::from)?;

    log::info!("[edgezero] writing workspace files");
    write_root_files(&hbs, layout, data_value)?;

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
    write_tmpl(
        &hbs,
        "core_src_config_rs",
        data_value,
        &layout.core_dir.join("src/config.rs"),
    )?;
    write_tmpl(
        &hbs,
        "app_name_toml",
        data_value,
        &layout.out_dir.join(format!("{}.toml", layout.name)),
    )?;

    log::info!("[edgezero] writing cli crate {}", layout.cli_name);
    write_tmpl(
        &hbs,
        "cli_Cargo_toml",
        data_value,
        &layout.cli_dir.join("Cargo.toml"),
    )?;
    write_tmpl(
        &hbs,
        "cli_src_main_rs",
        data_value,
        &layout.cli_dir.join("src/main.rs"),
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

/// Run `run_provision --local` once per adapter declared in the
/// newly generated project's manifest.
///
/// This is the scaffold-time counterpart of the operator running
/// `edgezero provision --adapter <id> --local` after the fact: it
/// drives each adapter's `synthesise_baseline_manifest` +
/// local-provision writers so a fresh `edgezero new` output has its
/// per-adapter local files populated (wrangler.toml, .dev.vars,
/// spin.toml, runtime-config.toml, spin's `.env`, axum's
/// `.edgezero/.env`, fastly's `[local_server.*]` entries) without a
/// second command.
///
/// Uses the UNTYPED [`run_provision`] on purpose. The generator has
/// no downstream `C` type in scope — typed-secret placeholders
/// (`SPIN_VARIABLE_*` etc.) land later, when the operator first runs
/// the generated downstream CLI's `provision` (which routes through
/// `run_provision_typed`).
///
/// `ProvisionArgs` is `#[non_exhaustive]`. We build it via
/// `Default::default()` + per-field assignment (using `clone_from`
/// where the source is a reference, per `assigning_clones`) rather
/// than struct-update syntax so a future field addition doesn't
/// silently regress the default-value contract.
fn provision_all_selected_adapters(
    project_root: &Path,
    adapter_ids: &[String],
) -> Result<(), GeneratorError> {
    let manifest_path = project_root.join("edgezero.toml");
    for adapter_id in adapter_ids {
        let mut prov_args = ProvisionArgs::default();
        prov_args.adapter.clone_from(adapter_id);
        prov_args.local = true;
        prov_args.dry_run = false;
        prov_args.manifest.clone_from(&manifest_path);
        run_provision(&prov_args).map_err(|reason| GeneratorError::ProvisionFailed {
            adapter: adapter_id.clone(),
            reason,
        })?;
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
    #[cfg(unix)]
    use crate::shared_test_guards::path_mutation_guard;
    use edgezero_core::app_config::app_name_prefix;
    use std::path::Path;
    #[cfg(unix)]
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    // `super::*` re-exports `env` and `fs` from outer `use` lines, so they're
    // already in scope here.

    // Holds the shared crate-level PATH guard for the lifetime of the
    // override so scaffold tests running concurrently with config's
    // push-shim tests can't stomp each other's PATH restores.

    #[cfg(unix)]
    struct PathOverride {
        _guard: MutexGuard<'static, ()>,
        original: Option<String>,
    }
    #[cfg(not(unix))]
    struct PathOverride {
        original: Option<String>,
    }

    impl PathOverride {
        fn prepend(path: &Path) -> Self {
            #[cfg(unix)]
            let guard = path_mutation_guard()
                .lock()
                .expect("PATH mutation guard poisoned");
            let original = env::var("PATH").ok();
            let sep = if cfg!(windows) { ";" } else { ":" };
            let prefix = path.to_string_lossy();
            let new_path = match &original {
                Some(existing) if !existing.is_empty() => format!("{prefix}{sep}{existing}"),
                _ => prefix.into_owned(),
            };
            env::set_var("PATH", &new_path);
            #[cfg(unix)]
            {
                Self {
                    original,
                    _guard: guard,
                }
            }
            #[cfg(not(unix))]
            {
                Self { original }
            }
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
    fn upper_camel_from_sanitized_covers_derivation_rules() {
        // Hyphen and underscore both split into PascalCase segments.
        assert_eq!(upper_camel_from_sanitized("my-app"), "MyApp");
        // Single segment is just capitalised.
        assert_eq!(upper_camel_from_sanitized("foo"), "Foo");
        // Mixed separators: each non-empty segment contributes one capital.
        assert_eq!(upper_camel_from_sanitized("a_b-c"), "ABC");
        // `sanitize_crate_name` may emit a leading `_` for digit-leading
        // input; the empty leading segment from the split is dropped.
        assert_eq!(upper_camel_from_sanitized("_foo"), "Foo");
        // Digit-leading produces a digit-leading PascalCase result, which
        // would be an invalid Rust ident, so we prefix `App`.
        assert_eq!(upper_camel_from_sanitized("123-app"), "App123App");
    }

    #[test]
    fn env_prefix_from_name_matches_runtime_app_name_prefix_exactly() {
        // The scaffold's documentation has to advertise the exact
        // env-var spelling the runtime reads, not the source-form
        // lowercase. Mirror `edgezero_core::app_config::app_name_prefix`
        // EXACTLY: uppercase, `-`→`_`. A drift here would teach
        // operators to set `my-app__SERVICE__TIMEOUT_MS=...` which
        // the runtime silently ignores.
        assert_eq!(env_prefix_from_name("my-app"), "MY_APP");
        assert_eq!(env_prefix_from_name("app-demo"), "APP_DEMO");
        assert_eq!(env_prefix_from_name("foo"), "FOO");
        assert_eq!(env_prefix_from_name("a_b-c"), "A_B_C");
        // Digit-leading: sanitize_crate_name emits `_123app` -- the
        // underscore is preserved and the uppercase form is correct
        // for the runtime overlay.
        assert_eq!(env_prefix_from_name("_123app"), "_123APP");
    }

    #[test]
    fn env_prefix_from_name_agrees_with_runtime_app_name_prefix() {
        // Pin agreement with the runtime by calling the actual
        // runtime function. If a future change to
        // `edgezero_core::app_config::app_name_prefix` updates the
        // normalisation rule (adds character handling, strips a
        // prefix, etc.) without a matching change here, this test
        // catches the drift immediately and the scaffold's
        // documentation stays correct.
        for name in ["app-demo", "my-app", "foo", "a-b-c", "x", "_123app"] {
            let runtime_shape = app_name_prefix(name);
            assert_eq!(
                env_prefix_from_name(name),
                runtime_shape,
                "scaffold env_prefix_from_name drifted from runtime app_name_prefix for {name:?}"
            );
        }
    }

    // ---------- build_tool_versions ----------

    #[test]
    fn build_tool_versions_pins_rust_only_with_no_adapters() {
        // The scaffolder always picks at least axum in practice,
        // but the empty case is the trust boundary: zero adapters
        // produces a stable file containing exactly the
        // rust-toolchain pin and a trailing newline.
        let out = build_tool_versions(&[]);
        assert_eq!(out, "rust 1.95.0\n");
    }

    #[test]
    fn build_tool_versions_pins_nodejs_when_cloudflare_adapter_selected() {
        // wrangler is a Node binary; pinning nodejs keeps the
        // version we tested wrangler against (the same nodejs the
        // repo's own `.tool-versions` pins).
        let out = build_tool_versions(&["cloudflare".to_owned()]);
        assert!(out.contains("nodejs 24.12.0"), "must pin nodejs: {out}");
        assert!(out.contains("rust 1.95.0"), "always pin rust: {out}");
        assert!(
            !out.contains("fastly"),
            "no fastly pin without fastly adapter: {out}"
        );
    }

    #[test]
    fn build_tool_versions_pins_fastly_and_viceroy_when_fastly_adapter_selected() {
        // `fastly` for the CLI we shell out to in provision /
        // config push; `viceroy` for `fastly compute serve`. Both
        // pins are needed when the operator actually uses the
        // fastly adapter; we pin them ONLY here so a CF-only
        // project doesn't end up with a fastly-CLI install
        // requirement.
        let out = build_tool_versions(&["fastly".to_owned()]);
        assert!(out.contains("fastly 15.1.0"), "must pin fastly: {out}");
        assert!(out.contains("viceroy 0.17.0"), "must pin viceroy: {out}");
        assert!(out.contains("rust 1.95.0"));
        assert!(
            !out.contains("nodejs"),
            "no nodejs pin without cloudflare adapter: {out}"
        );
    }

    #[test]
    fn build_tool_versions_axum_only_does_not_add_extra_pins() {
        // Axum runs on the host Rust toolchain, no extra binaries
        // needed — same as the "no adapters" shape but exercised
        // through the realistic axum-only case.
        let out = build_tool_versions(&["axum".to_owned()]);
        assert_eq!(out, "rust 1.95.0\n");
    }

    #[test]
    fn build_tool_versions_spin_adds_install_hint_comment_not_asdf_pin() {
        // Spin is install-flow-managed (not consistently asdf-
        // managed in our toolchain), so don't write a brittle pin
        // we can't honour — explain why with an inline hint so the
        // operator isn't left guessing.
        let out = build_tool_versions(&["spin".to_owned()]);
        assert!(out.contains("rust 1.95.0"));
        assert!(
            !out.contains("spin "),
            "must NOT pin spin via asdf shape: {out}"
        );
        assert!(
            out.contains("spinframework.dev/install"),
            "must point operators at the spin install URL: {out}"
        );
    }

    #[test]
    fn build_tool_versions_all_four_adapters_combines_pins_deterministically() {
        // Composite case: the typical generated project has all
        // four adapters. Output must list each pin exactly once
        // (no duplicates from accidental double-insertion in the
        // adapter loop), and be sort-stable so the file doesn't
        // churn across regenerations.
        let adapters = vec![
            "cloudflare".to_owned(),
            "fastly".to_owned(),
            "spin".to_owned(),
            "axum".to_owned(),
        ];
        let out = build_tool_versions(&adapters);
        // Each pin appears exactly once.
        for pin in [
            "nodejs 24.12.0",
            "fastly 15.1.0",
            "viceroy 0.17.0",
            "rust 1.95.0",
        ] {
            assert_eq!(
                out.matches(pin).count(),
                1,
                "`{pin}` must appear exactly once in: {out}"
            );
        }
        // Spin install hint present.
        assert!(out.contains("spinframework.dev/install"));
        // Stable order (alphabetical).
        let pin_block = out.split("\n\n").next().expect("pin block").to_owned();
        let lines: Vec<&str> = pin_block.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(
            lines, sorted,
            "pin lines must be sorted so the file is regen-stable: {pin_block}"
        );
    }

    #[test]
    fn generator_error_format_displays_underlying_fmt_error() {
        // `writeln!`-to-`String` cannot actually fail in production, but the
        // variant is part of the public error surface and `From<fmt::Error>`
        // wiring must keep working. Construct one and verify the Display
        // string carries the underlying error.
        let err: GeneratorError = fmt::Error.into();
        assert!(matches!(err, GeneratorError::Format(_)));
        assert!(err
            .to_string()
            .contains("failed to format generator output"));
    }

    fn write_git_stub(bin_dir: &Path) {
        fs::create_dir_all(bin_dir).expect("bin dir");
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
        }
    }

    fn assert_scaffold_files(project_dir: &Path) {
        assert!(project_dir.is_dir(), "project directory created");
        assert!(project_dir.join("Cargo.toml").exists());
        assert!(project_dir.join("edgezero.toml").exists());
        assert!(project_dir.join(".gitignore").exists());
        assert!(project_dir.join(".tool-versions").exists());
        assert!(project_dir.join("README.md").exists());
        assert!(project_dir.join("crates/demo-app-core/src/lib.rs").exists());
        assert!(
            project_dir.join("crates/demo-app-cli/Cargo.toml").exists(),
            "<name>-cli crate Cargo.toml should be scaffolded"
        );
        assert!(
            project_dir.join("crates/demo-app-cli/src/main.rs").exists(),
            "<name>-cli crate main.rs should be scaffolded"
        );
        assert!(
            project_dir
                .join("crates/demo-app-adapter-spin/spin.toml")
                .exists(),
            "spin.toml should be scaffolded"
        );
    }

    fn assert_scaffold_app_config(project_dir: &Path) {
        // `<name>.toml` and `<name>-core/src/config.rs` must be produced,
        // with the `<NameUpperCamel>Config` struct named after the project
        // (`demo-app` → `DemoAppConfig`).
        let app_toml_path = project_dir.join("demo-app.toml");
        assert!(
            app_toml_path.exists(),
            "<name>.toml should be scaffolded at the project root"
        );
        let app_toml = fs::read_to_string(&app_toml_path).expect("read demo-app.toml");
        // Parse the file rather than substring-matching on a comment.
        // The shape contract is "the root table IS the typed struct; no
        // `[config]` wrapper". A regression that re-introduces `[config
        // = ...]` or `[config.service]` would otherwise pass a
        // comment-only check.
        let parsed: toml::Value = toml::from_str(&app_toml).expect("parse demo-app.toml");
        let root = parsed.as_table().expect("root is a TOML table");
        assert!(
            root.get("config").is_none(),
            "<name>.toml must not have a top-level `config` key: the file is the typed struct"
        );
        assert!(
            root.get("service").is_some_and(toml::Value::is_table),
            "<name>.toml must declare `[service]` at the root, not nested under `[config]`"
        );

        let config_rs_path = project_dir.join("crates/demo-app-core/src/config.rs");
        assert!(
            config_rs_path.exists(),
            "<name>-core/src/config.rs should be scaffolded"
        );
        let config_rs = fs::read_to_string(&config_rs_path).expect("read config.rs");
        assert!(
            config_rs.contains("pub struct DemoAppConfig"),
            "config.rs must declare the DemoAppConfig struct"
        );
        assert!(
            config_rs.contains("edgezero_core::AppConfig"),
            "config.rs must derive edgezero_core::AppConfig"
        );

        // The scaffold's env-overlay documentation must name the
        // ACTUAL prefix the runtime reads -- `DEMO_APP__SERVICE__TIMEOUT_MS`
        // for project `demo-app`. A regression that reintroduced
        // `{{name}}__...` in the templates would render as
        // `demo-app__...` here and teach operators an env-var
        // spelling the runtime silently ignores. Both the typed
        // struct's rustdoc AND `<name>.toml`'s comment block must
        // pass this check.
        assert!(
            config_rs.contains("DEMO_APP__SERVICE__TIMEOUT_MS"),
            "config.rs rustdoc must advertise the DEMO_APP__-prefixed env override: {config_rs}"
        );
        assert!(
            !config_rs.contains("demo-app__") && !config_rs.contains("demo_app__SERVICE"),
            "config.rs must NOT show source-form lowercase env prefixes: {config_rs}"
        );
        assert!(
            app_toml.contains("DEMO_APP__"),
            "<name>.toml env-overlay comment must use the DEMO_APP__ prefix: {app_toml}"
        );
        assert!(
            !app_toml.contains("demo-app__") && !app_toml.contains("demo_app__SERVICE"),
            "<name>.toml must NOT show source-form lowercase env prefixes: {app_toml}"
        );

        let core_cargo = fs::read_to_string(project_dir.join("crates/demo-app-core/Cargo.toml"))
            .expect("read core Cargo.toml");
        assert!(
            core_cargo.contains("validator = { workspace = true }"),
            "<name>-core Cargo.toml must pull validator from the workspace"
        );

        let core_lib = fs::read_to_string(project_dir.join("crates/demo-app-core/src/lib.rs"))
            .expect("read core lib.rs");
        assert!(
            core_lib.contains("pub mod config"),
            "<name>-core lib.rs must expose the config module so consumers can reach DemoAppConfig"
        );

        let workspace_cargo =
            fs::read_to_string(project_dir.join("Cargo.toml")).expect("read workspace Cargo.toml");
        assert!(
            workspace_cargo.contains("validator = { version ="),
            "workspace Cargo.toml must seed the validator dependency"
        );
    }

    fn assert_scaffold_workspace(project_dir: &Path) {
        let cargo_toml =
            fs::read_to_string(project_dir.join("Cargo.toml")).expect("read Cargo.toml");
        for member in [
            "crates/demo-app-core",
            "crates/demo-app-cli",
            "crates/demo-app-adapter-cloudflare",
            "crates/demo-app-adapter-fastly",
            "crates/demo-app-adapter-spin",
        ] {
            assert!(
                cargo_toml.contains(member),
                "workspace Cargo.toml should include {member}"
            );
        }
        assert!(cargo_toml.contains("[workspace.lints.clippy]"));
        assert!(cargo_toml.contains("blanket_clippy_restriction_lints = \"allow\""));

        // Generated from a checkout: edgezero crates must resolve to local
        // path dependencies, not the Git fallback (whose `edgezero-cli` has
        // no library target until this work is published).
        assert!(
            cargo_toml.contains("edgezero-cli = { path ="),
            "edgezero-cli must resolve to a local path dependency"
        );
        assert!(
            cargo_toml.contains("edgezero-core = { path ="),
            "edgezero-core must resolve to a local path dependency"
        );

        let manifest =
            fs::read_to_string(project_dir.join("edgezero.toml")).expect("read edgezero.toml");
        assert!(manifest.contains("[adapters.cloudflare.adapter]"));
        assert!(manifest.contains("[adapters.fastly.adapter]"));
        assert!(
            manifest.contains("[adapters.spin"),
            "edgezero.toml should include spin adapter section"
        );

        let gitignore =
            fs::read_to_string(project_dir.join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.contains("target/"));

        // Provision-owned manifests are regenerated by `provision
        // --local`, so teammates must not commit somebody else's
        // per-machine ids / operator-set defaults. All five adapter
        // manifests are provision-generated; `axum.toml` joined the
        // list when Axum's `synthesise_baseline_manifest` was wired
        // up (2026-07 refactor).
        for entry in [
            "axum.toml",
            "fastly.toml",
            "spin.toml",
            "wrangler.toml",
            "runtime-config.toml",
            ".edgezero/",
            ".wrangler/",
            ".spin/",
            ".dev.vars",
            ".env",
        ] {
            assert!(
                gitignore.contains(entry),
                ".gitignore missing provision-owned entry `{entry}`: {gitignore}"
            );
        }

        let clippy = fs::read_to_string(project_dir.join("clippy.toml")).expect("read clippy.toml");
        assert!(clippy.contains("allow-expect-in-tests = true"));

        // Regression: the pre-fix `fastly.toml.hbs` template shipped a
        // literal `service_id = ""` line. `write_baseline_to_disk` skips
        // existing files, so the scaffold-then-provision flow left the
        // empty string in place — bypassing the synthesiser's "omit
        // service_id until deployed" invariant. Assert no
        // scaffolded/provisioned fastly.toml carries an empty
        // `service_id` after `edgezero new`.
        let fastly_toml_path = project_dir.join("crates/demo-app-adapter-fastly/fastly.toml");
        if fastly_toml_path.exists() {
            let fastly_toml = fs::read_to_string(&fastly_toml_path).expect("read fastly.toml");
            assert!(
                !fastly_toml.contains("service_id = \"\""),
                "fastly.toml must not carry an empty service_id placeholder \
                 (synthesise_fastly_toml omits it when None): {fastly_toml}"
            );
        }
    }

    fn assert_scaffold_crate_lints(project_dir: &Path) {
        for crate_dir in [
            "crates/demo-app-core",
            "crates/demo-app-cli",
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

        assert_generated_sources_are_lint_clean(project_dir);
    }

    /// Regression guard for the generated sources: a freshly scaffolded
    /// project must pass its own `restriction`-deny clippy gate. The pre-fix
    /// templates shipped a production `.expect(...)` in the `stream` handler,
    /// infallible `IntoResponse` test usage, and adapter host stubs that
    /// tripped `print_stderr` / `exit`.
    fn assert_generated_sources_are_lint_clean(project_dir: &Path) {
        let handlers = fs::read_to_string(project_dir.join("crates/demo-app-core/src/handlers.rs"))
            .expect("read handlers.rs");
        assert!(
            handlers.contains("pub async fn stream() -> Result<Response, EdgeError>"),
            "stream handler must be fallible, not panic via expect()",
        );
        assert!(
            !handlers.contains("static stream response"),
            "handler template must not ship a production expect()",
        );
        assert!(
            handlers.contains(".into_response()"),
            "handler tests must use the fallible IntoResponse pattern",
        );

        let axum_main =
            fs::read_to_string(project_dir.join("crates/demo-app-adapter-axum/src/main.rs"))
                .expect("read axum main.rs");
        assert!(
            !axum_main.contains("process::exit"),
            "axum host entrypoint must return Result, not call process::exit",
        );

        let fastly_main =
            fs::read_to_string(project_dir.join("crates/demo-app-adapter-fastly/src/main.rs"))
                .expect("read fastly main.rs");
        assert!(
            fastly_main.contains("reason ="),
            "adapter attributes must carry a reason for allow_attributes_without_reason",
        );
    }

    /// Walker regression guarding the class of bug that shipped
    /// `service_id = ""` in `fastly.toml.hbs` for months undetected.
    ///
    /// Every `*.toml.hbs` scaffold template that goes through
    /// `provision --local` is checked for empty-string placeholders on
    /// keys that provision would upsert. A `key = ""` line at the top
    /// of the template gets past `write_baseline_to_disk` (skips
    /// existing files) and reaches the emitted project's manifest
    /// unchanged -- the synthesiser's "omit key until real" invariant
    /// is silently bypassed. Any template that intentionally ships a
    /// blank value must comment the line (`# key = ""`) or add an
    /// exception below with an explicit reason.
    ///
    /// Rule: for every non-comment, non-blank line of the form
    /// `<key> = ""` (double or single quotes, optional whitespace),
    /// flag it. `authors = [""]` (empty-string-in-array, standard Cargo
    /// idiom) is allowed because `[...]` is the containing token, not
    /// `""`.
    #[test]
    fn adapter_hbs_templates_have_no_empty_string_placeholders() {
        // Resolve the workspace root from CARGO_MANIFEST_DIR
        // (crates/edgezero-cli) -> ../..
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();

        let mut offenders: Vec<String> = Vec::new();
        let crates_dir = workspace_root.join("crates");
        walk_toml_templates(&crates_dir, &mut |path, body| {
            for (idx, raw_line) in body.lines().enumerate() {
                let trimmed = raw_line.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                // `key = ""` or `key = ''` (permit optional spaces
                // around the =). Bail on any array / table forms.
                let empty_double = trimmed.contains("= \"\"") || trimmed.ends_with("=\"\"");
                let empty_single = trimmed.contains("= ''") || trimmed.ends_with("=''");
                if empty_double || empty_single {
                    offenders.push(format!(
                        "{}:{} — `{}`",
                        path.display(),
                        idx + 1,
                        raw_line.trim_end()
                    ));
                }
            }
        });

        assert!(
            offenders.is_empty(),
            "Template hygiene violation: `key = \"\"` lines below would ship as empty placeholders through `provision --local` because write_baseline_to_disk skips existing files. Comment the line, remove it, or fill in a real default:\n{}",
            offenders.join("\n")
        );
    }

    /// Depth-first walk of every `*.toml.hbs` file under `root`. Skips
    /// `target/` and `.git/` on principle even though they shouldn't
    /// contain templates. Reads each hit into memory and invokes
    /// `visit(path, body)`. Non-toml `.hbs` files are ignored -- only
    /// TOML has the "root scalar becomes nested under a header on
    /// re-emit" class of bug and only TOML has empty-placeholder
    /// semantics we care about.
    fn walk_toml_templates(root: &Path, visit: &mut dyn FnMut(&Path, &str)) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if path.is_dir() {
                if name_str == "target" || name_str == ".git" {
                    continue;
                }
                walk_toml_templates(&path, visit);
                continue;
            }
            if name_str.ends_with(".toml.hbs") {
                if let Ok(body) = fs::read_to_string(&path) {
                    visit(&path, &body);
                }
            }
        }
    }

    #[test]
    fn generate_new_scaffolds_workspace_layout() {
        let temp = TempDir::new().expect("temp dir");
        let bin_dir = temp.path().join("bin");
        write_git_stub(&bin_dir);
        let _path_guard = PathOverride::prepend(&bin_dir);

        let args = NewArgs {
            name: "demo-app".into(),
            dir: Some(temp.path().to_string_lossy().into_owned()),
        };

        generate_new(&args).expect("scaffold succeeds");

        let project_dir = temp.path().join("demo-app");
        assert_scaffold_files(&project_dir);
        assert_scaffold_workspace(&project_dir);
        assert_scaffold_app_config(&project_dir);
        assert_scaffold_crate_lints(&project_dir);
        assert_scaffold_cli_full_command_set(&project_dir);
    }

    /// The scaffolded `<name>-cli` must
    /// expose the full seven-command surface (`Build`, `Deploy`,
    /// `New`, `Serve`, `Auth`, `Provision`, `Config(Validate|Push)`)
    /// and wire the `Config` arm to the **typed** entry points
    /// parameterised over `<NameUpperCamel>Config` from the
    /// project's core crate. Without these, a freshly-scaffolded
    /// project would silently lose access to commands that landed
    /// in Stages 4–7.
    fn assert_scaffold_cli_full_command_set(project_dir: &Path) {
        let cargo_path = project_dir.join("crates/demo-app-cli/Cargo.toml");
        let cargo = fs::read_to_string(&cargo_path).expect("read cli Cargo.toml");
        assert!(
            cargo.contains("demo-app-core = { path = \"../demo-app-core\" }"),
            "<name>-cli/Cargo.toml must depend on <name>-core (typed config lives there): {cargo}"
        );

        let main_path = project_dir.join("crates/demo-app-cli/src/main.rs");
        let main = fs::read_to_string(&main_path).expect("read cli main.rs");

        // Imports — every args type the seven-command Cmd enum
        // references must be in scope.
        for import in [
            "AuthArgs",
            "BuildArgs",
            "ConfigPushArgs",
            "ConfigValidateArgs",
            "DeployArgs",
            "NewArgs",
            "ProvisionArgs",
            "ServeArgs",
        ] {
            assert!(
                main.contains(import),
                "<name>-cli/src/main.rs must import `{import}`: {main}"
            );
        }

        // Use `{{proj_core_mod}}` for the core crate's *Rust module*
        // name, not the package name with a `_core` suffix —
        // `demo-app_core` (mixing `-` and `_`) is invalid Rust.
        assert!(
            main.contains("use demo_app_core::config::DemoAppConfig;"),
            "<name>-cli must import the typed config via the underscored core module name: {main}"
        );

        // Cmd variants — all seven plus the nested ConfigCmd.
        for variant in [
            "Auth(AuthArgs)",
            "Build(BuildArgs)",
            "Config(DemoAppConfigCmd)",
            "Deploy(DeployArgs)",
            "New(NewArgs)",
            "Provision(ProvisionArgs)",
            "Serve(ServeArgs)",
        ] {
            assert!(
                main.contains(variant),
                "<name>-cli Cmd must include `{variant}`: {main}"
            );
        }

        // Typed dispatch — the whole reason a downstream CLI
        // exists. Raw push/validate would defeat the point. Provision
        // routes through the typed variant so #[secret] fields on the
        // downstream config reach adapter provision_typed impls.
        for call in [
            "run_config_push_typed::<DemoAppConfig>",
            "run_config_validate_typed::<DemoAppConfig>",
            "run_provision_typed::<DemoAppConfig>",
            "edgezero_cli::run_auth",
        ] {
            assert!(
                main.contains(call),
                "<name>-cli main.rs must dispatch via `{call}`: {main}"
            );
        }
        // Negative: the untyped variant must not survive template
        // regeneration — Task 30's whole point was to eliminate the
        // bypass where scaffolded CLIs would silently skip typed
        // secret writeback.
        assert!(
            !main.contains("edgezero_cli::run_provision(&args)"),
            "<name>-cli main.rs must NOT call untyped run_provision: {main}"
        );
    }

    /// Task 31: after `generate_new` returns, every adapter declared
    /// in the generated `edgezero.toml` must have already run its
    /// local-mode `provision`, so the Cloudflare and Spin per-crate
    /// files (synthesised platform manifests + `.env`-style line
    /// writers) exist on disk without a second command.
    #[test]
    fn generate_new_provisions_cloudflare_and_spin_scaffold_artifacts() {
        let temp = TempDir::new().expect("temp dir");

        let args = NewArgs {
            name: "demo-app".into(),
            dir: Some(temp.path().to_string_lossy().into_owned()),
        };

        generate_new(&args).expect("scaffold succeeds");

        let project_dir = temp.path().join("demo-app");
        let cf_crate = project_dir.join("crates/demo-app-adapter-cloudflare");
        let spin_crate = project_dir.join("crates/demo-app-adapter-spin");

        assert!(
            cf_crate.join("wrangler.toml").exists(),
            "cloudflare wrangler.toml must be synthesised at scaffold time"
        );
        assert!(
            cf_crate.join(".dev.vars").exists(),
            "cloudflare .dev.vars must be created at scaffold time (line writer)"
        );
        assert!(
            spin_crate.join("spin.toml").exists(),
            "spin spin.toml must be synthesised at scaffold time"
        );
        assert!(
            spin_crate.join("runtime-config.toml").exists(),
            "spin runtime-config.toml must be synthesised at scaffold time"
        );
        assert!(
            spin_crate.join(".env").exists(),
            "spin per-crate .env must be created at scaffold time (line writer)"
        );
    }

    /// Task 31: after `generate_new` returns, the axum adapter's
    /// local-state directory (`.edgezero/`) and its `.env`
    /// placeholder file must already exist at the project root,
    /// courtesy of the scaffold-time provision loop dispatching to
    /// the axum adapter's local writer.
    #[test]
    fn generate_new_provisions_axum_dot_edgezero_env() {
        let temp = TempDir::new().expect("temp dir");

        let args = NewArgs {
            name: "demo-app".into(),
            dir: Some(temp.path().to_string_lossy().into_owned()),
        };

        generate_new(&args).expect("scaffold succeeds");

        let project_dir = temp.path().join("demo-app");
        assert!(
            project_dir.join(".edgezero").is_dir(),
            ".edgezero/ must exist post-scaffold when axum is in the adapter set"
        );
        assert!(
            project_dir.join(".edgezero/.env").exists(),
            ".edgezero/.env must be seeded by axum's local provision writer"
        );
    }

    /// Task 31: when any adapter's provision call fails, the error
    /// bubbles out of the loop with the failing adapter's id, so
    /// operators can tell WHICH adapter blew up. Exercised via a
    /// direct call to the helper with a project root that has no
    /// `edgezero.toml` — `run_provision`'s manifest loader fails
    /// immediately and the loop wraps the error in
    /// `GeneratorError::ProvisionFailed { adapter, .. }`.
    #[test]
    fn provision_all_selected_adapters_surfaces_adapter_name_on_failure() {
        let temp = TempDir::new().expect("temp dir");
        let project_root = temp.path();
        // Deliberately do NOT create edgezero.toml — `run_provision`
        // will fail at `ManifestLoader::from_path`.
        let err = provision_all_selected_adapters(project_root, &["axum".to_owned()])
            .expect_err("provision must fail without a manifest");
        let GeneratorError::ProvisionFailed { adapter, reason } = &err else {
            panic!("expected ProvisionFailed, got {err:?}");
        };
        assert_eq!(adapter, "axum", "must name the failing adapter");
        assert!(
            !reason.is_empty(),
            "wrapped reason must carry the underlying error"
        );
        // Display string also carries the adapter name.
        let msg = err.to_string();
        assert!(
            msg.contains("axum"),
            "Display must surface failing adapter name: {msg}"
        );
    }
}
