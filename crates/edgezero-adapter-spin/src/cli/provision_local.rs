//! Local-mode arms of `provision` and `provision_typed` for the Spin
//! adapter: rewriting `spin.toml`, appending `[key_value_store.*]`
//! stanzas to `runtime-config.toml`, and emitting `.env` placeholders
//! next to the spin manifest.

use std::fs;
use std::path::Path;

use edgezero_adapter::env_file::{append_lines_dedup_with_header, EDGEZERO_PROVISION_HEADER};
use edgezero_adapter::registry::{ProvisionOutcome, ProvisionStores, TypedSecretEntry};

/// Local-mode provision arm: extend `[component.<id>].key_value_stores`
/// in `spin.toml`, append `[key_value_store.<platform>]` blocks (Spin
/// `SQLite` backend) to `runtime-config.toml`, and write
/// `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME=<platform>` lines
/// (all kinds) plus a commented `__KEY=<logical>_staging` placeholder
/// (CONFIG only) to `.env` next to `spin.toml`.
///
/// Both `spin.toml` and `runtime-config.toml` MUST exist at the
/// resolved paths -- Task 8b's CLI bootstrap writes both via
/// `synthesise_baseline_manifest` before provision runs. If either
/// is missing, we error clearly rather than silently re-synthesising:
/// a missing runtime-config next to a present spin.toml is a
/// programmer error worth surfacing (rather than silently mutating
/// the tree into an inconsistent state).
///
/// **Lookups use `store.logical`** (env-overlay-independent) for the
/// env-var KEY portion (`APP_CONFIG__NAME`); **TOML cells and env-var
/// VALUES use `store.platform`** (env-overlay resolved binding name
/// teammates can vary via `EDGEZERO__STORES__<KIND>__<ID>__NAME`).
pub(super) fn provision(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    component_selector: Option<&str>,
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    use toml_edit::DocumentMut;

    let spin_rel = adapter_manifest_path.unwrap_or("spin.toml");
    let spin_path = manifest_root.join(spin_rel);
    let spin_dir = spin_path.parent().unwrap_or(manifest_root);
    let rc_path = spin_dir.join("runtime-config.toml");
    let env_path = spin_dir.join(".env");

    if !spin_path.exists() {
        return Err(format!(
            "expected spin.toml at {} (Task 8b's CLI bootstrap should have written it before provision ran)",
            spin_path.display()
        ));
    }
    if !rc_path.exists() {
        return Err(format!(
            "expected runtime-config.toml at {} next to spin.toml (Task 8b's CLI bootstrap should have written it before provision ran)",
            rc_path.display()
        ));
    }

    // 1. spin.toml: append platform labels to [component.<id>].key_value_stores.
    let spin_raw = fs::read_to_string(&spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut spin_doc: DocumentMut = spin_raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;
    let mut spin_changed = false;
    let needs_component = !stores.kv.is_empty() || !stores.config.is_empty();
    if needs_component {
        let component_id = resolve_component_id(&spin_doc, component_selector, &spin_path)?;
        for store in stores.kv.iter().chain(stores.config.iter()) {
            if append_kv_store_to_component(
                &mut spin_doc,
                &component_id,
                &store.platform,
                &spin_path,
            )? {
                spin_changed = true;
            }
        }
    }

    // 2. runtime-config.toml: append [key_value_store.<platform>] blocks.
    let rc_raw = fs::read_to_string(&rc_path)
        .map_err(|err| format!("failed to read {}: {err}", rc_path.display()))?;
    let mut rc_doc: DocumentMut = rc_raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", rc_path.display()))?;
    let mut rc_changed = false;
    for store in stores.kv.iter().chain(stores.config.iter()) {
        if append_key_value_store_block(&mut rc_doc, &store.platform)? {
            rc_changed = true;
        }
    }

    // 3. .env: __NAME lines (all kinds) + commented __KEY placeholders
    // (CONFIG only). Dedup honours operator overrides -- an operator
    // who already uncommented + edited a __KEY line does NOT get the
    // commented placeholder re-added on a subsequent provision.
    let env_lines = build_env_lines(stores);

    if spin_changed && !dry_run {
        fs::write(&spin_path, spin_doc.to_string())
            .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    }
    if rc_changed && !dry_run {
        fs::write(
            &rc_path,
            normalise_runtime_config_header(rc_doc.to_string()),
        )
        .map_err(|err| format!("failed to write {}: {err}", rc_path.display()))?;
    }
    append_lines_dedup_with_header(
        &env_path,
        Some(EDGEZERO_PROVISION_HEADER),
        &env_lines,
        dry_run,
    )
    .map_err(|err| format!("write {}: {err}", env_path.display()))?;

    let total = stores
        .kv
        .len()
        .saturating_add(stores.config.len())
        .saturating_add(stores.secrets.len());
    let status_lines = vec![format!(
        "spin: wrote bindings + runtime-config + .env for {total} store(s) at {}",
        spin_path.display()
    )];
    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Resolve which `[component.<id>]` table `provision` /
/// `provision_typed` write into, given a parsed `spin.toml`. Same
/// rule as `resolve_spin_component` and
/// `Adapter::validate_adapter_manifest`:
/// - explicit `component_selector`: must match a declared component
///   id, else error;
/// - single component: implicit;
/// - multi-component without selector: error naming
///   `[adapters.spin.adapter].component` and listing available ids.
///
/// Operates on a `DocumentMut` (already parsed) so the callers can
/// share the single doc read with the writer.
fn resolve_component_id(
    doc: &toml_edit::DocumentMut,
    selector: Option<&str>,
    spin_path: &Path,
) -> Result<String, String> {
    let component_ids: Vec<String> = doc
        .get("component")
        .and_then(toml_edit::Item::as_table)
        .map(|tbl| tbl.iter().map(|(key, _)| key.to_owned()).collect())
        .unwrap_or_default();

    if component_ids.is_empty() {
        return Err(format!(
            "{}: no [component.*] declarations found",
            spin_path.display()
        ));
    }
    if let Some(sel) = selector {
        if component_ids.iter().any(|id| id == sel) {
            return Ok(sel.to_owned());
        }
        return Err(format!(
            "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
            sel,
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

/// Local-mode `provision_typed` arm: for each typed secret declared
/// on the app, edit `spin.toml` to add a lowercased `[variables]`
/// entry (`{ default = "", secret = true }`) plus a
/// `[component.<id>.variables]` binding that references it via the
/// `{{ spin_var }}` template placeholder, then write a
/// `SPIN_VARIABLE_<UPPER>=` line into `<spin_dir>/.env` so `spin up`
/// resolves the secret from the environment at runtime.
///
/// Casing: Spin's schema requires lowercase variable names
/// (`^[a-z][a-z0-9_]*$`); the Spin runtime reads variables from
/// upper-cased `SPIN_VARIABLE_*` env vars. `spin_var` is the
/// canonicalised (`to_ascii_lowercase`) secret key.
///
/// Idempotency: an existing `[variables].<spin_var>` entry is left
/// alone (operator override survives); the same rule applies to
/// `[component.<id>.variables].<spin_var>`. `.env` dedup is
/// delegated to [`append_lines_dedup_with_header`].
pub(super) fn provision_typed(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    component_selector: Option<&str>,
    typed_secrets: &[TypedSecretEntry<'_>],
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    use toml_edit::DocumentMut;

    let spin_rel = adapter_manifest_path.unwrap_or("spin.toml");
    let spin_path = manifest_root.join(spin_rel);
    let env_path = spin_path.parent().unwrap_or(manifest_root).join(".env");

    if !spin_path.exists() {
        return Err(format!(
            "expected spin.toml at {} (Task 8b's CLI bootstrap should have written it before provision ran)",
            spin_path.display()
        ));
    }

    let spin_raw = fs::read_to_string(&spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut spin_doc: DocumentMut = spin_raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;

    // Resolve the component id ONCE — if unresolvable (multi-component
    // with no selector, or a bad explicit selector), abort BEFORE
    // touching either the .env or spin.toml on disk.
    let component_id = resolve_component_id(&spin_doc, component_selector, &spin_path)?;

    let mut status_lines: Vec<String> = Vec::with_capacity(typed_secrets.len());
    let mut env_lines: Vec<String> = Vec::with_capacity(typed_secrets.len());
    let mut spin_changed = false;

    for entry in typed_secrets {
        let spin_var = entry.key_value.to_ascii_lowercase();
        let env_var = format!("SPIN_VARIABLE_{}", spin_var.to_ascii_uppercase());

        if upsert_variables_entry(&mut spin_doc, &spin_var, &spin_path)? {
            spin_changed = true;
        }
        if upsert_component_variable(&mut spin_doc, &component_id, &spin_var, &spin_path)? {
            spin_changed = true;
        }

        env_lines.push(format!("{env_var}="));
        status_lines.push(format!(
            "spin: variable `{spin_var}` on component `{component_id}` (env `{env_var}`)"
        ));
    }

    if spin_changed && !dry_run {
        fs::write(&spin_path, spin_doc.to_string())
            .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    }
    append_lines_dedup_with_header(
        &env_path,
        Some(EDGEZERO_PROVISION_HEADER),
        &env_lines,
        dry_run,
    )
    .map_err(|err| format!("write {}: {err}", env_path.display()))?;

    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Insert `[variables].<spin_var> = { default = "", secret = true }`
/// into `doc` if the key is absent. If a `[variables].<spin_var>`
/// entry already exists — commonly because the operator has
/// customised the `default` fallback or added extra metadata —
/// LEAVE it untouched so the operator override survives repeat
/// provisions. Returns `Ok(true)` if the entry was newly added,
/// `Ok(false)` if it was already present.
fn upsert_variables_entry(
    doc: &mut toml_edit::DocumentMut,
    spin_var: &str,
    spin_path: &Path,
) -> Result<bool, String> {
    use toml_edit::{InlineTable, Item, Table, Value};

    let variables = doc
        .entry("variables")
        .or_insert_with(|| Item::Table(Table::new()));
    let variables_tbl = variables
        .as_table_mut()
        .ok_or_else(|| format!("{}: `variables` is not a table", spin_path.display()))?;
    if variables_tbl.contains_key(spin_var) {
        return Ok(false);
    }
    let mut inline = InlineTable::new();
    inline.insert("default", Value::from(""));
    inline.insert("secret", Value::from(true));
    variables_tbl.insert(spin_var, Item::Value(Value::InlineTable(inline)));
    Ok(true)
}

/// Insert `[component.<component_id>.variables].<spin_var> = "{{ spin_var }}"`
/// (a literal placeholder string containing the template braces)
/// if the key is absent. LEAVES an existing binding alone so an
/// operator who has already wired the variable to a literal (or a
/// different template) survives a repeat provision. Returns
/// `Ok(true)` if newly added, `Ok(false)` if already present.
fn upsert_component_variable(
    doc: &mut toml_edit::DocumentMut,
    component_id: &str,
    spin_var: &str,
    spin_path: &Path,
) -> Result<bool, String> {
    use toml_edit::{value, Item, Table};

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
    let variables = target_tbl
        .entry("variables")
        .or_insert_with(|| Item::Table(Table::new()));
    let variables_tbl = variables.as_table_mut().ok_or_else(|| {
        format!(
            "{}: [component.{component_id}.variables] is not a table",
            spin_path.display()
        )
    })?;
    if variables_tbl.contains_key(spin_var) {
        return Ok(false);
    }
    variables_tbl.insert(spin_var, value(format!("{{{{ {spin_var} }}}}")));
    Ok(true)
}

/// In-memory variant of `ensure_kv_label_in_component`: append
/// `platform` to `[component.<component_id>].key_value_stores` in
/// `doc`. Creates the array if absent. Returns `Ok(true)` if the
/// label was newly added, `Ok(false)` if already present. The caller
/// writes the doc back to disk once at the end of `provision`
/// so multiple platform labels land in a single atomic write.
fn append_kv_store_to_component(
    doc: &mut toml_edit::DocumentMut,
    component_id: &str,
    platform: &str,
    spin_path: &Path,
) -> Result<bool, String> {
    use toml_edit::{value, Array, Value};

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
    if arr.iter().any(|item| item.as_str() == Some(platform)) {
        return Ok(false);
    }
    arr.push(platform);
    Ok(true)
}

/// Append `[key_value_store.<platform>]` with `type = "spin"` +
/// `path = ".spin/sqlite_key_value.db"` to `doc` if the platform's
/// stanza is absent. Idempotent — an already-present stanza is left
/// untouched (returns `false`). All local-mode stores back to the
/// same local `SQLite` file (Spin's default local KV backend).
///
/// The parent `[key_value_store]` table is set implicit so
/// `toml_edit` emits only `[key_value_store.<platform>]` section
/// headers, matching the shape `spin up` reads.
fn append_key_value_store_block(
    doc: &mut toml_edit::DocumentMut,
    platform: &str,
) -> Result<bool, String> {
    use toml_edit::{value, Item, Table};

    // Fast-path idempotency check: if a stanza for this platform
    // already exists, no work to do.
    if doc
        .get("key_value_store")
        .and_then(toml_edit::Item::as_table)
        .is_some_and(|tbl| tbl.contains_key(platform))
    {
        return Ok(false);
    }

    let parent = doc.entry("key_value_store").or_insert_with(|| {
        let mut parent_tbl = Table::new();
        parent_tbl.set_implicit(true);
        Item::Table(parent_tbl)
    });
    // `key_value_store` exists but is not a table (e.g. the file has
    // `key_value_store = "oops"`). Refuse to edit — mirrors the
    // "refusing to edit malformed local state" pattern the Fastly and
    // Cloudflare local arms use. Silently returning `Ok(false)` here
    // would let the caller write `spin.toml` with a
    // `key_value_stores = ["<platform>"]` binding that
    // `runtime-config.toml` never declares, leaving the runtime
    // unable to resolve the store at boot.
    let Some(parent_tbl) = parent.as_table_mut() else {
        return Err(format!(
            "runtime-config.toml: `key_value_store` exists but is not a table; refusing to edit in place (offending platform: `{platform}`)"
        ));
    };
    let mut inner = Table::new();
    inner.insert("type", value("spin"));
    inner.insert("path", value(".spin/sqlite_key_value.db"));
    parent_tbl.insert(platform, Item::Table(inner));
    Ok(true)
}

/// Ensure the schema-version header is the first content of a
/// serialised `runtime-config.toml`. `toml_edit` stores the bare
/// comment-only baseline from `synthesise_runtime_config_toml`
/// as the document's trailing decor; inserting the first
/// `[key_value_store.<label>]` table then shuffles that header
/// to the BOTTOM of the emitted string. Force it back to the
/// top so `runtime-config.toml` reliably starts with the schema-
/// version line, matching the invariant every other provision-
/// written file honours.
fn normalise_runtime_config_header(serialised: String) -> String {
    if serialised.starts_with(EDGEZERO_PROVISION_HEADER) {
        return serialised;
    }
    let mut out = String::with_capacity(
        serialised
            .len()
            .saturating_add(EDGEZERO_PROVISION_HEADER.len())
            .saturating_add(1),
    );
    out.push_str(EDGEZERO_PROVISION_HEADER);
    out.push('\n');
    for line in serialised.lines() {
        if line.trim() == EDGEZERO_PROVISION_HEADER {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Build the `.env` line set emitted by [`provision`].
///
/// One `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME=<platform>`
/// entry per declared store (KV / CONFIG / SECRETS). CONFIG stores
/// additionally get a **commented** `__KEY` placeholder — Spin has
/// no way to preview the KEY overlay at provision time, so we hint
/// the shape and let the operator uncomment + edit it.
///
/// Env-var KEY uses the LOGICAL id in uppercase so the runtime's
/// env-overlay lookup finds it regardless of teammates' platform
/// name overrides. Env-var VALUE uses the PLATFORM name so the
/// runtime opens the same Spin KV store name that `spin.toml`'s
/// `key_value_stores` array + `runtime-config.toml`'s stanza declare.
///
/// Dedup responsibility is delegated to [`append_lines_dedup_with_header`]: the
/// commented and uncommented forms normalise to the same key, so an
/// operator who already uncommented + edited a `__KEY` line survives
/// a re-run of provision (the commented placeholder is NOT re-added).
fn build_env_lines(stores: &ProvisionStores<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (kind, kind_stores) in [
        ("KV", stores.kv),
        ("CONFIG", stores.config),
        ("SECRETS", stores.secrets),
    ] {
        for store in kind_stores {
            let logical_upper = store.logical.to_ascii_uppercase();
            let platform = &store.platform;
            lines.push(format!(
                "EDGEZERO__STORES__{kind}__{logical_upper}__NAME={platform}"
            ));
        }
    }
    for store in stores.config {
        let logical_upper = store.logical.to_ascii_uppercase();
        let logical = &store.logical;
        lines.push(format!(
            "# EDGEZERO__STORES__CONFIG__{logical_upper}__KEY={logical}_staging"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::run::{synthesise_runtime_config_toml, synthesise_spin_toml};
    use super::super::SpinCliAdapter;
    use edgezero_adapter::registry::{
        Adapter as _, ProvisionMode, ProvisionStores, ResolvedStoreId, TypedSecretEntry,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` / `"demo"` per call site)
    // keeps the setup-vs-assertion pair in sync -- a typo in one
    // place no longer silently divorces from the other, because both
    // reference the same const. Also names the intent: these are the
    // LOGICAL store ids + spin component id the adapter operates on,
    // not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_KV_ID_ALT: &str = "cache";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";
    const TEST_COMPONENT_ID: &str = "demo";

    fn write_spin(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(&path, contents).expect("write spin.toml");
        path
    }

    // ---------- provision (dry-run + error path + idempotent skip) ----------

    #[test]
    fn provision_dry_run_does_not_edit_spin_toml() {
        let dir = tempdir().expect("tempdir");
        let original =
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let kv_ids: Vec<ResolvedStoreId> =
            ResolvedStoreId::from_logicals(&[TEST_KV_ID, TEST_KV_ID_ALT]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 2);
        assert!(out.status_lines[0].contains("would ensure KV label `sessions`"));
        assert!(out.status_lines[1].contains("would ensure KV label `cache`"));
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, original, "dry-run mutated spin.toml");
    }

    #[test]
    fn provision_writes_resolved_platform_label_into_kv_array() {
        // Regression: spin provision used to receive only logical
        // ids and add them verbatim to
        // `[component.X].key_value_stores`. With the platform-name
        // flow, an operator who sets
        // `EDGEZERO__STORES__KV__SESSIONS__NAME=prod_sessions` now
        // sees `prod_sessions` land as the KV label (matching what
        // the runtime opens), with the logical id preserved for
        // human-facing wording.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids = vec![ResolvedStoreId::new(TEST_KV_ID, "prod_sessions")];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("real-run succeeds");
        assert!(
            out.status_lines[0].contains("`prod_sessions`")
                && out.status_lines[0].contains("`sessions`"),
            "status line names BOTH the platform label and the logical id: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains("\"prod_sessions\""),
            "platform label written into spin.toml KV array: {after}"
        );
        assert!(
            !after.contains("\"sessions\""),
            "logical id is NOT written (would shadow the platform binding): {after}"
        );
    }

    #[test]
    fn provision_writes_kv_labels_into_resolved_component() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("real run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("added KV label `sessions`"),
            "got: {out:?}"
        );
        let after = fs::read_to_string(dir.path().join("spin.toml")).expect("read back");
        assert!(
            after.contains("\"sessions\""),
            "label landed in spin.toml: {after}"
        );
    }

    #[test]
    fn provision_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = SpinCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("spin.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_writes_config_labels_into_kv_array_and_leaves_secrets_manual() {
        // Stage 5: config now lives in Spin KV. Provision writes each
        // `[stores.config].id` into `[component.X].key_value_stores`
        // (same machinery as `[stores.kv]`). Secrets stay manual until
        // we ship native secret support.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &secret_ids,
        };
        let out = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("config + secrets provision succeeds");
        assert_eq!(out.status_lines.len(), 2);
        assert!(
            out.status_lines[0].contains("config label")
                && out.status_lines[0].contains("key_value_stores"),
            "config row reports KV-array write: {out:?}"
        );
        assert!(
            out.status_lines[1].contains("manual"),
            "secret row still flags manual declaration: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains(&format!("\"{TEST_CONFIG_ID}\"")),
            "config label landed in spin.toml: {after}"
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
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("no-store provision is fine");
        assert_eq!(
            out.status_lines,
            vec!["spin has no declared stores to provision"]
        );
    }

    // ---------- provision_local (Local arm) — Task 25 ----------

    /// Seed BOTH baseline files (spin.toml + runtime-config.toml) at
    /// `dir`, matching Task 24's `synthesise_baseline_manifest` output.
    fn seed_baseline(dir: &Path, app_name: &str) {
        fs::write(dir.join("spin.toml"), synthesise_spin_toml(app_name, None))
            .expect("seed spin.toml");
        fs::write(
            dir.join("runtime-config.toml"),
            synthesise_runtime_config_toml(),
        )
        .expect("seed runtime-config.toml");
    }

    #[test]
    fn spin_local_provision_writes_kv_bindings_and_runtime_config_blocks() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");

        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).expect("read spin.toml");
        // Both platform labels (KV + config) land in
        // [component.<id>].key_value_stores.
        assert!(
            spin_after.contains("\"sessions\""),
            "KV label in spin.toml: {spin_after}"
        );
        assert!(
            spin_after.contains("\"app_config\""),
            "config label in spin.toml: {spin_after}"
        );

        let rc_after = fs::read_to_string(dir.path().join("runtime-config.toml"))
            .expect("read runtime-config.toml");
        for label in ["sessions", "app_config"] {
            assert!(
                rc_after.contains(&format!("[key_value_store.{label}]")),
                "runtime-config has [key_value_store.{label}]: {rc_after}"
            );
        }
        assert!(
            rc_after.contains(r#"type = "spin""#),
            "type = \"spin\": {rc_after}"
        );
        assert!(
            rc_after.contains(r#"path = ".spin/sqlite_key_value.db""#),
            "sqlite path: {rc_after}"
        );
    }

    #[test]
    fn spin_local_provision_writes_env_name_lines_for_kv_config_secrets() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");

        let env_after = fs::read_to_string(dir.path().join(".env")).expect("read .env");
        assert!(
            env_after.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line: {env_after}"
        );
        assert!(
            env_after.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "kv __NAME line: {env_after}"
        );
        assert!(
            env_after.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default"),
            "secret __NAME line: {env_after}"
        );
        assert!(
            env_after.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "commented config __KEY placeholder: {env_after}"
        );
    }

    #[test]
    fn spin_local_provision_errors_if_spin_toml_absent() {
        let dir = tempdir().expect("tempdir");
        // Do NOT seed spin.toml. runtime-config.toml alone must not
        // paper over the missing spin.toml.
        fs::write(
            dir.path().join("runtime-config.toml"),
            synthesise_runtime_config_toml(),
        )
        .expect("seed runtime-config");

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing spin.toml must error");
        assert!(
            err.contains("spin.toml") && err.contains(dir.path().to_str().unwrap()),
            "error names the missing spin.toml path: {err}"
        );
    }

    #[test]
    fn spin_local_provision_errors_if_runtime_config_toml_absent() {
        let dir = tempdir().expect("tempdir");
        // Seed spin.toml but NOT runtime-config.toml. Missing
        // runtime-config next to a present spin.toml is a
        // programmer error worth surfacing (rather than silently
        // re-synthesising an inconsistent tree).
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml(TEST_COMPONENT_ID, None),
        )
        .expect("seed spin.toml");

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing runtime-config.toml must error");
        assert!(
            err.contains("runtime-config.toml"),
            "error names runtime-config.toml specifically: {err}"
        );
    }

    #[test]
    fn spin_local_provision_errors_when_runtime_config_key_value_store_is_not_a_table() {
        // Regression: if runtime-config.toml declares
        // `key_value_store = "oops"` (a scalar instead of a table),
        // `append_key_value_store_block` used to return `false`
        // silently — the caller then wrote spin.toml with a
        // `key_value_stores = ["sessions"]` binding that pointed at a
        // store label runtime-config.toml never declared. Spin would
        // fail to boot with a confusing lookup error. Now it errors
        // at provision time, matching the "refusing to edit malformed
        // local state" pattern the other adapters use.
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml(TEST_COMPONENT_ID, None),
        )
        .expect("seed spin.toml");
        // Malformed runtime-config.toml: scalar where a table
        // (or absence) is expected.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "# edgezero-provision: v1\nkey_value_store = \"oops\"\n",
        )
        .expect("seed malformed runtime-config.toml");

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect_err("malformed key_value_store must error");
        assert!(
            err.contains("key_value_store") && err.contains("not a table"),
            "error must name the malformed field and its shape: {err}"
        );
        // Sibling `spin.toml` and `.env` must NOT be written on the
        // error path — otherwise we'd corrupt the tree even if we
        // errored.
        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).expect("read spin.toml");
        assert!(
            !spin_after.contains(&format!("\"{TEST_KV_ID}\"")),
            "spin.toml must NOT list the KV binding when provision errored: {spin_after}"
        );
    }

    #[test]
    fn spin_local_provision_resolves_nested_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("crates/spin");
        fs::create_dir_all(&nested).expect("mkdir nested");
        seed_baseline(&nested, TEST_COMPONENT_ID);

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("crates/spin/spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("nested-path local provision succeeds");

        let spin_after = fs::read_to_string(nested.join("spin.toml")).expect("read spin.toml");
        assert!(
            spin_after.contains("\"sessions\""),
            "KV label lands in nested spin.toml: {spin_after}"
        );
        let rc_after =
            fs::read_to_string(nested.join("runtime-config.toml")).expect("read runtime-config");
        assert!(
            rc_after.contains("[key_value_store.sessions]"),
            "stanza lands in nested runtime-config.toml: {rc_after}"
        );
        assert!(
            nested.join(".env").exists(),
            ".env lands next to nested spin.toml"
        );
        assert!(
            !dir.path().join(".env").exists(),
            "root-level .env must NOT be written"
        );
    }

    #[test]
    fn spin_local_provision_dedup_preserves_operator_edited_env_lines() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        // Operator pre-seeds an uncommented __KEY override.
        fs::write(
            dir.path().join(".env"),
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override\n",
        )
        .expect("seed .env");

        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");

        let env_after = fs::read_to_string(dir.path().join(".env")).expect("read .env");
        assert!(
            env_after.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override"),
            "operator's uncommented __KEY line survives: {env_after}"
        );
        assert!(
            !env_after.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="),
            "commented __KEY placeholder must NOT be re-added: {env_after}"
        );
        // Exactly one line whose normalised key is the __KEY env-var
        // name -- the uncommented operator override wins.
        let key_lines = env_after
            .lines()
            .filter(|line| {
                let after_hash = line.trim_start().strip_prefix('#').unwrap_or(line);
                after_hash
                    .trim_start()
                    .starts_with("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=")
            })
            .count();
        assert_eq!(
            key_lines, 1,
            "exactly one __KEY line after dedup: {env_after}"
        );
    }

    #[test]
    fn spin_local_provision_uses_platform_binding_when_env_overlay_active() {
        // Env-overlay round-trip. Simulates
        //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config
        // via ResolvedStoreId::new(logical, platform). The env-var KEY
        // must still use the LOGICAL id in upper-case (`APP_CONFIG`);
        // the TOML cells + env-var VALUE use the PLATFORM name
        // (`prod_config`) so the runtime opens the store name that
        // spin.toml + runtime-config.toml declare.
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");

        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).expect("read spin.toml");
        assert!(
            spin_after.contains("\"prod_config\""),
            "spin.toml has platform label prod_config: {spin_after}"
        );
        assert!(
            !spin_after.contains("\"app_config\""),
            "spin.toml must NOT have the logical id app_config: {spin_after}"
        );

        let rc_after = fs::read_to_string(dir.path().join("runtime-config.toml"))
            .expect("read runtime-config");
        assert!(
            rc_after.contains("[key_value_store.prod_config]"),
            "runtime-config has [key_value_store.prod_config]: {rc_after}"
        );
        assert!(
            !rc_after.contains("[key_value_store.app_config]"),
            "runtime-config must NOT have logical-named stanza: {rc_after}"
        );

        let env_after = fs::read_to_string(dir.path().join(".env")).expect("read .env");
        assert!(
            env_after.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config"),
            "env-var key uses LOGICAL uppercase, value uses PLATFORM: {env_after}"
        );
        assert!(
            !env_after.contains("EDGEZERO__STORES__CONFIG__PROD_CONFIG__NAME="),
            "platform name must NOT leak into the env-var key: {env_after}"
        );
    }

    // ---------- provision_typed (Task 26) ----------

    #[test]
    fn spin_provision_typed_writes_lowercased_variables_and_uppercased_env() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml(TEST_COMPONENT_ID, None),
        )
        .unwrap();

        let entries = [TypedSecretEntry::new(
            "default",
            "API_TOKEN",
            "Demo_API_TOKEN",
        )];
        SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed local ok");

        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).unwrap();
        assert!(
            spin_after.contains("[variables]"),
            "root [variables] header: {spin_after}"
        );
        assert!(
            spin_after.contains("demo_api_token = { default = \"\", secret = true }"),
            "inline variables entry with default=\"\" and secret=true: {spin_after}"
        );
        assert!(
            spin_after.contains("[component.demo.variables]"),
            "component variables header: {spin_after}"
        );
        assert!(
            spin_after.contains(r#"demo_api_token = "{{ demo_api_token }}""#),
            "component-level template placeholder: {spin_after}"
        );

        let env_after = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(
            env_after
                .lines()
                .any(|line| line == "SPIN_VARIABLE_DEMO_API_TOKEN="),
            "SPIN_VARIABLE_<UPPER>= placeholder line: {env_after}"
        );
    }

    #[test]
    fn spin_provision_typed_uses_explicit_component_selector() {
        let dir = tempdir().unwrap();
        // Synthesise with component_selector = "worker" so the
        // [component.worker] table exists.
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml("demo", Some("worker")),
        )
        .unwrap();

        let entries = [TypedSecretEntry::new("default", "API_TOKEN", "demo_token")];
        SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                Some("worker"),
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed with selector ok");

        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).unwrap();
        assert!(
            spin_after.contains("[component.worker.variables]"),
            "selector target receives placeholder: {spin_after}"
        );
        assert!(
            !spin_after.contains("[component.demo.variables]"),
            "non-selected component id must NOT receive placeholder: {spin_after}"
        );
    }

    #[test]
    fn spin_provision_typed_errors_when_component_ambiguous_and_no_selector() {
        let dir = tempdir().unwrap();
        // Multi-component spin.toml with NO selector.
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n\
             [application]\nname = \"x\"\nversion = \"0\"\n\
             [component.foo]\nsource = \"foo.wasm\"\n\
             [component.bar]\nsource = \"bar.wasm\"\n",
        )
        .unwrap();

        let entries = [TypedSecretEntry::new("default", "API_TOKEN", "demo_token")];
        let err = SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect_err("ambiguous component must error");
        assert!(
            err.contains("foo")
                && err.contains("bar")
                && err.contains("[adapters.spin.adapter].component"),
            "error names available component ids AND the config knob: {err}"
        );
    }

    #[test]
    fn spin_provision_typed_errors_when_selector_does_not_match_component() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml(TEST_COMPONENT_ID, None),
        )
        .unwrap();

        let entries = [TypedSecretEntry::new("default", "API_TOKEN", "demo_token")];
        let err = SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                Some("missing"),
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect_err("bad selector must error");
        assert!(
            err.contains("missing"),
            "error names the missing selector: {err}"
        );
    }

    #[test]
    fn spin_provision_typed_cloud_mode_is_a_no_op() {
        let dir = tempdir().unwrap();
        // Do NOT seed spin.toml — cloud mode must return an empty
        // outcome WITHOUT touching the filesystem.
        let entries = [TypedSecretEntry::new("default", "API_TOKEN", "demo_token")];
        let out = SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Cloud,
                false,
            )
            .expect("cloud mode no-op ok");
        assert!(
            out.status_lines.is_empty(),
            "cloud mode emits no status lines: {:?}",
            out.status_lines
        );
        assert!(
            out.deployed.is_none(),
            "cloud mode carries no deployed state"
        );
        assert!(
            !dir.path().join("spin.toml").exists(),
            "cloud mode must NOT create spin.toml"
        );
        assert!(
            !dir.path().join(".env").exists(),
            "cloud mode must NOT create .env"
        );
    }

    #[test]
    fn spin_provision_typed_deduplicates_matching_variable() {
        let dir = tempdir().unwrap();
        // Operator has customised `default = "custom-fallback"` — a
        // repeat provision_typed must NOT clobber it.
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n\
             [application]\nname = \"demo\"\nversion = \"0\"\n\
             [[trigger.http]]\nroute = \"/...\"\ncomponent = \"demo\"\n\
             [component.demo]\nsource = \"demo.wasm\"\n\
             [variables]\ndemo_api_token = { default = \"custom-fallback\", secret = true }\n",
        )
        .unwrap();

        let entries = [TypedSecretEntry::new(
            "default",
            "API_TOKEN",
            "demo_api_token",
        )];
        SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("idempotent provision_typed ok");

        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).unwrap();
        assert!(
            spin_after.contains(r#"default = "custom-fallback""#),
            "operator's custom `default` value preserved: {spin_after}"
        );
    }

    #[test]
    fn spin_provision_typed_errors_if_spin_toml_absent() {
        let dir = tempdir().unwrap();
        // Do NOT seed spin.toml. Local mode must error naming the
        // missing baseline (Task 8b's CLI bootstrap should have
        // written it).
        let entries = [TypedSecretEntry::new("default", "API_TOKEN", "demo_token")];
        let err = SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing spin.toml must error");
        assert!(
            err.contains("spin.toml") && err.contains(dir.path().to_str().unwrap()),
            "error names the missing spin.toml path: {err}"
        );
    }

    // ---------- provision_local contract suite — Task 40 ----------
    //
    // Eight tests locking the local-mode provision contract:
    // four common tests shared with sibling adapters (first-run
    // shape, byte-identical re-provision, operator-edit
    // survival, no cloud calls) plus four Spin-specific
    // env-label alignment tests (spec §"Per-adapter test
    // contract" item 4) that guarantee Spin's three-file
    // cross-references (`.env`, `runtime-config.toml`,
    // `spin.toml`) can never drift. If any of the three sets
    // diverges, the Spin runtime lookup fails at boot with
    // "unknown key_value_stores label X".

    use super::super::env_mutation_guard;
    use std::collections::BTreeSet;
    use std::env;
    use std::ffi::OsString;

    /// RAII guard: prepends `extra` to `$PATH` on construct,
    /// restores the original value on drop. Must be held while
    /// `env_mutation_guard()` is locked.
    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            let original = env::var_os("PATH");
            let new_path = match &original {
                Some(prev) => {
                    let mut accum = OsString::from(extra);
                    accum.push(":");
                    accum.push(prev);
                    accum
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new_path);
            Self { original }
        }
    }

    #[cfg(unix)]
    impl Drop for PathPrepend {
        fn drop(&mut self) {
            match self.original.take() {
                Some(prev) => env::set_var("PATH", prev),
                None => env::remove_var("PATH"),
            }
        }
    }

    /// RAII guard for a single arbitrary env var. Sets on
    /// construct, restores the previous value (or removes) on
    /// drop. Must be held while `env_mutation_guard()` is
    /// locked.
    struct SetVar {
        key: String,
        original: Option<OsString>,
    }

    impl SetVar {
        fn new(key: &str, value: &str) -> Self {
            let original = env::var_os(key);
            env::set_var(key, value);
            Self {
                key: key.to_owned(),
                original,
            }
        }
    }

    impl Drop for SetVar {
        fn drop(&mut self) {
            match self.original.take() {
                Some(prev) => env::set_var(&self.key, prev),
                None => env::remove_var(&self.key),
            }
        }
    }

    /// Shell-script shim named `spin` that fails loudly if ever
    /// invoked. The Spin local-mode provision path is pure file
    /// editing — no shell-out — so this fake must never fire.
    /// `zero_cloud_calls` prepends the shim's directory to PATH
    /// and asserts provision still returns Ok.
    #[cfg(unix)]
    fn fake_spin_panicking() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("spin");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho 'spin was called during local provision' >&2\nexit 42\n",
        )
        .expect("write fake spin");
        let mut perms = fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod");
        dir
    }

    /// Extract `EDGEZERO__STORES__*__NAME=<value>` values from
    /// `.env` into a set. Commented lines are skipped so the
    /// commented `__KEY` placeholders don't inflate the set.
    fn extract_env_names(env_content: &str) -> BTreeSet<String> {
        env_content
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| key.contains("__NAME"))
            .map(|(_, val)| val.trim().to_owned())
            .collect()
    }

    /// Extract `[key_value_store.<name>]` block names from
    /// `runtime-config.toml` into a set.
    fn extract_runtime_config_store_names(doc: &toml_edit::DocumentMut) -> BTreeSet<String> {
        doc.get("key_value_store")
            .and_then(toml_edit::Item::as_table)
            .map(|tbl| tbl.iter().map(|(key, _)| key.to_owned()).collect())
            .unwrap_or_default()
    }

    /// Extract `[component.<id>].key_value_stores = [...]`
    /// array entries from `spin.toml` into a set.
    fn extract_spin_component_bindings(
        doc: &toml_edit::DocumentMut,
        component_id: &str,
    ) -> BTreeSet<String> {
        doc.get("component")
            .and_then(|item| item.get(component_id))
            .and_then(|item| item.get("key_value_stores"))
            .and_then(toml_edit::Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(toml_edit::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Run `SpinCliAdapter::provision(Local, dry_run=false)`
    /// with the given kv / config / secrets logical ids
    /// (platform names default to the logical ids).
    fn run_local_provision(dir: &Path, kv: &[&str], config: &[&str], secrets: &[&str]) {
        let kv_ids = ResolvedStoreId::from_logicals(kv);
        let config_ids = ResolvedStoreId::from_logicals(config);
        let secret_ids = ResolvedStoreId::from_logicals(secrets);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        SpinCliAdapter
            .provision(
                dir,
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
    }

    // ---- Common tests (shared shape with sibling adapters) ----

    #[test]
    fn provision_local_first_run_writes_expected_files() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(dir.path(), &[TEST_KV_ID], &[TEST_CONFIG_ID], &[]);

        let spin_path = dir.path().join("spin.toml");
        let rc_path = dir.path().join("runtime-config.toml");
        let env_path = dir.path().join(".env");
        assert!(spin_path.exists(), "spin.toml must exist");
        assert!(rc_path.exists(), "runtime-config.toml must exist");
        assert!(env_path.exists(), ".env must exist");

        let spin_after = fs::read_to_string(&spin_path).unwrap();
        let rc_after = fs::read_to_string(&rc_path).unwrap();
        let env_after = fs::read_to_string(&env_path).unwrap();

        for (label, content) in [
            ("spin.toml", &spin_after),
            ("runtime-config.toml", &rc_after),
            (".env", &env_after),
        ] {
            assert!(
                content.starts_with("# edgezero-provision: v1"),
                "{label} must start with schema header: {content}"
            );
        }

        // spin.toml has both labels in [component.demo].key_value_stores.
        let spin_doc: toml_edit::DocumentMut = spin_after.parse().unwrap();
        let bindings = extract_spin_component_bindings(&spin_doc, TEST_COMPONENT_ID);
        let expected: BTreeSet<String> = [TEST_KV_ID, TEST_CONFIG_ID]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(bindings, expected, "spin.toml component bindings");

        // runtime-config.toml has [key_value_store.<label>] blocks
        // with the Spin SQLite backend defaults.
        for label in [TEST_KV_ID, TEST_CONFIG_ID] {
            assert!(
                rc_after.contains(&format!("[key_value_store.{label}]")),
                "runtime-config missing stanza for {label}: {rc_after}"
            );
        }
        assert!(
            rc_after.contains(r#"type = "spin""#),
            r#"runtime-config missing type = "spin": {rc_after}"#
        );
        assert!(
            rc_after.contains(r#"path = ".spin/sqlite_key_value.db""#),
            "runtime-config missing default SQLite path: {rc_after}"
        );

        // .env has the __NAME lines for both kinds.
        assert!(
            env_after.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "KV __NAME line: {env_after}"
        );
        assert!(
            env_after.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line: {env_after}"
        );
    }

    #[test]
    fn provision_local_re_provision_is_byte_identical() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(dir.path(), &[TEST_KV_ID], &[TEST_CONFIG_ID], &[]);
        let spin_first = fs::read(dir.path().join("spin.toml")).unwrap();
        let rc_first = fs::read(dir.path().join("runtime-config.toml")).unwrap();
        let env_first = fs::read(dir.path().join(".env")).unwrap();

        run_local_provision(dir.path(), &[TEST_KV_ID], &[TEST_CONFIG_ID], &[]);
        let spin_second = fs::read(dir.path().join("spin.toml")).unwrap();
        let rc_second = fs::read(dir.path().join("runtime-config.toml")).unwrap();
        let env_second = fs::read(dir.path().join(".env")).unwrap();

        assert_eq!(spin_first, spin_second, "spin.toml byte-identical");
        assert_eq!(rc_first, rc_second, "runtime-config.toml byte-identical");
        assert_eq!(env_first, env_second, ".env byte-identical");
    }

    /// Renamed 2026-07 (deep self-review finding P1-f): the prior
    /// name `provision_local_push_after_provision_preserves_*`
    /// promised a push→provision integration test but the body
    /// re-runs `provision_typed` twice.
    #[test]
    fn provision_typed_local_re_run_preserves_operator_spin_variable_value() {
        // Operator installs a real secret value into a
        // `SPIN_VARIABLE_<UPPER>=…` line; a subsequent
        // provision_typed run must NOT clobber it. Key-normalised
        // dedup in append_lines_dedup_with_header collapses the
        // empty placeholder against the operator's value so the
        // operator edit survives byte-for-byte.
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("spin.toml"),
            synthesise_spin_toml(TEST_COMPONENT_ID, None),
        )
        .unwrap();

        let entries = [TypedSecretEntry::new(
            "default",
            "API_TOKEN",
            "demo_api_token",
        )];
        SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();

        let env_path = dir.path().join(".env");
        let after_first = fs::read_to_string(&env_path).unwrap();
        assert!(
            after_first
                .lines()
                .any(|line| line == "SPIN_VARIABLE_DEMO_API_TOKEN="),
            "first provision emits the empty SPIN_VARIABLE_ placeholder: {after_first}"
        );

        // Operator uncomments-value: fills in the real value.
        let operator_edited = after_first.replace(
            "SPIN_VARIABLE_DEMO_API_TOKEN=",
            "SPIN_VARIABLE_DEMO_API_TOKEN=real_secret_value",
        );
        fs::write(&env_path, &operator_edited).unwrap();

        SpinCliAdapter
            .provision_typed(
                dir.path(),
                Some("spin.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();

        let after_second = fs::read_to_string(&env_path).unwrap();
        assert!(
            after_second.contains("SPIN_VARIABLE_DEMO_API_TOKEN=real_secret_value"),
            "operator value survives re-provision: {after_second}"
        );
        assert!(
            !after_second
                .lines()
                .any(|line| line == "SPIN_VARIABLE_DEMO_API_TOKEN="),
            "empty placeholder must NOT be re-added: {after_second}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn provision_local_zero_cloud_calls() {
        // Local mode is pure file editing. A fake `spin` on
        // PATH that panics on invocation must never fire — if
        // it did, the exit-42 would surface via subprocess
        // status or file-side effects (there are none).
        let _lock = env_mutation_guard().lock().expect("guard");
        let fake = fake_spin_panicking();
        let _path = PathPrepend::new(fake.path());

        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(
            dir.path(),
            &[TEST_KV_ID],
            &[TEST_CONFIG_ID],
            &[TEST_SECRET_ID],
        );

        // Success is the assertion — provision returned Ok
        // (no shell-out to the panicking shim happened).
        assert!(dir.path().join("spin.toml").exists());
        assert!(dir.path().join("runtime-config.toml").exists());
        assert!(dir.path().join(".env").exists());
    }

    // ---- Spin-specific env-label alignment tests ----

    #[test]
    fn provision_local_writes_expected_env_lines() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(
            dir.path(),
            &[TEST_KV_ID],
            &[TEST_CONFIG_ID],
            &[TEST_SECRET_ID],
        );
        let env_after = fs::read_to_string(dir.path().join(".env")).unwrap();
        for expected in [
            "EDGEZERO__STORES__KV__SESSIONS__NAME=sessions",
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config",
            "EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default",
        ] {
            assert!(
                env_after.contains(expected),
                "missing expected line `{expected}`: {env_after}"
            );
        }
    }

    /// THE contract: `.env` __NAME values, runtime-config
    /// store names, and spin.toml component bindings MUST
    /// reference the exact same set of labels. If any of the
    /// three sets diverges, the Spin runtime lookup fails at
    /// boot with "unknown `key_value_stores` label X".
    ///
    /// Only KV + config are declared here — secrets land in
    /// `.env` only (no runtime-config stanza, no spin.toml
    /// binding), so including them would break set equality
    /// by construction.
    #[test]
    fn provision_local_labels_line_up() {
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(
            dir.path(),
            &[TEST_KV_ID, TEST_KV_ID_ALT],
            &[TEST_CONFIG_ID],
            &[],
        );

        let env_after = fs::read_to_string(dir.path().join(".env")).unwrap();
        let rc_after = fs::read_to_string(dir.path().join("runtime-config.toml")).unwrap();
        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).unwrap();
        let rc_doc: toml_edit::DocumentMut = rc_after.parse().unwrap();
        let spin_doc: toml_edit::DocumentMut = spin_after.parse().unwrap();

        let env_names = extract_env_names(&env_after);
        let runtime_config_stores = extract_runtime_config_store_names(&rc_doc);
        let spin_bindings = extract_spin_component_bindings(&spin_doc, TEST_COMPONENT_ID);

        let expected: BTreeSet<String> = [TEST_KV_ID, TEST_KV_ID_ALT, TEST_CONFIG_ID]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(env_names, expected, ".env __NAME values: {env_after}");
        assert_eq!(
            runtime_config_stores, expected,
            "runtime-config stores: {rc_after}"
        );
        assert_eq!(
            spin_bindings, expected,
            "spin.toml component bindings: {spin_after}"
        );
        // The load-bearing set-equality assertion: any drift
        // between the three files means the runtime lookup
        // fails at boot.
        assert_eq!(env_names, runtime_config_stores);
        assert_eq!(runtime_config_stores, spin_bindings);
    }

    #[test]
    fn provision_local_env_overlay_round_trips() {
        // Simulate the CLI's env-overlay resolution: the
        // process env carries the __NAME override AND the
        // constructed ResolvedStoreId reflects the resolved
        // platform binding the CLI would compute. The env
        // var itself does not drive the adapter code (which
        // consumes ProvisionStores) — the guard is there
        // because the process env is shared with parallel
        // test threads.
        let _lock = env_mutation_guard().lock().expect("guard");
        let _var = SetVar::new("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "prod_config");

        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        SpinCliAdapter
            .provision(
                dir.path(),
                Some("spin.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("provision with overlay ok");

        let env_after = fs::read_to_string(dir.path().join(".env")).unwrap();
        let rc_after = fs::read_to_string(dir.path().join("runtime-config.toml")).unwrap();
        let spin_after = fs::read_to_string(dir.path().join("spin.toml")).unwrap();
        let rc_doc: toml_edit::DocumentMut = rc_after.parse().unwrap();
        let spin_doc: toml_edit::DocumentMut = spin_after.parse().unwrap();

        let env_names = extract_env_names(&env_after);
        let runtime_config_stores = extract_runtime_config_store_names(&rc_doc);
        let spin_bindings = extract_spin_component_bindings(&spin_doc, TEST_COMPONENT_ID);

        let expected: BTreeSet<String> = ["prod_config".to_owned()].into_iter().collect();
        assert_eq!(env_names, expected, ".env carries prod_config: {env_after}");
        assert_eq!(runtime_config_stores, expected, "runtime-config stores");
        assert_eq!(spin_bindings, expected, "spin.toml bindings");
        assert_eq!(env_names, runtime_config_stores);
        assert_eq!(runtime_config_stores, spin_bindings);
    }

    #[test]
    fn re_provision_preserves_operator_uncommented_override() {
        // Task 16c dedup contract: first provision writes a
        // commented `# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=<logical>_staging`
        // placeholder. Operator uncomments AND changes the value.
        // Re-run provision must:
        //   1. leave the operator line byte-for-byte,
        //   2. NOT re-add the commented placeholder (key-normalised
        //      dedup: commented and uncommented forms collapse).
        let dir = tempdir().expect("tempdir");
        seed_baseline(dir.path(), TEST_COMPONENT_ID);
        run_local_provision(dir.path(), &[], &[TEST_CONFIG_ID], &[]);

        let env_path = dir.path().join(".env");
        let after_first = fs::read_to_string(&env_path).unwrap();
        let placeholder = "# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging";
        assert!(
            after_first.contains(placeholder),
            "first provision writes the commented placeholder: {after_first}"
        );

        // Operator uncomments AND edits.
        let operator_line = "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=my_custom_override";
        let operator_edited = after_first.replace(placeholder, operator_line);
        fs::write(&env_path, &operator_edited).unwrap();

        run_local_provision(dir.path(), &[], &[TEST_CONFIG_ID], &[]);

        let after_second = fs::read_to_string(&env_path).unwrap();
        assert!(
            after_second.contains(operator_line),
            "operator uncommented + edited __KEY line preserved: {after_second}"
        );
        assert!(
            !after_second.contains(placeholder),
            "commented placeholder must NOT be re-added: {after_second}"
        );

        // Exactly one line whose normalised key is the __KEY
        // env var — the uncommented operator override wins,
        // proving key-normalised dedup collapses the two forms.
        let key_lines = after_second
            .lines()
            .filter(|line| {
                let after_hash = line.trim_start().strip_prefix('#').unwrap_or(line);
                after_hash
                    .trim_start()
                    .starts_with("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=")
            })
            .count();
        assert_eq!(
            key_lines, 1,
            "exactly one __KEY line after dedup: {after_second}"
        );
    }
}
