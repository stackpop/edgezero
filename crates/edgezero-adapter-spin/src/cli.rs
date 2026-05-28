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
            // credentials — this is a thin shell-out.
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
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: spin provision is pure spin.toml editing — no
        // shell-out (Spin KV stores are provisioned by the Spin
        // runtime / Fermyon at deploy). For each declared KV id,
        // append the label to the resolved component's
        // `key_value_stores` array. Config and secret variables
        // are NOT handled here: the manifest carries only store
        // ids, not app-config field keys or secret key names —
        // `config push --adapter spin` declares config variables
        // (it loads the typed `<name>.toml`), and secret
        // variables are manually declared by the developer in
        // spin.toml.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for provision".to_owned(),
            );
        };
        let spin_path = manifest_root.join(rel);

        let mut out = Vec::new();
        if !stores.kv.is_empty() {
            let component_id = resolve_spin_component(&spin_path, component_selector)?;
            for id in stores.kv {
                if dry_run {
                    out.push(format!(
                        "would ensure KV label `{id}` is in [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                    continue;
                }
                let added = ensure_kv_label_in_component(&spin_path, &component_id, id)?;
                if added {
                    out.push(format!(
                        "added KV label `{id}` to [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                } else {
                    out.push(format!(
                        "KV label `{id}` already present in [component.{component_id}].key_value_stores in {}; skipping",
                        spin_path.display()
                    ));
                }
            }
        }
        for id in stores.config {
            out.push(format!(
                "spin config id `{id}` is provisioned by `config push --adapter spin` (declares Spin variables); nothing to do here"
            ));
        }
        for id in stores.secrets {
            out.push(format!(
                "spin secret id `{id}` requires manual `[variables].* secret = true` + `[component.*.variables].*` declarations in spin.toml; nothing to do here"
            ));
        }
        if out.is_empty() {
            out.push("spin has no declared stores to provision".to_owned());
        }
        Ok(out)
    }

    fn push_config_entries(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        _store_id: &str,
        entries: &[(String, String)],
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: pure spin.toml editing — no shell-out. Spec
        // says Spin variables must match `^[a-z][a-z0-9_]*$`, and
        // dotted CLI keys translate `.→__` (lowercase). A Spin
        // variable is only readable by a component when it is both
        // declared in `[variables]` AND bound in
        // `[component.<component>.variables]`, so push writes
        // both tables. Secret variables are intentionally NOT
        // touched — the typed CLI flow already stripped
        // `SECRET_FIELDS`, and the raw flow leaves declaration to
        // the developer (manual `[variables].* secret = true`).
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for config push"
                    .to_owned(),
            );
        };
        let spin_path = manifest_root.join(rel);
        let component_id = resolve_spin_component(&spin_path, component_selector)?;

        // Translate `.→__` lowercase up front so both the
        // dry-run preview and the writeback see the exact key
        // form that will land in spin.toml. Reject any key whose
        // translation fails `^[a-z][a-z0-9_]*$` — `config
        // validate` should already have caught it, but a
        // belt-and-braces check keeps spin.toml well-formed.
        let mut translated: Vec<(String, String)> = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let spin_key = translate_key_for_spin(key);
            if !is_valid_spin_key(&spin_key) {
                let reason = spin_key_rule_violation(&spin_key);
                return Err(format!(
                    "config key `{key}` translates to Spin variable `{spin_key}`, which is not a valid Spin variable name. {reason}. Rename the config key so the translated name conforms. (`edgezero config validate` -- typed or raw -- runs the same Spin-variable check against the manifest before push, so a validate step earlier in the flow would have surfaced this without a push attempt.)"
                ));
            }
            translated.push((spin_key, value.clone()));
        }

        if translated.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to [component.{component_id}.variables] in {}",
                spin_path.display()
            )]);
        }

        if dry_run {
            let mut out = Vec::with_capacity(translated.len().saturating_add(1));
            out.push(format!(
                "would write {} Spin variable(s) to {} (both [variables] and [component.{component_id}.variables]):",
                translated.len(),
                spin_path.display()
            ));
            for (spin_key, value) in &translated {
                out.push(format!(
                    "  [variables.{spin_key}] default = {value:?}; [component.{component_id}.variables].{spin_key} = {{{{ {spin_key} }}}}"
                ));
            }
            return Ok(out);
        }

        write_spin_variables(&spin_path, &component_id, &translated)?;
        Ok(vec![format!(
            "pushed {} Spin variable(s) to {} ([variables] + [component.{component_id}.variables])",
            translated.len(),
            spin_path.display()
        )])
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: Multi for KV (label-backed); Single for Config and
        // Secrets (flat-variable namespace).
        &["config", "secrets"]
    }

    fn validate_adapter_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
    ) -> Result<(), String> {
        // check 3: spin.toml must exist and either declare
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
        // check 1: each dotted config key, translated `.→__`,
        // must match `^[a-z][a-z0-9_]*$` — Spin's flat variable
        // namespace has no other escaping.
        for key in keys {
            let spin_var = key.replace('.', "__");
            if !is_valid_spin_key(&spin_var) {
                let reason = spin_key_rule_violation(&spin_var);
                return Err(format!(
                    "config key `{key}` translates to Spin variable `{spin_var}`, which is not a valid Spin variable name. {reason}. Rename the config key so the translated name conforms."
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
        // check 2: flattened config keys ∪ `#[secret]` values
        // must be a unique set in the effective Spin variable
        // namespace, since Spin has one flat namespace per
        // component. The CLI already filtered out
        // `#[secret(store_ref)]` entries (those are runtime store
        // ids, not Spin variables).
        //
        // The runtime stores are ASYMMETRIC in how they canonicalise
        // lookups:
        //   - `SpinConfigStore::translate_key` does `.→__`, case-
        //     preserving. (Uppercase config keys are rejected
        //     separately by `validate_app_config_keys`, so by the
        //     time we reach this check `config_keys` are already
        //     guaranteed lowercase.)
        //   - `SpinSecretStore::get_bytes` lowercases the key
        //     before calling `variables::get` (since Spin variable
        //     names must be lowercase).
        //
        // The validator must mirror both, or a collision like
        // config key `greeting` + `#[secret]` value `"GREETING"`
        // — which resolve to the same Spin variable at runtime —
        // would be missed. We also run `is_valid_spin_key` on
        // each canonicalised secret value so invalid Spin chars
        // (dashes, digit-first values) fail at validation rather
        // than at runtime with an opaque `InvalidName` error.
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
            // Match `SpinSecretStore`'s runtime canonicalisation
            // exactly: lowercase only (no `.→__` — secret keys
            // aren't expected to contain dots, and the runtime
            // doesn't translate them either).
            let spin_var = value.to_ascii_lowercase();
            if !is_valid_spin_key(&spin_var) {
                let reason = spin_key_rule_violation(&spin_var);
                return Err(format!(
                    "`#[secret]` field `{field_name}` value `{value}` translates to Spin variable `{spin_var}`, which is not a valid Spin variable name. {reason}. Pick a `#[secret]` value that conforms."
                ));
            }
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

/// Return a per-failure-mode diagnostic for a key that failed
/// `is_valid_spin_key`. Spin's variable-name rule
/// (`^[a-z][a-z0-9_]*$`) is one regex but the operator usually
/// wants to know WHICH bit they broke: digit-leading, uppercase,
/// or stray punctuation. Returns a short phrase to splice into
/// the caller's full error.
fn spin_key_rule_violation(key: &str) -> &'static str {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return "Spin variable names must not be empty";
    };
    if first.is_ascii_digit() {
        return "Spin variable names must start with a lowercase letter, not a digit";
    }
    if first.is_ascii_uppercase() {
        return "Spin variable names must be lowercase (uppercase letters are not allowed)";
    }
    if !first.is_ascii_lowercase() {
        return "Spin variable names must start with a lowercase ASCII letter";
    }
    for ch in chars {
        if ch.is_ascii_uppercase() {
            return "Spin variable names must be lowercase (uppercase letters are not allowed)";
        }
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
            return "Spin variable names may only contain lowercase letters, digits, and underscores";
        }
    }
    "Spin variable names must match `^[a-z][a-z0-9_]*$`"
}

/// Standard error wording when a TOML key we expected to be a
/// table (`[variables]`, `[component.X]`, `[component.X.variables]`,
/// `[variables.<key>]`) is found as a non-table value. Spin requires
/// these slots to be tables; an inline value usually means an old
/// hand-edited spin.toml that pre-dates the variables convention.
fn not_a_table_error(spin_path: &Path, what: &str) -> String {
    format!(
        "{}: `{what}` exists but is not a TOML table. Spin requires `[{what}]` table syntax with key/value pairs underneath. If `{what} = ...` was set as a single inline value, replace it with `[{what}]` block syntax and move keys into it.",
        spin_path.display()
    )
}

fn collect_spin_component_ids(parsed: &toml::Value) -> Vec<String> {
    parsed
        .as_table()
        .and_then(|root| root.get("component"))
        .and_then(toml::Value::as_table)
        .map(|components| components.keys().cloned().collect())
        .unwrap_or_default()
}

/// Resolve which `[component.<id>]` table `provision` should
/// write into. Mirrors the rule used by `validate_adapter_manifest`
///: single-component spin.toml resolves implicitly,
/// multi-component requires an explicit `component = "..."` in
/// `[adapters.spin.adapter]`, and a selector that doesn't match
/// any declared id is an error.
fn resolve_spin_component(
    spin_path: &Path,
    component_selector: Option<&str>,
) -> Result<String, String> {
    let raw = fs::read_to_string(spin_path).map_err(|err| {
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
            return Ok(selector.to_owned());
        }
        return Err(format!(
            "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
            selector,
            spin_path.display(),
            component_ids.join(", ")
        ));
    }
    if component_ids.len() == 1 {
        return Ok(component_ids.into_iter().next().unwrap_or_default());
    }
    Err(format!(
        "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
        spin_path.display(),
        component_ids.len(),
        component_ids.join(", ")
    ))
}

/// Ensure `label` appears in `[component.<component_id>]`'s
/// `key_value_stores = [...]` array. Creates the array if absent.
/// Returns `Ok(true)` if the label was newly added, `Ok(false)` if
/// it was already there (idempotent across re-runs). Preserves the
/// rest of the spin manifest, including formatting and comments.
fn ensure_kv_label_in_component(
    spin_path: &Path,
    component_id: &str,
    label: &str,
) -> Result<bool, String> {
    use toml_edit::{value, Array, DocumentMut, Value};

    let raw = fs::read_to_string(spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;

    let component_root = doc.get_mut("component").ok_or_else(|| {
        format!(
            "{}: [component.*] tables expected but `component` key missing",
            spin_path.display()
        )
    })?;
    let component_tbl = component_root
        .as_table_mut()
        .ok_or_else(|| format!("{}: `component` is not a table", spin_path.display()))?;
    let target = component_tbl.get_mut(component_id).ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not declared",
            spin_path.display()
        )
    })?;
    let target_tbl = target.as_table_mut().ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not a table",
            spin_path.display()
        )
    })?;

    let entry = target_tbl
        .entry("key_value_stores")
        .or_insert_with(|| value(Array::new()));
    let arr = entry
        .as_value_mut()
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            format!(
                "{}: [component.{component_id}].key_value_stores is not an array",
                spin_path.display()
            )
        })?;

    if arr.iter().any(|item| item.as_str() == Some(label)) {
        return Ok(false);
    }
    arr.push(label);

    fs::write(spin_path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    Ok(true)
}

/// Translate a dotted CLI config key into a Spin variable name
///. Spin's flat variable namespace has no concept of
/// nested paths, so we encode the dotted path as `__`-separated
/// segments and lowercase the result.
fn translate_key_for_spin(dotted_key: &str) -> String {
    dotted_key.replace('.', "__").to_ascii_lowercase()
}

/// Declare + bind each Spin variable so the component can read
/// it. Writes both:
/// 1. `[variables].<key>` with `default = "<value>"` — the
///    application-level declaration.
/// 2. `[component.<component>.variables].<key>` = `"{{ <key> }}"`
///    — the component binding (without it the variable is
///    invisible to the wasm component).
///
/// Idempotent: re-running updates the `default` value in place
/// and overwrites the component binding. Preserves the rest of
/// the spin manifest (formatting, comments, sibling tables).
fn write_spin_variables(
    spin_path: &Path,
    component_id: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    use toml_edit::{table, value, DocumentMut, Item};

    let raw = fs::read_to_string(spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;

    // (1) Application-level declarations under [variables].
    // Existing entries may be either a `[variables.<key>]` block
    // table OR an inline-table value (`<key> = { default = "..." }`).
    // Real-world spin.toml files hand-edited by developers very often
    // use the inline form; preserve whichever shape the user chose
    // and update the `default` field in place. New entries (no prior
    // declaration) get the block form by default.
    let variables_entry = doc.entry("variables").or_insert_with(table);
    let variables_tbl = variables_entry
        .as_table_mut()
        .ok_or_else(|| not_a_table_error(spin_path, "variables"))?;
    for (spin_key, val) in entries {
        let var_entry = variables_tbl
            .entry(spin_key.as_str())
            .or_insert_with(|| Item::Table(toml_edit::Table::new()));
        match var_entry {
            Item::Table(tbl) => {
                tbl.insert("default", value(val.as_str()));
            }
            Item::Value(toml_edit::Value::InlineTable(inline)) => {
                inline.insert("default", toml_edit::Value::from(val.as_str()));
            }
            Item::Value(
                toml_edit::Value::String(_)
                | toml_edit::Value::Integer(_)
                | toml_edit::Value::Float(_)
                | toml_edit::Value::Boolean(_)
                | toml_edit::Value::Datetime(_)
                | toml_edit::Value::Array(_),
            )
            | Item::None
            | Item::ArrayOfTables(_) => {
                return Err(not_a_table_error(
                    spin_path,
                    &format!("variables.{spin_key}"),
                ));
            }
        }
    }

    // (2) Component-level bindings under
    // [component.<component>.variables]. Surfaces the
    // application variable into the wasm component via spin's
    // `{{ <key> }}` template syntax.
    let component_root = doc.entry("component").or_insert_with(table);
    let component_tbl = component_root
        .as_table_mut()
        .ok_or_else(|| not_a_table_error(spin_path, "component"))?;
    let target = component_tbl.entry(component_id).or_insert_with(table);
    let target_tbl = target
        .as_table_mut()
        .ok_or_else(|| not_a_table_error(spin_path, &format!("component.{component_id}")))?;
    let bindings_entry = target_tbl.entry("variables").or_insert_with(table);
    let bindings_tbl = bindings_entry.as_table_mut().ok_or_else(|| {
        not_a_table_error(spin_path, &format!("component.{component_id}.variables"))
    })?;
    for (spin_key, _) in entries {
        let template = format!("{{{{ {spin_key} }}}}");
        bindings_tbl.insert(spin_key.as_str(), value(template));
    }

    fs::write(spin_path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    Ok(())
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
    fn spin_key_rule_violation_picks_the_right_diagnostic_per_mode() {
        // Each failure mode produces a distinct, actionable phrase
        // so the error message tells the operator WHICH bit of the
        // rule they broke -- not just "doesn't match a regex".
        assert!(spin_key_rule_violation("").contains("empty"));
        assert!(spin_key_rule_violation("1foo").contains("digit"));
        assert!(spin_key_rule_violation("Foo").contains("lowercase"));
        assert!(spin_key_rule_violation("foo-bar").contains("lowercase letters, digits"));
        assert!(spin_key_rule_violation("fooBar").contains("lowercase"));
        // `_foo` starts with `_` -- not digit, not uppercase, not
        // lowercase ASCII letter. Falls through to the catch-all.
        assert!(spin_key_rule_violation("_foo").contains("lowercase ASCII letter"));
    }

    #[test]
    fn not_a_table_error_includes_path_keyword_and_migration_hint() {
        let path = Path::new("/tmp/spin.toml");
        let err = not_a_table_error(path, "variables");
        assert!(err.contains("/tmp/spin.toml"), "names path: {err}");
        assert!(err.contains("`variables`"), "names keyword: {err}");
        assert!(
            err.contains("block syntax") || err.contains("[variables]"),
            "points at fix: {err}"
        );
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
        // `greeting` collide.
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

    // ---------- secret-value canonicalisation regressions ----------

    /// Runtime `SpinSecretStore::get_bytes` lowercases the key
    /// before calling `variables::get`. The validator must
    /// mirror that or a `#[secret]` value like `"GREETING"`
    /// (uppercase) silently passes validation but collides with
    /// the config key `greeting` at runtime — both resolve to
    /// the same Spin variable `greeting`.
    #[test]
    fn validate_typed_secrets_detects_collision_after_lowercasing_secret_value() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&["greeting"], &[("api_token", "GREETING")])
            .expect_err("case-only collision against config key must error");
        assert!(
            err.contains("greeting") && err.contains("collides"),
            "error names the lowercased collision: {err}"
        );
    }

    /// `#[secret]` values must also be valid Spin variable names
    /// after canonicalisation. A dashed value like `"api-token"`
    /// reaches Spin at runtime and gets rejected with an opaque
    /// `InvalidName` — the validator should catch it earlier.
    #[test]
    fn validate_typed_secrets_rejects_invalid_spin_variable_in_secret_value() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&["greeting"], &[("api_token", "api-token")])
            .expect_err("dashed secret value must error");
        assert!(
            err.contains("api-token") && err.contains("api-token") && err.contains("Spin variable"),
            "error names the bad value + that it's a Spin variable issue: {err}"
        );
    }

    /// Negative case: a lowercased secret value that happens to
    /// coincide with another lowercased value MUST collide
    /// (sanity check that the `seen` set still works post-fix).
    #[test]
    fn validate_typed_secrets_detects_collision_between_two_lowercased_secret_values() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&[], &[("first", "SHARED_NAME"), ("second", "shared_name")])
            .expect_err("two values lowercasing to the same name must collide");
        assert!(
            err.contains("shared_name") && err.contains("collides"),
            "error names the shared canonical name: {err}"
        );
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

    // ---------- resolve_spin_component ----------

    fn write_spin(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(&path, contents).expect("write spin.toml");
        path
    }

    #[test]
    fn resolve_spin_component_picks_single_component_implicitly() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.only]\nsource = \"a.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, None).expect("resolve");
        assert_eq!(resolved, "only");
    }

    #[test]
    fn resolve_spin_component_uses_selector_when_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, Some("second")).expect("resolve");
        assert_eq!(resolved, "second");
    }

    #[test]
    fn resolve_spin_component_errors_on_multi_without_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let err = resolve_spin_component(&path, None).expect_err("ambiguous must error");
        assert!(
            err.contains("first") && err.contains("second"),
            "error lists candidates: {err}"
        );
    }

    #[test]
    fn resolve_spin_component_errors_on_bad_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.real]\nsource = \"a.wasm\"\n",
        );
        let err = resolve_spin_component(&path, Some("typo")).expect_err("bad selector must error");
        assert!(
            err.contains("typo") && err.contains("real"),
            "error names bad selector and available id: {err}"
        );
    }

    // ---------- ensure_kv_label_in_component ----------

    #[test]
    fn ensure_kv_label_adds_to_component_without_key_value_stores() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let added = ensure_kv_label_in_component(&path, "demo", "sessions").expect("ensure");
        assert!(added, "newly added label should return true");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("key_value_stores = [\"sessions\"]")
                || after.contains("key_value_stores = ['sessions']"),
            "added KV label: {after}"
        );
    }

    #[test]
    fn ensure_kv_label_appends_to_existing_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"cache\"]\n",
        );
        let added = ensure_kv_label_in_component(&path, "demo", "sessions").expect("ensure");
        assert!(added);
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("\"cache\""), "kept existing label: {after}");
        assert!(after.contains("\"sessions\""), "added new label: {after}");
    }

    #[test]
    fn ensure_kv_label_is_idempotent_when_already_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"sessions\"]\n",
        );
        let added = ensure_kv_label_in_component(&path, "demo", "sessions").expect("ensure");
        assert!(!added, "duplicate label should return false");
    }

    #[test]
    fn ensure_kv_label_errors_when_component_missing() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let err = ensure_kv_label_in_component(&path, "missing", "sessions")
            .expect_err("missing component must error");
        assert!(
            err.contains("missing"),
            "error names the missing component id: {err}"
        );
    }

    #[test]
    fn ensure_kv_label_preserves_top_comments_and_other_fields() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "# keep me\nspin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nallowed_outbound_hosts = []\n",
        );
        ensure_kv_label_in_component(&path, "demo", "sessions").expect("ensure");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("# keep me"), "preserved comment: {after}");
        assert!(
            after.contains("allowed_outbound_hosts = []"),
            "preserved sibling field: {after}"
        );
    }

    // ---------- provision (dry-run + error path + idempotent skip) ----------

    #[test]
    fn provision_dry_run_does_not_edit_spin_toml() {
        let dir = tempdir().expect("tempdir");
        let original =
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let kv_ids = vec!["sessions".to_owned(), "cache".to_owned()];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("would ensure KV label `sessions`"));
        assert!(out[1].contains("would ensure KV label `cache`"));
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, original, "dry-run mutated spin.toml");
    }

    #[test]
    fn provision_writes_kv_labels_into_resolved_component() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids = vec!["sessions".to_owned()];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real run succeeds");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("added KV label `sessions`"), "got: {out:?}");
        let after = fs::read_to_string(dir.path().join("spin.toml")).expect("read back");
        assert!(
            after.contains("\"sessions\""),
            "label landed in spin.toml: {after}"
        );
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
        let err = SpinCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("spin.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_reports_config_and_secrets_as_out_of_scope() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let config_ids = vec!["app_config".to_owned()];
        let secret_ids = vec!["default".to_owned()];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &secret_ids,
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("config/secrets-only provision still succeeds");
        assert_eq!(out.len(), 2);
        assert!(
            out[0].contains("config push"),
            "config row points at config push: {out:?}"
        );
        assert!(
            out[1].contains("manual"),
            "secret row flags manual declaration: {out:?}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["spin has no declared stores to provision"]);
    }

    // ---------- translate_key_for_spin ----------

    #[test]
    fn translate_key_for_spin_replaces_dots_with_double_underscores() {
        assert_eq!(
            translate_key_for_spin("service.timeout_ms"),
            "service__timeout_ms"
        );
    }

    #[test]
    fn translate_key_for_spin_passes_through_unsegmented_keys() {
        assert_eq!(translate_key_for_spin("greeting"), "greeting");
    }

    #[test]
    fn translate_key_for_spin_lowercases() {
        // Spin's `^[a-z][a-z0-9_]*$` rule rejects uppercase; the
        // translator normalises so the validator in sees the
        // canonical form before push.
        assert_eq!(translate_key_for_spin("GREETING"), "greeting");
        assert_eq!(
            translate_key_for_spin("Service.TimeoutMs"),
            "service__timeoutms"
        );
    }

    // ---------- write_spin_variables ----------

    #[test]
    fn write_spin_variables_writes_both_tables() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let entries = vec![
            ("greeting".to_owned(), "hi".to_owned()),
            ("service__timeout_ms".to_owned(), "1500".to_owned()),
        ];
        write_spin_variables(&path, "demo", &entries).expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        // The generated manifest must round-trip through a TOML
        // parser (spec "validation strength" — regex + parse
        // is the floor when neither the spin CLI nor spin_sdk is
        // reachable from the test harness).
        let parsed: toml::Value = toml::from_str(&after).expect("parses as TOML");
        let variables = parsed
            .get("variables")
            .and_then(toml::Value::as_table)
            .expect("[variables] present");
        assert_eq!(
            variables["greeting"]["default"].as_str(),
            Some("hi"),
            "greeting default landed: {after}"
        );
        assert_eq!(
            variables["service__timeout_ms"]["default"].as_str(),
            Some("1500")
        );
        let bindings = parsed["component"]["demo"]["variables"]
            .as_table()
            .expect("[component.demo.variables] present");
        assert_eq!(
            bindings["greeting"].as_str(),
            Some("{{ greeting }}"),
            "binding uses spin template: {after}"
        );
        assert_eq!(
            bindings["service__timeout_ms"].as_str(),
            Some("{{ service__timeout_ms }}")
        );
    }

    #[test]
    fn write_spin_variables_is_idempotent_and_updates_in_place() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let first = vec![("greeting".to_owned(), "hi".to_owned())];
        write_spin_variables(&path, "demo", &first).expect("first write");
        // Re-push with a new value — should overwrite, not error.
        let second = vec![("greeting".to_owned(), "hello".to_owned())];
        write_spin_variables(&path, "demo", &second).expect("second write");
        let after = fs::read_to_string(&path).expect("read back");
        let parsed: toml::Value = toml::from_str(&after).expect("parses");
        assert_eq!(
            parsed["variables"]["greeting"]["default"].as_str(),
            Some("hello"),
            "default updated: {after}"
        );
        // Component binding stays a single entry (not duplicated).
        let bindings = parsed["component"]["demo"]["variables"]
            .as_table()
            .expect("bindings present");
        assert_eq!(bindings.len(), 1, "no duplicate bindings: {after}");
    }

    #[test]
    fn write_spin_variables_updates_existing_inline_table_entry_in_place() {
        // Hand-edited spin.toml files often declare variables in
        // inline-table form: `greeting = { default = "hello" }`. The
        // writeback path must update such entries in place (matching
        // the user's chosen shape) instead of erring "is not a
        // table". app-demo's spin.toml is exactly this shape.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n\
             [application]\nname = \"x\"\nversion = \"0\"\n\
             [variables]\n\
             greeting = { default = \"old\" }\n\
             feature__new_checkout = { default = \"false\" }\n\
             [component.demo]\nsource = \"demo.wasm\"\n",
        );
        let entries = vec![
            ("greeting".to_owned(), "updated".to_owned()),
            ("feature__new_checkout".to_owned(), "true".to_owned()),
        ];
        write_spin_variables(&path, "demo", &entries).expect("inline-table writeback succeeds");

        let after = fs::read_to_string(&path).expect("read back");
        let parsed: toml::Value = toml::from_str(&after).expect("parses");
        assert_eq!(
            parsed["variables"]["greeting"]["default"].as_str(),
            Some("updated"),
            "inline-table entry updated: {after}"
        );
        assert_eq!(
            parsed["variables"]["feature__new_checkout"]["default"].as_str(),
            Some("true"),
            "second inline-table entry updated: {after}"
        );
        // The original inline-table shape is preserved (not
        // converted to a block table), so the user's formatting
        // stays intact.
        assert!(
            after.contains("greeting = {") || after.contains("greeting= {"),
            "preserved inline-table shape: {after}"
        );
    }

    #[test]
    fn write_spin_variables_preserves_other_component_fields() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nallowed_outbound_hosts = []\n",
        );
        let entries = vec![("greeting".to_owned(), "hi".to_owned())];
        write_spin_variables(&path, "demo", &entries).expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("allowed_outbound_hosts = []"),
            "preserved sibling field: {after}"
        );
        assert!(
            after.contains("source = \"demo.wasm\""),
            "preserved source: {after}"
        );
    }

    #[test]
    fn write_spin_variables_golden_round_trips_and_passes_spin_key_regex() {
        // golden test — floor of the validation ladder when
        // neither the spin CLI nor spin_sdk validation is
        // reachable: every variable name matches the Spin
        // `^[a-z][a-z0-9_]*$` rule, and the generated manifest
        // parses as TOML.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service__timeout_ms".to_owned(), "1500".to_owned()),
            ("api__base_url".to_owned(), "https://example.com".to_owned()),
        ];
        write_spin_variables(&path, "demo", &entries).expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        let parsed: toml::Value = toml::from_str(&after).expect("parses as TOML");

        let variables = parsed["variables"].as_table().expect("[variables] present");
        for key in variables.keys() {
            assert!(
                is_valid_spin_key(key),
                "variable name `{key}` violates Spin's `^[a-z][a-z0-9_]*$` rule"
            );
        }
        let bindings = parsed["component"]["demo"]["variables"]
            .as_table()
            .expect("[component.demo.variables] present");
        for key in bindings.keys() {
            assert!(
                is_valid_spin_key(key),
                "binding name `{key}` violates Spin's `^[a-z][a-z0-9_]*$` rule"
            );
            let template = bindings[key].as_str().expect("binding is a string");
            assert_eq!(template, format!("{{{{ {key} }}}}"));
        }
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_does_not_edit_spin_toml() {
        // the spec calls for the
        // dry-run to print the would-be `__`-encoded keys and the
        // would-be content of BOTH spin.toml tables, then leave
        // the on-disk file unchanged. Exercise a multi-entry
        // input whose translation isn't a no-op so the test
        // verifies `.→__` lowercasing actually surfaces in the
        // preview.
        let dir = tempdir().expect("tempdir");
        let original = "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                "app_config",
                &entries,
                true,
            )
            .expect("dry-run succeeds");
        // Header line names the count + both tables.
        assert!(
            out.iter()
                .any(|line| line.contains("would write 3 Spin variable")),
            "dry-run summary present with count: {out:?}"
        );
        assert!(
            out.iter().any(|line| {
                line.contains("[variables]") && line.contains("[component.demo.variables]")
            }),
            "dry-run summary names BOTH spin.toml tables: {out:?}"
        );
        // Each translated key appears in some preview line, with
        // the `.→__` lowercased form (not the dotted source).
        for translated in &["greeting", "service__timeout_ms", "feature__new_checkout"] {
            assert!(
                out.iter().any(|line| line.contains(translated)),
                "dry-run names translated key `{translated}`: {out:?}"
            );
        }
        // No dotted source keys leaked through.
        for dotted in &["service.timeout_ms", "feature.new_checkout"] {
            assert!(
                !out.iter().any(|line| line.contains(dotted)),
                "dry-run must not leak the dotted source form `{dotted}`: {out:?}"
            );
        }
        // Each preview line also surfaces the spin template
        // syntax for the component binding (the literal `{{ key
        // }}` form, asserted as `{{ <key>` to dodge prettier-
        // unfriendly closing-brace pairs).
        for translated in &["greeting", "service__timeout_ms", "feature__new_checkout"] {
            assert!(
                out.iter().any(|line| {
                    line.contains(&format!(".{translated}"))
                        && line.contains(&format!("{{{{ {translated}"))
                }),
                "dry-run shows component binding template for `{translated}`: {out:?}"
            );
        }
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(
            after, original,
            "dry-run must leave spin.toml byte-identical"
        );
    }

    #[test]
    fn push_writes_variables_into_resolved_component() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let entries = vec![
            ("greeting".to_owned(), "hi".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        let out = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                "app_config",
                &entries,
                false,
            )
            .expect("real push succeeds");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("pushed 2 Spin variable"), "got: {out:?}");
        // Re-parse and assert both the dot-translated key and the
        // pristine binding are present (`service.timeout_ms` →
        // `service__timeout_ms`).
        let after = fs::read_to_string(&path).expect("read back");
        let parsed: toml::Value = toml::from_str(&after).expect("parses");
        assert_eq!(
            parsed["variables"]["service__timeout_ms"]["default"].as_str(),
            Some("1500"),
            "`.` translated to `__`: {after}"
        );
        assert_eq!(
            parsed["component"]["demo"]["variables"]["service__timeout_ms"].as_str(),
            Some("{{ service__timeout_ms }}")
        );
    }

    #[test]
    fn push_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("greeting".to_owned(), "hi".to_owned())];
        let err = SpinCliAdapter
            .push_config_entries(dir.path(), None, None, "app_config", &entries, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("spin.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn push_rejects_keys_that_violate_spin_variable_rule() {
        // `config validate` should already have caught this, but
        // the adapter belt-and-braces check keeps spin.toml
        // well-formed if a raw push slips an invalid key through.
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let entries = vec![("api-token".to_owned(), "x".to_owned())];
        let err = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                "app_config",
                &entries,
                false,
            )
            .expect_err("dashed key must error");
        assert!(
            err.contains("api-token") && err.contains("Spin"),
            "error names the bad key + Spin: {err}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_without_writing() {
        let dir = tempdir().expect("tempdir");
        let original = "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let out = SpinCliAdapter
            .push_config_entries(
                dir.path(),
                Some("spin.toml"),
                None,
                "app_config",
                &[],
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("no config entries"), "got: {out:?}");
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, original, "zero-entry push must not edit spin.toml");
    }
}
