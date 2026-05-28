use std::env;
use std::fs;
use std::io::ErrorKind;
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

static FASTLY_ADAPTER: FastlyCliAdapter = FastlyCliAdapter;

static FASTLY_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "fastly",
    display_name: "Fastly Compute@Edge",
    crate_suffix: "adapter-fastly",
    dependency_crate: "edgezero-adapter-fastly",
    dependency_repo_path: "crates/edgezero-adapter-fastly",
    template_registrations: FASTLY_TEMPLATE_REGISTRATIONS,
    files: FASTLY_FILE_SPECS,
    extra_dirs: &["src", ".cargo"],
    dependencies: FASTLY_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "fastly.toml",
        build_target: "wasm32-wasip1",
        build_profile: "release",
        build_features: &["fastly"],
    },
    commands: CommandTemplates {
        build: "fastly compute build -C {crate_dir}",
        deploy: "fastly compute deploy -C {crate_dir}",
        serve: "fastly compute serve -C {crate_dir}",
    },
    logging: LoggingDefaults {
        endpoint: Some("stdout"),
        level: "info",
        echo_stdout: Some(true),
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`cd {crate_dir}`", "`edgezero serve --adapter fastly`"],
    },
    run_module: "edgezero_adapter_fastly",
};

static FASTLY_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_fastly",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback:
            "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly_wasm",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback:
            "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false, features = [\"fastly\"] }",
        features: &["fastly"],
    },
];

static FASTLY_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "fastly_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "fastly_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "fastly_cargo_config_toml",
        output: ".cargo/config.toml",
    },
    AdapterFileSpec {
        template: "fastly_fastly_toml",
        output: "fastly.toml",
    },
];

static FASTLY_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "fastly_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "fastly_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_fastly_toml",
        contents: include_str!("templates/fastly.toml.hbs"),
    },
];

const FASTLY_INSTALL_HINT: &str =
    "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

struct FastlyCliAdapter;

/// Outcome of scanning `fastly config-store list --json` for a
/// platform store id by `name`. Distinguishes three cases the
/// caller wants to act on differently:
///
/// - `Found(id)` — happy path.
/// - `NotFound` — JSON parsed cleanly and the array contains
///   entries with well-formed `name` + `id` string fields, but no
///   entry matched `name`. Operator likely needs to run
///   `provision`.
/// - `SchemaDrift(detail)` — the JSON parsed but doesn't match
///   the expected shape (no `items` envelope nor bare array, OR
///   entries are missing `name` / `id` string fields, OR the
///   bytes didn't parse as JSON at all). Likely a fastly CLI
///   version bump that changed the output schema; surface the
///   detail so the operator can pin a known-compatible version.
#[derive(Debug)]
enum ConfigStoreLookup {
    Found(String),
    NotFound,
    SchemaDrift(String),
}

// The three `validate_*` trait methods exist on `Adapter` because
// spin requires them (variable-name regex, `[component.*]`
// discovery, flat-namespace collision). The trait surface is typed
// generically so any future adapter with similar constraints can
// override — but fastly has no equivalent platform requirements,
// so the no-op defaults are correct:
//
// - `validate_app_config_keys`: Fastly Config Store keys accept
//   alphanumeric + `-` / `_` / `.` up to 256 chars. Any reasonable
//   Rust struct field name passes; no regex check needed.
// - `validate_adapter_manifest`: would require shelling out to
//   `fastly compute validate` at validate-time. We keep
//   `config validate` pure-Rust so it stays fast and
//   tool-independent.
// - `validate_typed_secrets`: Fastly's KV / Config / Secret
//   stores are independent namespaces — no spin-style flat-
//   namespace collision risk to detect.
//
// `single_store_kinds` IS overridden below — explicitly returns
// `&[]` for documentation, matching the inherited default.
#[expect(
    clippy::missing_trait_methods,
    reason = "see the explanatory block comment immediately above; fastly's no-op defaults for the three validate_* hooks are intentional and documented"
)]
impl Adapter for FastlyCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `fastly profile {create|delete|list}` is the native
            // sign-in surface for Fastly Compute. EdgeZero stores no
            // credentials — this is a thin shell-out.
            AdapterAction::AuthLogin => {
                run_native_cli("fastly", &["profile", "create"], FASTLY_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("fastly", &["profile", "delete"], FASTLY_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("fastly", &["profile", "list"], FASTLY_INSTALL_HINT)
            }
            AdapterAction::Build => {
                let artifact = build(args)?;
                log::info!("[edgezero] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("fastly adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "fastly"
    }

    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Fastly is Multi for every store kind. Each id maps 1:1
        // to a Fastly resource (kv-store / config-store /
        // secret-store) created via the Fastly CLI; the manifest
        // writeback declares the resource link for `fastly
        // compute deploy` and the local viceroy server.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for provision"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);

        let mut out = Vec::new();
        for (kind, ids) in [
            ("kv", stores.kv),
            ("config", stores.config),
            ("secret", stores.secrets),
        ] {
            for id in ids {
                if dry_run {
                    out.push(format!(
                        "would run `fastly {kind}-store create --name={id}` and append [setup.{kind}_stores.{id}] / [local_server.{kind}_stores.{id}] to {}",
                        fastly_path.display()
                    ));
                    continue;
                }
                if setup_block_present(&fastly_path, kind, id)? {
                    out.push(format!(
                        "fastly {kind}-store `{id}` already declared in {}; skipping",
                        fastly_path.display()
                    ));
                    continue;
                }
                create_fastly_store(kind, id)?;
                // If the platform store was created but the
                // writeback fails, remote state and the local
                // manifest are out of sync. Re-running `provision`
                // would attempt to create the platform store again
                // and fail with "already exists". Surface the
                // recovery path explicitly so the operator isn't
                // stuck.
                append_fastly_setup(&fastly_path, kind, id).map_err(|err| {
                    format!(
                        "fastly {kind}-store `{id}` was created remotely, but writeback to {path} failed: {err}\n  To recover, either:\n    1. Manually append `[setup.{kind}_stores.{id}]` and `[local_server.{kind}_stores.{id}]` to {path} and re-run, or\n    2. Delete the orphan remote store via `fastly {kind}-store delete --name={id}` and re-run `edgezero provision --adapter fastly`.",
                        path = fastly_path.display()
                    )
                })?;
                out.push(format!(
                    "created fastly {kind}-store `{id}`; appended setup tables to {}",
                    fastly_path.display()
                ));
            }
        }
        if out.is_empty() {
            out.push("fastly has no declared stores to provision".to_owned());
        }
        Ok(out)
    }

    fn push_config_entries(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store_id: &str,
        entries: &[(String, String)],
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Resolve the platform config-store id on demand via
        // `fastly config-store list --json` (matched by name =
        // `store_id`), then `fastly config-store-entry create
        // --store-id=<id> --key=<k> --value=<v>` per key. Keys
        // arrive pre-flattened from the CLI (dotted form).
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to fastly config-store `{store_id}`"
            )]);
        }
        if dry_run {
            // List each entry so the operator can verify intent
            // before committing. Matches the spin dry-run preview
            // shape.
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would resolve fastly config-store `{store_id}` via `fastly config-store list --json` and run `fastly config-store-entry create` for {} entries:",
                entries.len()
            ));
            for (key, _) in entries {
                out.push(format!("  would create entry `{key}`"));
            }
            return Ok(out);
        }
        let resolved_id = resolve_remote_config_store_id(store_id)?;
        // The per-entry shell-out is spec-compliant but
        // non-atomic. Track which entries succeeded so a mid-loop
        // failure surfaces what got pushed and what didn't — the
        // operator can resume from a known boundary rather than
        // re-pushing the whole set.
        let mut pushed: Vec<String> = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            if let Err(err) = create_config_store_entry(&resolved_id, key, value) {
                let remaining: Vec<&str> = entries
                    .iter()
                    .skip(pushed.len().saturating_add(1))
                    .map(|(remaining_key, _)| remaining_key.as_str())
                    .collect();
                return Err(format!(
                    "fastly push failed at entry `{key}` after committing {committed} of {total} entries; the remaining {remaining_count} entries were NOT pushed.\n  Committed (safe to skip on retry): {pushed:?}\n  Failed: `{key}` — {err}\n  Not attempted (re-push these): {remaining:?}",
                    committed = pushed.len(),
                    total = entries.len(),
                    remaining_count = remaining.len()
                ));
            }
            pushed.push(key.clone());
        }
        Ok(vec![format!(
            "pushed {} entries to fastly config-store `{store_id}` (id={resolved_id})",
            entries.len()
        )])
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        // Explicit `&[]` rather than inheriting the trait default,
        // so the "Multi for every store kind" intent is documented
        // at the call site. Fastly KV / Config / Secrets all
        // support multiple distinct platform resources per kind,
        // unlike spin's flat-namespace single-store model.
        &[]
    }
}

/// Shell out to `fastly <kind>-store create --name=<id>`. Returns
/// `Ok(())` on success; surfaces the CLI's stderr verbatim on
/// failure (including the "already exists" error, which is the
/// caller's signal to fix the toml or use a different name).
///
/// # Errors
/// Returns an error if `fastly` isn't on `PATH`, the child fails to
/// spawn, or the exit status is non-zero.
fn create_fastly_store(kind: &str, name: &str) -> Result<(), String> {
    let subcommand = format!("{kind}-store");
    let name_arg = format!("--name={name}");
    let output = Command::new("fastly")
        .args([subcommand.as_str(), "create", name_arg.as_str()])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly {subcommand} create --name={name}` exited with status {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Probe `fastly.toml` for the existence of BOTH
/// `[setup.<kind>_stores.<id>]` AND `[local_server.<kind>_stores.<id>]`.
/// Both are required for a complete provision; checking only `[setup]`
/// would let a half-edited manifest (e.g. `[setup.*]` present but
/// `[local_server.*]` missing) slip through as "already provisioned"
/// and never get repaired. Treats a missing file as "not present" so
/// the first provision call can create it.
fn setup_block_present(path: &Path, kind: &str, id: &str) -> Result<bool, String> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let plural = format!("{kind}_stores");
    let has = |parent: &str| {
        doc.get(parent)
            .and_then(|root| root.get(plural.as_str()))
            .and_then(|kind_tbl| kind_tbl.get(id))
            .is_some()
    };
    // append_fastly_setup is idempotent per parent: if only one of
    // the two blocks is present, returning false here lets it run
    // again and add just the missing block (the present-key check
    // inside append_fastly_setup skips the one that already exists).
    Ok(has("setup") && has("local_server"))
}

/// Append `[setup.<kind>_stores.<id>]` and
/// `[local_server.<kind>_stores.<id>]` to `fastly.toml`. Creates
/// the file (and the parent `[setup]` / `[local_server]` tables)
/// if absent. Both new blocks are written as empty tables — the
/// resource-link declaration is enough for `fastly compute deploy`
/// to honour, and `config push` fills in entries later.
fn append_fastly_setup(path: &Path, kind: &str, id: &str) -> Result<(), String> {
    use toml_edit::{table, DocumentMut, Item};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let plural = format!("{kind}_stores");
    for parent in ["setup", "local_server"] {
        let parent_entry = doc.entry(parent).or_insert_with(table);
        let parent_tbl = parent_entry.as_table_mut().ok_or_else(|| {
            format!(
                "{}: `{parent}` exists but is not a table; refusing to edit in place",
                path.display()
            )
        })?;
        let kind_entry = parent_tbl
            .entry(plural.as_str())
            .or_insert_with(|| Item::Table(toml_edit::Table::new()));
        let kind_tbl = kind_entry.as_table_mut().ok_or_else(|| {
            format!(
                "{}: `{parent}.{plural}` exists but is not a table; refusing to edit in place",
                path.display()
            )
        })?;
        if !kind_tbl.contains_key(id) {
            kind_tbl.insert(id, Item::Table(toml_edit::Table::new()));
        }
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

// -------------------------------------------------------------------
// `config push` helpers
// -------------------------------------------------------------------

/// Shell out to `fastly config-store-entry create --store-id=<id>
/// --key=<k> --value=<v>` for a single entry. Surfaces fastly's
/// stderr verbatim on failure — including the "entry already
/// exists" error, which is the operator's signal to delete the
/// entry (or use `config-store-entry update` manually) before
/// re-running push.
fn create_config_store_entry(store_id: &str, key: &str, value: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let value_arg = format!("--value={value}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "create",
            store_arg.as_str(),
            key_arg.as_str(),
            value_arg.as_str(),
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry create --store-id={store_id} --key={key} ...` exited with status {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Parse `fastly config-store list --json` output and return the
/// platform `id` of the store whose `name` matches `name`. Accepts
/// both a bare array (`[ {"id": "...", "name": "..."}, ... ]`)
/// and an `{"items": [...]}` envelope so this stays compatible
/// across fastly CLI versions.
fn find_config_store_id(stdout: &str, name: &str) -> ConfigStoreLookup {
    let parsed: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(err) => {
            return ConfigStoreLookup::SchemaDrift(format!("stdout did not parse as JSON: {err}"));
        }
    };
    let Some(array) = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
    else {
        return ConfigStoreLookup::SchemaDrift(format!(
            "expected a bare array `[...]` or an `{{\"items\": [...]}}` envelope; got JSON of shape `{}`",
            shape_summary(&parsed)
        ));
    };
    let mut any_well_formed = false;
    for entry in array {
        let entry_name = entry.get("name").and_then(serde_json::Value::as_str);
        let entry_id = entry.get("id").and_then(serde_json::Value::as_str);
        if entry_name.is_some() && entry_id.is_some() {
            any_well_formed = true;
        }
        if entry_name == Some(name) {
            return entry_id.map_or_else(
                || {
                    ConfigStoreLookup::SchemaDrift(format!(
                        "entry matched name `{name}` but is missing a string `id` field"
                    ))
                },
                |id| ConfigStoreLookup::Found(id.to_owned()),
            );
        }
    }
    if array.is_empty() || any_well_formed {
        ConfigStoreLookup::NotFound
    } else {
        ConfigStoreLookup::SchemaDrift(
            "no entry has both string `name` and `id` fields -- fastly CLI may have changed its output schema"
                .to_owned(),
        )
    }
}

/// One-line type label for a `serde_json::Value` (for diagnostic
/// error messages — not a canonical JSON-schema description).
fn shape_summary(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Resolve the platform config-store id on demand: shell out to
/// `fastly config-store list --json`, parse the JSON, match by
/// `name`. The provision flow doesn't persist this id, so push
/// has to re-fetch every time.
fn resolve_remote_config_store_id(name: &str) -> Result<String, String> {
    let output = Command::new("fastly")
        .args(["config-store", "list", "--json"])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`fastly config-store list --json` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match find_config_store_id(&stdout, name) {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => Err(format!(
            "no fastly config-store matches `{name}` (did you run `edgezero provision --adapter fastly`?)"
        )),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// # Errors
/// Returns an error if the Fastly CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip1",
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
/// Returns an error if the Fastly CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute deploy failed with status {status}"));
    }

    Ok(())
}

fn find_fastly_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "fastly.toml") {
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
            path.file_name().is_some_and(|n| n == "fastly.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate fastly.toml".to_owned());
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
    let target_triple = "wasm32-wasip1";
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(target_triple)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(target_triple)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(target_triple)
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
    register_adapter(&FASTLY_ADAPTER);
    register_adapter_blueprint(&FASTLY_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Fastly CLI serve command (Viceroy) fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "serve"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute serve failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    use tempfile::tempdir;

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("fastly.toml"), "name=\"first\"").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("fastly.toml"), "name=\"second\"").unwrap();

        let found = find_fastly_manifest(&second).unwrap();
        assert_eq!(found, second.join("fastly.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("fastly.toml"), "name = \"demo\"").unwrap();

        let manifest = find_fastly_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("fastly.toml"));
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
    fn read_package_falls_back_to_name() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "name = \"demo\"").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    #[test]
    fn read_package_prefers_package_table() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"demo\"\n").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    // ---------- setup_block_present ----------

    #[test]
    fn setup_block_present_true_when_table_exists() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "name = \"demo\"\n[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        assert!(setup_block_present(&path, "kv", "sessions").expect("probe"));
    }

    #[test]
    fn setup_block_present_false_when_id_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n[setup.kv_stores.other]\n").expect("write");
        assert!(!setup_block_present(&path, "kv", "sessions").expect("probe"));
    }

    #[test]
    fn setup_block_present_false_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert!(!setup_block_present(&path, "kv", "sessions").expect("probe"));
    }

    #[test]
    fn setup_block_present_false_when_only_setup_or_only_local_server_exists() {
        // Spec requires BOTH [setup.<kind>_stores.<id>] AND
        // [local_server.<kind>_stores.<id>] for a fully provisioned
        // store. A half-edited manifest (e.g. operator hand-added
        // the [setup.*] block but skipped [local_server.*]) must
        // return false so the next provision repairs the missing
        // block; append_fastly_setup is idempotent per parent, so
        // it skips the present one and writes the missing one.
        let dir = tempdir().expect("tempdir");
        let only_setup = dir.path().join("only_setup.toml");
        fs::write(&only_setup, "name = \"demo\"\n[setup.kv_stores.sessions]\n").expect("write");
        assert!(
            !setup_block_present(&only_setup, "kv", "sessions").expect("probe"),
            "[setup.*] alone is not enough -- [local_server.*] also required"
        );

        let only_local = dir.path().join("only_local.toml");
        fs::write(
            &only_local,
            "name = \"demo\"\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        assert!(
            !setup_block_present(&only_local, "kv", "sessions").expect("probe"),
            "[local_server.*] alone is not enough -- [setup.*] also required"
        );
    }

    // ---------- append_fastly_setup ----------

    #[test]
    fn append_fastly_setup_creates_both_tables_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        append_fastly_setup(&path, "kv", "sessions").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "setup table added: {after}"
        );
        assert!(
            after.contains("[local_server.kv_stores.sessions]"),
            "local_server table added: {after}"
        );
        assert!(
            after.contains("name = \"demo\""),
            "preserved original keys: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_appends_alongside_existing_kind_tables() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "[setup.kv_stores.cache]\n[local_server.kv_stores.cache]\n",
        )
        .expect("write");
        append_fastly_setup(&path, "kv", "sessions").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.cache]"),
            "existing entry kept: {after}"
        );
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "new entry added: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_is_idempotent_on_duplicate_id() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "[setup.kv_stores.sessions]\nfoo = \"keep\"\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        append_fastly_setup(&path, "kv", "sessions").expect("idempotent append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("foo = \"keep\""),
            "did not stomp existing key: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_creates_file_when_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Note: no fs::write — file starts absent.
        append_fastly_setup(&path, "config", "app_config").expect("create");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("[setup.config_stores.app_config]"));
        assert!(after.contains("[local_server.config_stores.app_config]"));
    }

    #[test]
    fn append_fastly_setup_preserves_top_comments() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "# managed by hand -- please keep this line\nname = \"demo\"\n",
        )
        .expect("write");
        append_fastly_setup(&path, "secret", "default").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("# managed by hand"),
            "preserved comment: {after}"
        );
    }

    // ---------- provision (dry-run + error path) ----------

    #[test]
    fn provision_dry_run_does_not_invoke_fastly() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let kv_ids = vec!["sessions".to_owned()];
        let config_ids = vec!["app_config".to_owned()];
        let secret_ids = vec!["default".to_owned()];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        // 1 KV + 1 config + 1 secret = 3 status lines.
        assert_eq!(out.len(), 3);
        assert!(out[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(out[1].contains("would run `fastly config-store create --name=app_config`"));
        assert!(out[2].contains("would run `fastly secret-store create --name=default`"));
        // Manifest untouched.
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated fastly.toml");
    }

    #[test]
    fn provision_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let kv_ids = vec!["sessions".to_owned()];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = FastlyCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("fastly.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["fastly has no declared stores to provision"]);
    }

    #[test]
    fn provision_skips_id_when_setup_block_already_present() {
        // setup_block_present's role in the flow: re-running
        // provision after the user already declared a store in
        // fastly.toml must be a no-op (no shell-out to fastly).
        // We can verify this in a real (non-dry-run) call because
        // the skip path bypasses create_fastly_store entirely.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        let kv_ids = vec!["sessions".to_owned()];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, false)
            .expect("skip path succeeds without invoking fastly");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("already declared"), "got: {out:?}");
    }

    // ---------- find_config_store_id ----------

    #[test]
    fn find_config_store_id_matches_bare_array_by_name() {
        let stdout = r#"[
            {"id": "abc123", "name": "app_config"},
            {"id": "def456", "name": "other_store"}
        ]"#;
        match find_config_store_id(stdout, "app_config") {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "abc123"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_tolerates_items_envelope() {
        let stdout = r#"{"items": [
            {"id": "xyz789", "name": "app_config"}
        ]}"#;
        match find_config_store_id(stdout, "app_config") {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "xyz789"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_distinguishes_not_found_from_match_failure() {
        // JSON parses cleanly, entries are well-formed
        // (`name` + `id` strings present), but no entry matches
        // → NotFound. Operator likely needs to run `provision`.
        let stdout = r#"[{"id": "abc", "name": "other"}]"#;
        assert!(matches!(
            find_config_store_id(stdout, "missing"),
            ConfigStoreLookup::NotFound
        ));
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_on_malformed_json() {
        // Unparseable bytes are NOT a "store not found" — they're
        // a "fastly CLI output format changed" signal. Operator
        // needs different recovery (file a bug, pin CLI version)
        // than for the "store doesn't exist yet" case.
        let drift = find_config_store_id("not json", "anything");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "non-JSON stdout must be schema drift, got {drift:?}"
        );
        let empty = find_config_store_id("", "anything");
        assert!(
            matches!(empty, ConfigStoreLookup::SchemaDrift(_)),
            "empty stdout must be schema drift, got {empty:?}"
        );
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_shape_unexpected() {
        // JSON parses but the top-level is neither a bare array
        // nor an `{items: [...]}` envelope.
        let stdout = r#"{"namespace": "fastly", "list": []}"#;
        match find_config_store_id(stdout, "any") {
            ConfigStoreLookup::SchemaDrift(detail) => {
                assert!(
                    detail.contains("bare array") || detail.contains("items"),
                    "schema-drift detail names the expected shapes: {detail}"
                );
            }
            ConfigStoreLookup::Found(id) => panic!("expected SchemaDrift, got Found({id})"),
            ConfigStoreLookup::NotFound => panic!("expected SchemaDrift, got NotFound"),
        }
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_entries_lack_name_id() {
        // Array of objects but none have BOTH string `name` and
        // string `id` fields — suggests schema rename (e.g.
        // fastly renamed `name` → `title`).
        let stdout = r#"[{"title": "app_config", "uid": "abc"}]"#;
        let drift = find_config_store_id(stdout, "app_config");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "entries lacking name/id must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_returns_not_found_for_empty_array() {
        // Empty array IS a valid "store doesn't exist yet" signal,
        // not schema drift — fastly CLI legitimately returns `[]`
        // when no config-stores exist.
        let drift = find_config_store_id("[]", "any");
        assert!(
            matches!(drift, ConfigStoreLookup::NotFound),
            "empty array must be NotFound, got {drift:?}"
        );
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_does_not_invoke_fastly() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                "app_config",
                &entries,
                true,
            )
            .expect("dry-run succeeds");
        // First line names the resolve+publish flow; subsequent lines preview
        // each key the push would create (so callers can eyeball the keyset
        // before running for real).
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would resolve fastly config-store `app_config`")
                && out[0].contains("config-store-entry create"),
            "dry-run header describes the would-be flow: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run lists `greeting`: {out:?}"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("`feature.new_checkout`")),
            "dry-run lists `feature.new_checkout`: {out:?}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_without_invoking_fastly() {
        let dir = tempdir().expect("tempdir");
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                "app_config",
                &[],
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "status line names the no-op: {out:?}"
        );
    }
}
