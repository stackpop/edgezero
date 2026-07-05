use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use edgezero_adapter::env_file::{append_lines_dedup_with_header, EDGEZERO_PROVISION_HEADER};
use edgezero_adapter::registry::{AdapterDeployedState, ProvisionOutcome, ProvisionStores};

use super::provision_cloud::is_real_namespace_id;

/// If `path` already declares a `[[kv_namespaces]]` entry with
/// `binding = binding` AND its `id` looks like a real Cloudflare
/// namespace id, return that id. Returns `Ok(None)` if the binding
/// is absent OR present with a placeholder id (so provision can
/// treat both cases as "needs (re-)create"). A failure to read /
/// parse the file is a hard error -- provision needs an authoritative
/// answer.
pub(super) fn existing_real_namespace_id(
    path: &Path,
    binding: &str,
) -> Result<Option<String>, String> {
    let Some(existing) = read_namespace_id(path, binding)? else {
        return Ok(None);
    };
    if is_real_namespace_id(&existing) {
        Ok(Some(existing))
    } else {
        Ok(None)
    }
}

/// Internal: look up `binding`'s `id` in `wrangler.toml` without
/// the "did you run provision?" error path that `find_namespace_id`
/// adds. Missing file -> `Ok(None)`. Returns the raw id whether or
/// not it looks like a real Cloudflare id.
///
/// Errors loudly if `kv_namespaces` exists but is neither an
/// array-of-tables nor an inline-array (e.g. the operator typed
/// `kv_namespaces = "oops"`). Silently returning `None` there
/// surfaces downstream as "did you run provision?" -- misleading,
/// because the actual problem is a malformed manifest.
pub(super) fn read_namespace_id(path: &Path, binding: &str) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let id = match doc.get("kv_namespaces") {
        Some(Item::ArrayOfTables(arr)) => arr.iter().find_map(|table| {
            if table.get("binding").and_then(Item::as_str) == Some(binding) {
                table.get("id").and_then(Item::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(Item::Value(Value::Array(arr))) => arr.iter().find_map(|item| {
            let table = item.as_inline_table()?;
            if table.get("binding").and_then(Value::as_str) == Some(binding) {
                table.get("id").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(other) => {
            return Err(format!(
                "{}: `kv_namespaces` exists but is neither `[[kv_namespaces]]` (array-of-tables) nor an inline array of `{{ binding, id }}` records; got TOML item of type `{}`",
                path.display(),
                item_kind(other)
            ));
        }
        None => None,
    };
    Ok(id)
}

/// Refuse to provision a new namespace when `wrangler.toml`'s
/// `kv_namespaces` exists in a form that `upsert_kv_namespace`
/// can't write back to. Today that means the inline-array form
/// (`kv_namespaces = [{ binding = "...", id = "..." }]`), which
/// `read_namespace_id` tolerates but `upsert_kv_namespace`'s
/// `as_array_of_tables_mut()` rejects. Without this guard, the
/// orphan-namespace hazard documented in `upsert_kv_namespace`
/// reappears: `wrangler kv namespace create` succeeds, then
/// upsert errors out and the new namespace lingers on
/// Cloudflare with no local writeback to track it. Missing or
/// array-of-tables forms are OK.
pub(super) fn check_kv_namespaces_writeback_shape(path: &Path) -> Result<(), String> {
    use toml_edit::{DocumentMut, Item, Value};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    match doc.get("kv_namespaces") {
        None | Some(Item::ArrayOfTables(_)) => Ok(()),
        Some(Item::Value(Value::Array(_))) => Err(format!(
            "{}: `kv_namespaces` is declared as an inline array (`kv_namespaces = [{{ binding = \"...\", id = \"...\" }}]`); provision can only write back through the `[[kv_namespaces]]` array-of-tables form. Convert each entry to a `[[kv_namespaces]]` block BEFORE re-running provision; otherwise a successful `wrangler kv namespace create` would leave the new namespace orphaned on Cloudflare with no local entry to track it.",
            path.display()
        )),
        Some(other) => Err(format!(
            "{}: `kv_namespaces` exists but is neither `[[kv_namespaces]]` (array-of-tables) nor an inline array of `{{ binding, id }}` records; got TOML item of type `{}`. Convert it manually before re-running provision.",
            path.display(),
            item_kind(other)
        )),
    }
}

/// One-line label for a `toml_edit::Item` (for diagnostic
/// messages -- not a canonical TOML type description).
fn item_kind(item: &toml_edit::Item) -> &'static str {
    use toml_edit::{Item, Value};
    match item {
        Item::None => "none",
        Item::Value(Value::String(_)) => "string",
        Item::Value(Value::Integer(_)) => "integer",
        Item::Value(Value::Float(_)) => "float",
        Item::Value(Value::Boolean(_)) => "boolean",
        Item::Value(Value::Datetime(_)) => "datetime",
        Item::Value(Value::Array(_)) => "array",
        Item::Value(Value::InlineTable(_)) => "inline-table",
        Item::Table(_) => "table",
        Item::ArrayOfTables(_) => "array-of-tables",
    }
}

/// Insert OR update the `[[kv_namespaces]]` entry for `binding`,
/// rewriting `id` if the binding already exists (e.g. provision
/// is replacing a `local-dev-placeholder`). Used by provision so
/// re-running on a scaffolded wrangler.toml replaces the placeholder
/// with the real id instead of silently skipping.
///
/// Caveat: `toml_edit::Table::insert` replaces the value's `Item`,
/// which drops any trailing inline comment that was attached to
/// the prior `id = "..."` line (e.g. `id = "old"  # delete me`).
/// Sibling fields under the same `[[kv_namespaces]]` table are
/// preserved verbatim -- only the `id` line's decor is lost.
///
/// Concurrency: provision is NOT safe to run concurrently against
/// the same `wrangler.toml`. Two concurrent runs may both miss the
/// idempotency check, both call `wrangler kv namespace create`
/// remotely, then race the file write -- the loser's namespace
/// becomes an orphan in the Cloudflare account. `EdgeZero` does not
/// take a lockfile; operators must serialise provision themselves.
pub(super) fn upsert_kv_namespace(path: &Path, binding: &str, id: &str) -> Result<(), String> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    // Treat NotFound as "start with empty document" symmetrically with
    // `read_namespace_id` so the orphan-namespace hazard goes away: if
    // wrangler.toml is missing entirely (e.g. operator deleted it
    // between scaffold and provision), the upsert that follows a
    // successful `wrangler kv namespace create` would otherwise error
    // out, leaving the remote namespace orphaned.
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let entry = doc
        .entry("kv_namespaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let arr_of_tables = entry.as_array_of_tables_mut().ok_or_else(|| {
        format!(
            "{}: `kv_namespaces` exists but is not an array-of-tables (`[[kv_namespaces]]`); convert it manually before re-running provision",
            path.display()
        )
    })?;

    let existing_idx = arr_of_tables
        .iter()
        .position(|table| table.get("binding").and_then(Item::as_str) == Some(binding));
    if let Some(idx) = existing_idx {
        if let Some(existing) = arr_of_tables.get_mut(idx) {
            existing.insert("id", value(id));
        }
    } else {
        let mut new_table = Table::new();
        new_table.insert("binding", value(binding));
        new_table.insert("id", value(id));
        arr_of_tables.push(new_table);
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

/// Local-mode provision arm: rewrite `[[kv_namespaces]]` entries in
/// the adapter's `wrangler.toml` for every declared KV / config
/// store, applying the deployed-precedence rule.
///
/// Precedence for the `id` cell of each entry:
/// 1. `deployed.sub_tables["kv_namespaces"][store.logical]` — the
///    cloud-side id recorded from a prior Cloud provision.
/// 2. The existing local `id` on a `[[kv_namespaces]]` entry whose
///    `binding` matches `store.platform`. Preserves operator-set
///    ids on file-based (no-cloud) setups.
/// 3. `format!("<placeholder-namespace-id-{}>", store.logical)`.
///
/// `preview_id` is written ONLY from
/// `deployed.sub_tables["preview_kv_namespaces"][store.logical]`; it
/// is never synthesised (matches the Cloud arm, which also omits
/// `preview_id` unless the operator provides one).
///
/// **Lookups use `store.logical`** (env-overlay-independent, stable
/// across machines); **TOML cells use `store.platform`** (env-overlay
/// resolved binding name teammates can vary via
/// `EDGEZERO__STORES__<KIND>__<ID>__NAME`).
///
/// Assumes `wrangler.toml` already exists at the resolved path
/// (Task 8b's CLI bootstrap writes it before provision runs); if it
/// is missing, returns an error naming the path rather than silently
/// re-synthesising, since the adapter trait does not receive an
/// `app_name` to synthesise with.
pub(super) fn provision(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    stores: &ProvisionStores<'_>,
    deployed: Option<&AdapterDeployedState>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    use toml_edit::DocumentMut;

    let wrangler_rel = adapter_manifest_path.unwrap_or("wrangler.toml");
    let wrangler_path = manifest_root.join(wrangler_rel);
    if !wrangler_path.exists() {
        return Err(format!(
            "expected wrangler.toml at {} (Task 8b's CLI bootstrap should have written it before provision ran)",
            wrangler_path.display()
        ));
    }
    let raw = fs::read_to_string(&wrangler_path)
        .map_err(|err| format!("failed to read {}: {err}", wrangler_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", wrangler_path.display()))?;

    let mut status_lines: Vec<String> = Vec::new();
    for store in stores.kv.iter().chain(stores.config.iter()) {
        // Lookups use LOGICAL id.
        let deployed_id = deployed
            .and_then(|state| state.sub_tables.get("kv_namespaces"))
            .and_then(|kv| kv.get(&store.logical))
            .map(String::as_str);
        let deployed_preview = deployed
            .and_then(|state| state.sub_tables.get("preview_kv_namespaces"))
            .and_then(|kv| kv.get(&store.logical))
            .map(String::as_str);
        let placeholder = format!("<placeholder-namespace-id-{}>", store.logical);

        // TOML cells use PLATFORM binding.
        let resolved_id = upsert_kv_namespace_entry(
            &mut doc,
            &wrangler_path,
            &store.platform,
            deployed_id,
            deployed_preview,
            &placeholder,
        )?;
        status_lines.push(format!(
            "cloudflare: kv binding `{}` -> id `{}` (logical id `{}`) in {}",
            store.platform,
            resolved_id,
            store.logical,
            wrangler_path.display(),
        ));
    }

    if !dry_run {
        fs::write(&wrangler_path, doc.to_string())
            .map_err(|err| format!("failed to write {}: {err}", wrangler_path.display()))?;
    }

    // `.dev.vars` lives NEXT TO the resolved wrangler.toml so
    // `wrangler dev` picks it up automatically for nested layouts
    // (e.g. `adapter_manifest_path = "crates/cf/wrangler.toml"`).
    let dev_vars_path = wrangler_path
        .parent()
        .unwrap_or(manifest_root)
        .join(".dev.vars");
    let dev_vars_lines = build_dev_vars_lines(stores);
    append_lines_dedup_with_header(
        &dev_vars_path,
        Some(EDGEZERO_PROVISION_HEADER),
        &dev_vars_lines,
        dry_run,
    )
    .map_err(|err| format!("write {}: {err}", dev_vars_path.display()))?;
    status_lines.push(format!(
        "cloudflare: wrote {} .dev.vars entries to {}",
        dev_vars_lines.len(),
        dev_vars_path.display()
    ));

    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Build the `.dev.vars` line set emitted by [`provision`].
///
/// One `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME="<platform>"`
/// entry per declared store (KV / CONFIG / SECRETS). CONFIG stores
/// additionally get a **commented** `__KEY` placeholder — Cloudflare
/// has no way to preview the KEY overlay at provision time, so we
/// hint the shape and let the operator uncomment + fill it in.
///
/// Dedup responsibility is delegated to
/// [`edgezero_adapter::env_file::append_lines_dedup`]: because the
/// commented and uncommented forms normalise to the same key, an
/// operator who already uncommented + edited a KEY line survives a
/// re-run of provision — the commented placeholder is not re-added.
fn build_dev_vars_lines(stores: &ProvisionStores<'_>) -> Vec<String> {
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
                r#"EDGEZERO__STORES__{kind}__{logical_upper}__NAME="{platform}""#
            ));
        }
    }
    for store in stores.config {
        let logical_upper = store.logical.to_ascii_uppercase();
        let logical = &store.logical;
        lines.push(format!(
            r#"# EDGEZERO__STORES__CONFIG__{logical_upper}__KEY="{logical}_staging""#
        ));
    }
    lines
}

/// In-memory upsert of a single `[[kv_namespaces]]` entry inside
/// `doc`, matched by `binding = platform`. Precedence for the
/// resolved id and `preview_id` is documented on [`provision`].
///
/// Returns the id cell as written so the caller can name it in the
/// operator-facing status line.
///
/// Errors if `kv_namespaces` exists but is not an array-of-tables --
/// symmetric with [`upsert_kv_namespace`]'s check. Missing
/// `kv_namespaces` is created as an empty array-of-tables and the
/// new entry appended.
fn upsert_kv_namespace_entry(
    doc: &mut toml_edit::DocumentMut,
    path: &Path,
    platform: &str,
    deployed_id: Option<&str>,
    deployed_preview: Option<&str>,
    placeholder: &str,
) -> Result<String, String> {
    use toml_edit::{value, ArrayOfTables, Item, Table};

    let entry = doc
        .entry("kv_namespaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let arr = entry.as_array_of_tables_mut().ok_or_else(|| {
        format!(
            "{}: `kv_namespaces` exists but is not an array-of-tables (`[[kv_namespaces]]`); convert it manually before re-running provision",
            path.display()
        )
    })?;

    let existing_idx = arr
        .iter()
        .position(|table| table.get("binding").and_then(Item::as_str) == Some(platform));
    let resolved_id = if let Some(idx) = existing_idx {
        // Existing entry: replace id from deployed if present,
        // otherwise leave existing id in place (operator-set /
        // prior placeholder). Only fall back to a fresh placeholder
        // if the existing entry has NO id at all.
        let existing_id = arr
            .get(idx)
            .and_then(|table| table.get("id").and_then(Item::as_str).map(str::to_owned));
        let resolved = deployed_id
            .map(str::to_owned)
            .or(existing_id)
            .unwrap_or_else(|| placeholder.to_owned());
        if let Some(table) = arr.get_mut(idx) {
            table.insert("id", value(&resolved));
            if let Some(preview) = deployed_preview {
                table.insert("preview_id", value(preview));
            }
        }
        resolved
    } else {
        // No matching entry: append a new `[[kv_namespaces]]` table.
        let resolved = deployed_id.unwrap_or(placeholder).to_owned();
        let mut new_table = Table::new();
        new_table.insert("binding", value(platform));
        new_table.insert("id", value(&resolved));
        if let Some(preview) = deployed_preview {
            new_table.insert("preview_id", value(preview));
        }
        arr.push(new_table);
        resolved
    };
    Ok(resolved_id)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::path_mutation_guard;
    use super::super::run::synthesise_wrangler_toml;
    use super::super::CloudflareCliAdapter;
    use super::*;
    use edgezero_adapter::env_file::EDGEZERO_PROVISION_HEADER;
    use edgezero_adapter::registry::{
        Adapter as _, AdapterDeployedState, ProvisionMode, ProvisionStores, ResolvedStoreId,
    };
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::path::PathBuf;
    use tempfile::tempdir;

    const TEST_KV_ID: &str = "sessions";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            let original = env::var_os("PATH");
            let new = match &original {
                Some(prev) => {
                    let mut accum = OsString::from(extra);
                    accum.push(":");
                    accum.push(prev);
                    accum
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new);
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

    /// A wrangler shim that fails loudly if invoked. Used by
    /// `provision_local_zero_cloud_calls` to prove local-mode
    /// provision never shells out to the real Cloudflare CLI:
    /// if provision returns `Ok(_)` with THIS script on PATH,
    /// the shim was NEVER called.
    #[cfg(unix)]
    fn fake_wrangler_panicking() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("wrangler");
        fs::write(
            &script_path,
            "#!/bin/sh\necho 'wrangler was called during local provision' >&2\nexit 42\n",
        )
        .expect("write fake wrangler");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    fn write_wrangler(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("wrangler.toml");
        fs::write(&path, contents).expect("write wrangler.toml");
        path
    }

    /// Build an `AdapterDeployedState` with a single
    /// `kv_namespaces.<logical> = <namespace_id>` mapping. Keeps the
    /// per-test fixture terse.
    fn deployed_kv(logical: &str, namespace_id: &str) -> AdapterDeployedState {
        let mut kv = BTreeMap::new();
        kv.insert(logical.to_owned(), namespace_id.to_owned());
        let mut state = AdapterDeployedState::default();
        state.sub_tables.insert("kv_namespaces".to_owned(), kv);
        state
    }

    // ---------- read_namespace_id ----------

    #[test]
    fn read_namespace_id_errors_when_kv_namespaces_is_non_array_value() {
        // `kv_namespaces = "oops"` is a malformed manifest. Silently
        // returning None there bubbles up as "did you run provision?"
        // -- a misleading error. The right surface is "manifest
        // doesn't match the expected shape".
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), "name = \"demo\"\nkv_namespaces = \"oops\"\n");
        let err = read_namespace_id(&path, TEST_CONFIG_ID)
            .expect_err("non-array kv_namespaces must error");
        assert!(
            err.contains("array-of-tables") || err.contains("inline array"),
            "error names the expected shapes: {err}"
        );
        assert!(
            err.contains("string"),
            "error names the offending kind: {err}"
        );
    }

    // ---------- upsert_kv_namespace ----------

    #[test]
    fn upsert_kv_namespace_replaces_placeholder_id_for_existing_binding() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "placeholder replaced: {after}"
        );
        assert!(
            !after.contains("local-dev-placeholder"),
            "placeholder removed: {after}"
        );
        assert_eq!(
            after.matches("binding = \"sessions\"").count(),
            1,
            "no duplicate binding: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_appends_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), "name = \"demo\"\n");
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("binding = \"sessions\"")
                && after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "appended new entry: {after}"
        );
        assert!(
            after.contains("name = \"demo\""),
            "preserved original keys: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_appends_next_to_existing_entries() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"cache\"\nid = \"old\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("binding = \"cache\"") && after.contains("id = \"old\""),
            "existing entry kept: {after}"
        );
        assert!(
            after.contains("binding = \"sessions\""),
            "new entry added: {after}"
        );
        assert_eq!(
            after.matches("[[kv_namespaces]]").count(),
            2,
            "two entries: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_preserves_top_comments() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "# managed by hand -- please keep this line\nname = \"my-worker\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("# managed by hand"),
            "preserved comment: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_preserves_sibling_fields_on_existing_entry() {
        // toml_edit replaces only the `id` Item when we update it;
        // sibling fields on the same `[[kv_namespaces]]` table
        // (e.g. `preview_id`, custom annotations the user added)
        // must survive the rewrite. Pinning this so a future
        // toml_edit upgrade or a refactor can't silently drop
        // operator data.
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\npreview_id = \"local-preview\"\ndescription = \"hand-added by ops\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "id rewritten: {after}"
        );
        assert!(
            after.contains("preview_id = \"local-preview\""),
            "preserved preview_id: {after}"
        );
        assert!(
            after.contains("description = \"hand-added by ops\""),
            "preserved description: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_creates_file_when_wrangler_toml_missing() {
        // Orphan-namespace hazard: if `wrangler kv namespace create`
        // succeeds but wrangler.toml is missing at writeback time,
        // erroring here would leave the remote namespace orphaned
        // with no local reference. Symmetric with read_namespace_id's
        // NotFound -> Ok(None) behaviour: upsert treats NotFound as
        // "start with empty document" and writes the entry.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        assert!(!path.exists(), "precondition: file must not exist");
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff")
            .expect("missing file is permissive");
        let after = fs::read_to_string(&path).expect("file now exists");
        assert!(
            after.contains("binding = \"sessions\""),
            "created file with new entry: {after}"
        );
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "id written: {after}"
        );
    }

    // ---------- writeback shape pre-check ----------

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        check_kv_namespaces_writeback_shape(&path)
            .expect("missing file is permissive (upsert creates it)");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_kv_namespaces_absent() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write wrangler.toml");
        check_kv_namespaces_writeback_shape(&path).expect("no kv_namespaces => OK");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(
            &path,
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        )
        .expect("write wrangler.toml");
        check_kv_namespaces_writeback_shape(&path)
            .expect("[[kv_namespaces]] is the writeback-supported shape");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_rejects_inline_array_with_actionable_message() {
        // Regression for the orphan-namespace hazard: pre-fix, a
        // `kv_namespaces = [{ binding = "sessions" }]` manifest (no
        // id present) made `read_namespace_id` return None ("not yet
        // provisioned") so provision shelled `wrangler kv namespace
        // create` successfully, then `upsert_kv_namespace`'s
        // `as_array_of_tables_mut()` returned None and the upsert
        // errored — leaving the freshly-created namespace orphaned
        // on Cloudflare. The pre-flight rejects the inline-array
        // shape BEFORE any account-side call.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(
            &path,
            "name = \"demo\"\nkv_namespaces = [{ binding = \"sessions\" }]\n",
        )
        .expect("write wrangler.toml");
        let err = check_kv_namespaces_writeback_shape(&path)
            .expect_err("inline-array form must be rejected before provision shells out");
        assert!(
            err.contains("inline array")
                && err.contains("[[kv_namespaces]]")
                && err.contains("orphaned"),
            "error must name the inline-array form, the supported [[kv_namespaces]] form, AND the orphan hazard so the operator knows what's at stake: {err}"
        );
    }

    // ---------- provision (Local mode) ----------

    #[test]
    fn cloudflare_local_provision_emits_bindings_with_placeholders_when_no_deployed() {
        // [stores.kv].ids = ["sessions"], no deployed block.
        // Expect the freshly-written entry to carry the placeholder id,
        // and NOT emit a preview_id at all (deployed lookup only).
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        assert!(
            out.deployed.is_none(),
            "local provision must not repopulate deployed: {:?}",
            out.deployed
        );
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("[[kv_namespaces]]"),
            "array-of-tables header emitted: {after}"
        );
        assert!(
            after.contains("binding = \"sessions\""),
            "binding named after logical (no env overlay): {after}"
        );
        assert!(
            after.contains("id = \"<placeholder-namespace-id-sessions>\""),
            "placeholder id derived from logical: {after}"
        );
        assert!(
            !after.contains("preview_id"),
            "preview_id must NOT be synthesised without deployed data: {after}"
        );
    }

    #[test]
    fn cloudflare_local_provision_uses_deployed_namespace_id_when_set() {
        // Deployed carries `kv_namespaces.sessions = "abc123"`.
        // Expect the id cell in wrangler.toml to be "abc123" (deployed
        // wins over placeholder).
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let state = deployed_kv(TEST_KV_ID, "abc123");
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                Some(&state),
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        assert!(out.deployed.is_none());
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"abc123\""),
            "deployed id wins over placeholder: {after}"
        );
        assert!(
            !after.contains("<placeholder-namespace-id-sessions>"),
            "no placeholder emitted when deployed provides an id: {after}"
        );
    }

    #[test]
    fn cloudflare_local_provision_preserves_sibling_operator_keys() {
        // Operator hand-added `usage_model = "bundled"` on the
        // [[kv_namespaces]] table. Provision must overwrite `id` from
        // deployed but leave `usage_model` untouched.
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"operator-set\"\nusage_model = \"bundled\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let state = deployed_kv(TEST_KV_ID, "from-cloud");
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                Some(&state),
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"from-cloud\""),
            "deployed id wins over existing local id: {after}"
        );
        assert!(
            after.contains("usage_model = \"bundled\""),
            "operator sibling key preserved: {after}"
        );
        assert_eq!(
            after.matches("binding = \"sessions\"").count(),
            1,
            "no duplicate binding entry: {after}"
        );
    }

    #[test]
    fn cloudflare_local_provision_falls_back_to_existing_local_id_when_no_deployed() {
        // No deployed. Existing local id = "operator-set" is
        // preserved (precedence: deployed -> existing -> placeholder).
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"operator-set\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"operator-set\""),
            "existing local id preserved when no deployed: {after}"
        );
        assert!(
            !after.contains("<placeholder-namespace-id-sessions>"),
            "no placeholder emitted when existing id is present: {after}"
        );
    }

    #[test]
    fn cloudflare_local_provision_resolves_nested_adapter_manifest_path() {
        // Mirrors the app-demo layout: adapter_manifest_path =
        // "crates/cf/wrangler.toml". Pre-seed the nested file (Task
        // 8b's CLI bootstrap does this before provision runs).
        // Assert the upsert lands in the nested file and NOT in a
        // sibling wrangler.toml at manifest_root.
        let dir = tempdir().expect("tempdir");
        let nested_dir = dir.path().join("crates").join("cf");
        fs::create_dir_all(&nested_dir).expect("mkdir nested");
        let nested_path = nested_dir.join("wrangler.toml");
        fs::write(&nested_path, synthesise_wrangler_toml("demo")).expect("seed nested");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&nested_path).expect("read nested");
        assert!(
            after.contains("binding = \"sessions\""),
            "upsert landed in nested wrangler.toml: {after}"
        );
        assert!(
            after.contains("id = \"<placeholder-namespace-id-sessions>\""),
            "placeholder id written into nested wrangler.toml: {after}"
        );
        // A sibling wrangler.toml at manifest_root must NOT have
        // been created.
        assert!(
            !dir.path().join("wrangler.toml").exists(),
            "no sibling wrangler.toml at manifest_root: {}",
            dir.path().display()
        );
    }

    #[test]
    fn cloudflare_local_provision_errors_if_manifest_absent() {
        // Same nested path, but no pre-seed. The adapter trait
        // doesn't receive app_name -- provision cannot synthesise
        // the manifest itself; that's Task 8b's job.
        let dir = tempdir().expect("tempdir");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing wrangler.toml must error");
        assert!(
            err.contains("crates/cf/wrangler.toml") || err.contains("crates\\cf\\wrangler.toml"),
            "error names the missing path: {err}"
        );
        assert!(
            err.contains("wrangler.toml"),
            "error mentions wrangler.toml: {err}"
        );
    }

    #[test]
    fn cloudflare_local_provision_writes_platform_binding_looks_up_deployed_by_logical() {
        // Env-overlay round-trip. Simulates
        //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config
        // via ResolvedStoreId::new(logical, platform).
        //
        // Deployed is keyed by LOGICAL ("app_config"); the binding
        // cell in wrangler.toml must be PLATFORM ("prod_config").
        // Bug that collapses the split would either write
        //   binding = "app_config"    (wrong: platform ignored)
        // OR fail to find the deployed id (wrong: lookup used
        // platform instead of logical).
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        let state = deployed_kv(TEST_CONFIG_ID, "abc123");
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                Some(&state),
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("binding = \"prod_config\""),
            "binding cell uses PLATFORM name: {after}"
        );
        assert!(
            !after.contains("binding = \"app_config\""),
            "logical id must NOT leak into the binding cell: {after}"
        );
        assert!(
            after.contains("id = \"abc123\""),
            "deployed id resolved via LOGICAL lookup: {after}"
        );
    }

    // ---------- provision (Local mode) — .dev.vars emission ----------

    #[test]
    fn cloudflare_local_provision_writes_dev_vars_name_lines() {
        // Fixture: [stores.config].ids = ["app_config"],
        // [stores.kv].ids = ["sessions"]. No .dev.vars pre-existing.
        // Provision must land the file next to wrangler.toml with a
        // __NAME line per store and a commented __KEY placeholder for
        // the config store.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let dev_vars = fs::read_to_string(dir.path().join(".dev.vars")).expect("read .dev.vars");
        assert!(
            dev_vars.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME="sessions""#),
            "KV __NAME line present: {dev_vars}"
        );
        assert!(
            dev_vars.contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME="app_config""#),
            "CONFIG __NAME line present: {dev_vars}"
        );
        assert!(
            dev_vars
                .contains(r#"# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="app_config_staging""#),
            "commented CONFIG __KEY placeholder present: {dev_vars}"
        );
    }

    #[test]
    fn cloudflare_local_provision_dev_vars_dedup_respects_commented_overrides() {
        // Operator has already uncommented + edited the KEY line.
        // Re-running provision must NOT re-add the commented
        // placeholder — normalised_key collapses commented and
        // uncommented forms, so the operator's value survives.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let dev_vars_path = dir.path().join(".dev.vars");
        fs::write(
            &dev_vars_path,
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=\"real_staging\"\n",
        )
        .expect("seed .dev.vars");

        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let dev_vars = fs::read_to_string(&dev_vars_path).expect("read .dev.vars");
        assert!(
            dev_vars.contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="real_staging""#),
            "operator's uncommented KEY line survives: {dev_vars}"
        );
        assert!(
            !dev_vars
                .contains(r#"# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="app_config_staging""#),
            "commented placeholder must NOT be re-added: {dev_vars}"
        );
        // Exactly one line whose normalised key matches the KEY
        // env-var name. The uncommented one wins.
        let key_lines = dev_vars
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
            "exactly one KEY line remains after dedup: {dev_vars}"
        );
    }

    #[test]
    fn cloudflare_local_provision_dev_vars_uses_platform_name_when_env_overlay_active() {
        // Env-overlay round-trip. Simulates
        //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config
        // via ResolvedStoreId::new(logical, platform). The emitted
        // __NAME line's VALUE must be the env-resolved platform
        // (`prod_config`); the ENV-VAR KEY must still use the
        // LOGICAL id in upper-case (`APP_CONFIG`) so the runtime's
        // env-overlay lookup finds it.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), &synthesise_wrangler_toml("demo"));
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let dev_vars = fs::read_to_string(dir.path().join(".dev.vars")).expect("read .dev.vars");
        assert!(
            dev_vars.contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME="prod_config""#),
            "value uses PLATFORM name, env-var key uses LOGICAL: {dev_vars}"
        );
        assert!(
            !dev_vars.contains("EDGEZERO__STORES__CONFIG__PROD_CONFIG__NAME="),
            "platform name must NOT leak into the env-var key: {dev_vars}"
        );
    }

    // ---------- provision_local_ contract suite (spec §"Per-adapter test contract") ----------

    #[test]
    fn provision_local_first_run_writes_expected_files() {
        // First-run fixture: empty crate dir, no wrangler.toml, no
        // .dev.vars. The CLI's bootstrap layer (Task 8b's
        // `write_baseline_to_disk`) normally primes wrangler.toml via
        // `synthesise_baseline_manifest` BEFORE provision runs; this
        // test mirrors that step directly, then calls
        // `provision(Local)` on the seed.
        //
        // Contract: `wrangler.toml` lands at the resolved path;
        // `.dev.vars` lands next to it; BOTH files carry the
        // `# edgezero-provision: v1` schema header (Section 5 review
        // fix); wrangler.toml has a `[[kv_namespaces]]` entry bound to
        // `sessions`; `.dev.vars` has the __NAME overlay line.
        let dir = tempdir().expect("tempdir");
        let wrangler_path = dir.path().join("wrangler.toml");
        fs::write(&wrangler_path, synthesise_wrangler_toml("demo"))
            .expect("bootstrap wrangler.toml");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("first-run local provision succeeds");
        assert!(
            wrangler_path.exists(),
            "wrangler.toml exists at resolved path"
        );
        let dev_vars_path = dir.path().join(".dev.vars");
        assert!(
            dev_vars_path.exists(),
            ".dev.vars lands next to wrangler.toml: {}",
            dev_vars_path.display()
        );
        let wrangler = fs::read_to_string(&wrangler_path).expect("read wrangler.toml");
        assert!(
            wrangler.starts_with(EDGEZERO_PROVISION_HEADER),
            "wrangler.toml starts with schema header: {wrangler}"
        );
        assert!(
            wrangler.contains("[[kv_namespaces]]"),
            "wrangler.toml has [[kv_namespaces]]: {wrangler}"
        );
        assert!(
            wrangler.contains("binding = \"sessions\""),
            "wrangler.toml binds `sessions`: {wrangler}"
        );
        let dev_vars = fs::read_to_string(&dev_vars_path).expect("read .dev.vars");
        assert!(
            dev_vars.starts_with(EDGEZERO_PROVISION_HEADER),
            ".dev.vars starts with schema header: {dev_vars}"
        );
        assert!(
            dev_vars.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME="sessions""#),
            ".dev.vars carries the __NAME overlay: {dev_vars}"
        );
    }

    /// Locks the header-preservation contract for the case the sibling
    /// first-run test misses. The seeded fixture there uses
    /// `synthesise_wrangler_toml("demo")` which ALREADY carries the
    /// header at line 1 -- a merge bug that stripped the header on
    /// re-serialisation would pass `starts_with(EDGEZERO_PROVISION_HEADER)`
    /// only because the seed matched, not because provision preserved it.
    /// This test starts from a wrangler.toml with the schema header at
    /// line 1 AND a couple of operator-added TOML lines, runs provision,
    /// and asserts the header STILL sits at line 1 on the output.
    #[test]
    fn provision_local_preserves_schema_header_at_line_1_after_merge() {
        let dir = tempdir().expect("tempdir");
        let wrangler_path = dir.path().join("wrangler.toml");
        // Seed matches the synthesiser shape, then adds an operator's
        // `main =` line below the header. If provision's toml_edit
        // round-trip re-orders root decor or drops the leading comment,
        // the header slides down.
        fs::write(
            &wrangler_path,
            "# edgezero-provision: v1\nname = \"demo\"\ncompatibility_date = \"2024-01-01\"\nmain = \"src/index.ts\"\n",
        )
        .expect("seed wrangler.toml");

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("provision must succeed on a seeded wrangler.toml with operator edits");

        let wrangler = fs::read_to_string(&wrangler_path).expect("read wrangler.toml");
        let first_line = wrangler.lines().next().unwrap_or_default();
        assert_eq!(
            first_line, "# edgezero-provision: v1",
            "schema header must sit at line 1 after merge (bare `.starts_with(...)` masks a merge bug that slides the header down): {wrangler}"
        );
        // Operator's line still present.
        assert!(
            wrangler.contains("main = \"src/index.ts\""),
            "operator's `main` key must survive the merge: {wrangler}"
        );
    }

    #[test]
    fn provision_local_re_provision_is_byte_identical() {
        // Re-running provision on an already-provisioned fixture must
        // produce byte-identical wrangler.toml and .dev.vars — the
        // second run is a no-op at the file level. Any drift here
        // (rewriting a differently-formatted TOML, re-appending the
        // header, appending a duplicate __NAME line) would surface as
        // a byte mismatch.
        let dir = tempdir().expect("tempdir");
        let wrangler_path = dir.path().join("wrangler.toml");
        fs::write(&wrangler_path, synthesise_wrangler_toml("demo"))
            .expect("bootstrap wrangler.toml");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("first local provision succeeds");
        let dev_vars_path = dir.path().join(".dev.vars");
        let wrangler_first = fs::read(&wrangler_path).expect("read wrangler.toml (first run)");
        let dev_vars_first = fs::read(&dev_vars_path).expect("read .dev.vars (first run)");
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("second local provision succeeds");
        let wrangler_second = fs::read(&wrangler_path).expect("read wrangler.toml (second run)");
        let dev_vars_second = fs::read(&dev_vars_path).expect("read .dev.vars (second run)");
        assert_eq!(
            wrangler_first, wrangler_second,
            "wrangler.toml must be byte-identical across two provision runs"
        );
        assert_eq!(
            dev_vars_first, dev_vars_second,
            ".dev.vars must be byte-identical across two provision runs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn provision_local_zero_cloud_calls() {
        // Install a panicking `wrangler` shim on PATH: if ever
        // invoked, it prints to stderr and exits 42, which surfaces
        // as an `Err` out of any `Command::new("wrangler").output()`
        // caller. `provision(Local)` MUST NOT shell out — it operates
        // purely on local files (wrangler.toml + .dev.vars). A
        // successful `Ok(_)` here is the proof: had a regression
        // routed Local through a shell-out path, the shim would have
        // failed loudly instead.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let wrangler_path = dir.path().join("wrangler.toml");
        fs::write(&wrangler_path, synthesise_wrangler_toml("demo"))
            .expect("bootstrap wrangler.toml");
        let fake = fake_wrangler_panicking();
        let _path = PathPrepend::new(fake.path());

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision must not shell out to wrangler");
    }
}
