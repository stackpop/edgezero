use std::fs;
use std::path::Path;

use edgezero_adapter::registry::{
    AdapterDeployedState, ProvisionOutcome, ProvisionStores, TypedSecretEntry,
};

/// Local-mode provision: seed Viceroy state in `fastly.toml` for the
/// declared stores + the `edgezero_runtime_env` runtime-override
/// store. NO shell-outs to `fastly` -- everything is a `toml_edit`
/// mutation, so operators can run `provision --local` without
/// authenticating.
///
/// The manifest must already exist (Task 8b's CLI bootstrap writes it
/// via `synthesise_fastly_toml`); we deliberately don't re-synthesise
/// here because the app name isn't in scope at this call site.
///
/// `deployed.fields.get("service_id")`, when present, is upserted to
/// the top-level `service_id` key -- spec says the deployed
/// service-id wins over anything the operator pre-seeded from a stale
/// template. When `deployed` has no `service_id` we leave any existing
/// value alone (operator's local seed is authoritative).
///
/// All other mutations (kv-store blocks, config-store blocks, runtime
/// override block) are idempotent — re-running is a no-op.
pub(super) fn provision(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    stores: &ProvisionStores<'_>,
    deployed: Option<&AdapterDeployedState>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    use toml_edit::DocumentMut;

    let fastly_rel = adapter_manifest_path.unwrap_or("fastly.toml");
    let fastly_path = manifest_root.join(fastly_rel);
    if !fastly_path.exists() {
        return Err(format!(
            "expected fastly.toml at {} (Task 8b's CLI bootstrap should have written it before provision ran)",
            fastly_path.display()
        ));
    }
    let raw = fs::read_to_string(&fastly_path)
        .map_err(|err| format!("failed to read {}: {err}", fastly_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", fastly_path.display()))?;

    let mut status_lines: Vec<String> = Vec::new();

    // 1. Upsert top-level `service_id` from deployed. Applies to BOTH
    //    synthesis and MERGE paths -- operators who pre-seeded
    //    fastly.toml from a stale template still get the cloud-
    //    authoritative id pinned. No cloud authority => leave any
    //    existing operator-set value alone.
    //
    // TOML root-key positioning matters here: once the parsed doc has
    // any headed sub-table (`[scripts]`, `[local_server]`, …), a naive
    // `doc.insert("service_id", …)` appends the scalar AFTER those
    // headers, and the re-serialised file parses the value as
    // `local_server.service_id`. `upsert_root_scalar_before_tables`
    // preserves the "scalars before sub-tables" TOML rule regardless
    // of insertion order.
    if let Some(sid) = deployed.and_then(|state| state.fields.get("service_id")) {
        upsert_root_scalar_before_tables(&mut doc, "service_id", sid.as_str());
        status_lines.push(format!(
            "fastly: pinned service_id = \"{sid}\" from deployed"
        ));
    }

    // Path suffix threaded into each status line so the operator sees
    // exactly which file each mutation landed in. Cheap to include
    // per-line and load-bearing when the manifest lives in a nested
    // adapter crate (`crates/demo-fastly/fastly.toml`) rather than at
    // the project root.
    let path_display = fastly_path.display().to_string();

    // 2. [[local_server.kv_stores.<platform>]] per KV store.
    for store in stores.kv {
        upsert_local_kv_store(&mut doc, &store.platform)?;
        status_lines.push(format!(
            "fastly: local kv_store `{}` (logical id `{}`) in {path_display}",
            store.platform, store.logical
        ));
    }

    // 3. [local_server.config_stores.<platform>] + empty `.contents`
    //    sub-table per CONFIG store. `contents` MUST be a TOML table
    //    (not `contents = ""`) -- the `config push --local` writer
    //    edits it in place via `as_table_mut()`.
    for store in stores.config {
        upsert_local_config_store(&mut doc, &store.platform)?;
        status_lines.push(format!(
            "fastly: local config_store `{}` (logical id `{}`) in {path_display}",
            store.platform, store.logical
        ));
    }

    // 4. `edgezero_runtime_env` block: __NAME lines for all kinds +
    //    commented __KEY placeholders for CONFIG stores. Same
    //    discipline as Cloudflare `.dev.vars`.
    if upsert_runtime_env_config_store(&mut doc, stores)? {
        status_lines.push(format!(
            "fastly: wrote edgezero_runtime_env block in {path_display}"
        ));
    }

    if !dry_run {
        fs::write(&fastly_path, doc.to_string())
            .map_err(|err| format!("failed to write {}: {err}", fastly_path.display()))?;
    }

    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Local-mode `provision_typed`: append `[[local_server.secret_stores.<store_id>]]`
/// entries in `fastly.toml`. Cloud secret storage uses `fastly secret-store-entry
/// create` at deploy time — the caller in `mod.rs` gates this on `ProvisionMode::Local`
/// and returns `Ok(ProvisionOutcome::default())` for cloud mode.
pub(super) fn provision_typed(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    typed_secrets: &[TypedSecretEntry<'_>],
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    let fastly_rel = adapter_manifest_path.unwrap_or("fastly.toml");
    let fastly_path = manifest_root.join(fastly_rel);
    if !fastly_path.exists() {
        return Err(format!(
            "expected fastly.toml at {} (Task 8b's CLI bootstrap should have written it before provision ran)",
            fastly_path.display()
        ));
    }
    let raw = fs::read_to_string(&fastly_path)
        .map_err(|err| format!("failed to read {}: {err}", fastly_path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", fastly_path.display()))?;

    let mut status_lines: Vec<String> = Vec::new();
    let mut appended = 0_usize;

    let path_display = fastly_path.display().to_string();
    for entry in typed_secrets {
        let added = upsert_secret_store_entry(&mut doc, entry.store_id, entry.key_value)?;
        if added {
            appended = appended.saturating_add(1);
        }
        status_lines.push(format!(
            "fastly: secret_store `{}` key `{}` (env `{}`) in {path_display}",
            entry.store_id,
            entry.key_value,
            entry.key_value.to_ascii_uppercase(),
        ));
    }

    if !dry_run && appended > 0 {
        fs::write(&fastly_path, doc.to_string())
            .map_err(|err| format!("failed to write {}: {err}", fastly_path.display()))?;
    }

    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Upsert a scalar key at the root of `doc`, guaranteeing it lands
/// BEFORE any headed sub-table.
///
/// TOML root-key rule: once a `[header]` opens a sub-table, every
/// subsequent `key = "value"` line is parsed as a child of that header.
/// `toml_edit::DocumentMut::insert` appends at end-of-order, so
/// inserting a root scalar after the doc has picked up any `[scripts]`
/// / `[local_server]` header from parse silently produces
/// `local_server.<key>` on re-emit.
///
/// If the key already exists, `insert` preserves its position (the
/// value cell is overwritten in place), so we only need the reorder
/// dance in the fresh-insert case: hoist every root-level sub-table
/// / array-of-tables out, insert the scalar, then re-attach the
/// tables in original order. Preserves comments and decor on both
/// the scalar's neighbours and the sub-tables via `toml_edit`'s
/// per-item decor tracking.
fn upsert_root_scalar_before_tables(doc: &mut toml_edit::DocumentMut, key: &str, val: &str) {
    use toml_edit::value;
    let table = doc.as_table_mut();
    if table.contains_key(key) {
        table.insert(key, value(val));
        return;
    }
    let sub_table_keys: Vec<String> = table
        .iter()
        .filter(|(_, item)| item.is_table() || item.is_array_of_tables())
        .map(|(name, _)| name.to_owned())
        .collect();
    let mut removed: Vec<(String, toml_edit::Item)> = Vec::with_capacity(sub_table_keys.len());
    for name in sub_table_keys {
        if let Some(item) = table.remove(&name) {
            removed.push((name, item));
        }
    }
    table.insert(key, value(val));
    for (name, item) in removed {
        table.insert(&name, item);
    }
}

/// Append `[[local_server.kv_stores.<platform_name>]]` with a stub
/// `key = "__init__"` / `data = ""` row to `doc`, IFF no entry with
/// that platform name already exists. Idempotent.
fn upsert_local_kv_store(
    doc: &mut toml_edit::DocumentMut,
    platform_name: &str,
) -> Result<(), String> {
    use toml_edit::{value, ArrayOfTables, Item, Table};

    let local_server_entry = doc
        .entry("local_server")
        .or_insert_with(|| Item::Table(Table::new()));
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        "`local_server` exists but is not a table; refusing to edit in place".to_owned()
    })?;
    let kv_stores_entry = local_server_tbl
        .entry("kv_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let kv_stores_tbl = kv_stores_entry.as_table_mut().ok_or_else(|| {
        "`local_server.kv_stores` exists but is not a table; refusing to edit in place".to_owned()
    })?;
    // Idempotent: skip if an array-of-tables (or anything) already
    // registered for this platform name.
    if kv_stores_tbl.contains_key(platform_name) {
        return Ok(());
    }
    let mut arr = ArrayOfTables::new();
    let mut row = Table::new();
    row.insert("key", value("__init__"));
    row.insert("data", value(""));
    arr.push(row);
    kv_stores_tbl.insert(platform_name, Item::ArrayOfTables(arr));
    Ok(())
}

/// Insert `[local_server.config_stores.<platform_name>]` with
/// `format = "inline-toml"` and an EMPTY `contents` sub-TABLE. The
/// empty table (NOT `contents = ""`) is load-bearing: the Fastly
/// `config push --local` writer edits `contents` in place via
/// `as_table_mut()` and refuses to proceed if it isn't a table.
/// Idempotent — skip if the block already exists.
fn upsert_local_config_store(
    doc: &mut toml_edit::DocumentMut,
    platform_name: &str,
) -> Result<(), String> {
    use toml_edit::{value, Item, Table};

    let local_server_entry = doc
        .entry("local_server")
        .or_insert_with(|| Item::Table(Table::new()));
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        "`local_server` exists but is not a table; refusing to edit in place".to_owned()
    })?;
    let config_stores_entry = local_server_tbl
        .entry("config_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let config_stores_tbl = config_stores_entry.as_table_mut().ok_or_else(|| {
        "`local_server.config_stores` exists but is not a table; refusing to edit in place"
            .to_owned()
    })?;
    if config_stores_tbl.contains_key(platform_name) {
        return Ok(());
    }
    let mut store_tbl = Table::new();
    store_tbl.set_implicit(false);
    store_tbl.insert("format", value("inline-toml"));
    let mut contents_tbl = Table::new();
    contents_tbl.set_implicit(false);
    store_tbl.insert("contents", Item::Table(contents_tbl));
    config_stores_tbl.insert(platform_name, Item::Table(store_tbl));
    Ok(())
}

/// Ensure `[local_server.config_stores.edgezero_runtime_env]` exists
/// and add any missing managed keys to its `.contents` sub-table:
/// - one `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME = "<platform>"`
///   line per declared store across ALL kinds (KV / CONFIG / SECRETS);
/// - one COMMENTED `# EDGEZERO__STORES__CONFIG__<LOGICAL_UPPER>__KEY =
///   "<logical>_staging"` placeholder per CONFIG store, mirroring the
///   Cloudflare `.dev.vars` discipline. Fastly has no way to preview
///   the KEY overlay at provision time — commented placeholders hint
///   the shape and let the operator uncomment + fill it in.
///
/// **Additive merge** (spec §"Merge mechanics"): on re-provision after
/// adding a store, the block already exists — we open its `.contents`
/// table and insert only the managed keys that aren't present.
/// Operator-set values and non-managed keys are left byte-for-byte.
/// The commented `__KEY` placeholder decor is only emitted on the
/// first-write path (when the block is newly created); on re-provision
/// we don't try to rewrite decor on existing keys, which would risk
/// clobbering operator edits — operators who need new __KEY hints can
/// re-run provision on an empty block or copy the shape by hand.
///
/// Returns `true` when the block was newly written OR at least one
/// key was added; `false` when nothing changed.
fn upsert_runtime_env_config_store(
    doc: &mut toml_edit::DocumentMut,
    stores: &ProvisionStores<'_>,
) -> Result<bool, String> {
    use toml_edit::{value, Item, Table};

    const RUNTIME_ENV_NAME: &str = "edgezero_runtime_env";

    let local_server_entry = doc
        .entry("local_server")
        .or_insert_with(|| Item::Table(Table::new()));
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        "`local_server` exists but is not a table; refusing to edit in place".to_owned()
    })?;
    let config_stores_entry = local_server_tbl
        .entry("config_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let config_stores_tbl = config_stores_entry.as_table_mut().ok_or_else(|| {
        "`local_server.config_stores` exists but is not a table; refusing to edit in place"
            .to_owned()
    })?;

    // Compute the full managed __NAME key set once — used both for
    // first-write insertion and for additive-merge gap-fill.
    let managed_keys: Vec<(String, String)> = [
        ("KV", stores.kv),
        ("CONFIG", stores.config),
        ("SECRETS", stores.secrets),
    ]
    .into_iter()
    .flat_map(|(kind_label, kind_stores)| {
        kind_stores.iter().map(move |store| {
            (
                format!(
                    "EDGEZERO__STORES__{kind_label}__{}__NAME",
                    store.logical.to_ascii_uppercase()
                ),
                store.platform.clone(),
            )
        })
    })
    .collect();

    let block_existed = config_stores_tbl.contains_key(RUNTIME_ENV_NAME);
    if block_existed {
        // Additive merge path. Open the existing block's `.contents`
        // sub-table and insert only the managed keys that aren't
        // already there. Skip the commented __KEY decor rewrite —
        // operator may have uncommented or removed those on purpose.
        let store_entry = config_stores_tbl.get_mut(RUNTIME_ENV_NAME).ok_or_else(|| {
            format!(
                "`local_server.config_stores.{RUNTIME_ENV_NAME}` disappeared between contains_key and get_mut"
            )
        })?;
        let store_tbl = store_entry.as_table_mut().ok_or_else(|| {
            format!(
                "`local_server.config_stores.{RUNTIME_ENV_NAME}` exists but is not a table; refusing to edit in place"
            )
        })?;
        let contents_entry = store_tbl
            .entry("contents")
            .or_insert_with(|| Item::Table(Table::new()));
        let contents_tbl = contents_entry.as_table_mut().ok_or_else(|| {
            format!(
                "`local_server.config_stores.{RUNTIME_ENV_NAME}.contents` exists but is not a table; refusing to edit in place"
            )
        })?;
        let mut added = false;
        for (key, platform) in &managed_keys {
            if !contents_tbl.contains_key(key) {
                contents_tbl.insert(key, value(platform.as_str()));
                added = true;
            }
        }
        return Ok(added);
    }

    // First-write path — build the whole block, including the
    // commented __KEY placeholder decor.
    let mut store_tbl = Table::new();
    store_tbl.set_implicit(false);
    store_tbl.insert("format", value("inline-toml"));

    let mut contents_tbl = Table::new();
    contents_tbl.set_implicit(false);
    for (key, platform) in &managed_keys {
        contents_tbl.insert(key, value(platform.as_str()));
    }

    // Commented `__KEY` placeholders for CONFIG stores. Toml_edit
    // has no primitive for "commented key inside a table", so we
    // stash the comment lines as a suffix on the last-inserted
    // key/value's decor. The test asserts only presence-as-substring
    // in the raw file text, so location within the block doesn't
    // matter — but appending at the end keeps the __NAME contract
    // uncontaminated (a re-parse still yields only real keys).
    let comment_suffix: String = stores
        .config
        .iter()
        .map(|store| {
            let upper = store.logical.to_ascii_uppercase();
            let logical = store.logical.as_str();
            format!("\n# EDGEZERO__STORES__CONFIG__{upper}__KEY = \"{logical}_staging\"")
        })
        .collect::<Vec<_>>()
        .concat();
    if !comment_suffix.is_empty() {
        let last_key = contents_tbl.iter().last().map(|(key, _)| key.to_owned());
        if let Some(last) = last_key {
            if let Some(item) = contents_tbl.get_mut(&last) {
                if let Some(val) = item.as_value_mut() {
                    val.decor_mut().set_suffix(comment_suffix);
                }
            }
        } else {
            // Edge case: no declared stores at all (contents_tbl is
            // empty). Attach the comments via the contents table's
            // own decor so they survive serialisation.
            contents_tbl.decor_mut().set_suffix(comment_suffix);
        }
    }

    store_tbl.insert("contents", Item::Table(contents_tbl));
    config_stores_tbl.insert(RUNTIME_ENV_NAME, Item::Table(store_tbl));
    Ok(true)
}

/// Append one `[[local_server.secret_stores.<store_id>]]` entry with
/// `key = "<key_value>"` and `env = "<KEY_VALUE_UPPER>"` — Fastly's
/// secret-store convention pairs the key name with the env var the
/// local runtime exposes it under. Idempotent: if the target array
/// already contains an entry with matching `key = "<key_value>"` we
/// skip and leave sibling entries (including operator-adjusted `env`
/// values) alone. Returns `Ok(true)` when a new entry was appended,
/// `Ok(false)` when a matching key was already present.
///
/// Refuses to clobber non-standard values: if the target
/// `secret_stores.<store_id>` node exists but isn't an array of
/// tables, or if `local_server` / `local_server.secret_stores`
/// exist but aren't tables, the helper errors with a "refusing to
/// edit in place" diagnostic.
fn upsert_secret_store_entry(
    doc: &mut toml_edit::DocumentMut,
    store_id: &str,
    key_value: &str,
) -> Result<bool, String> {
    use toml_edit::{value, ArrayOfTables, Item, Table};

    let local_server_entry = doc
        .entry("local_server")
        .or_insert_with(|| Item::Table(Table::new()));
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        "`local_server` exists but is not a table; refusing to edit in place".to_owned()
    })?;
    let secret_stores_entry = local_server_tbl
        .entry("secret_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let secret_stores_tbl = secret_stores_entry.as_table_mut().ok_or_else(|| {
        "`local_server.secret_stores` exists but is not a table; refusing to edit in place"
            .to_owned()
    })?;
    let store_entry = secret_stores_tbl
        .entry(store_id)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let store_arr = store_entry.as_array_of_tables_mut().ok_or_else(|| {
        format!(
            "`local_server.secret_stores.{store_id}` exists but is not an array of tables; refusing to edit in place"
        )
    })?;
    for existing in store_arr.iter() {
        if existing.get("key").and_then(|item| item.as_str()) == Some(key_value) {
            return Ok(false);
        }
    }
    let mut row = Table::new();
    row.insert("key", value(key_value));
    row.insert("env", value(key_value.to_ascii_uppercase()));
    store_arr.push(row);
    Ok(true)
}

/// Write the local-server config-store entries to `fastly.toml`:
/// `[local_server.config_stores.<platform_name>]` becomes
/// `format = "inline-toml"`, and `[local_server.config_stores.<platform_name>.contents]`
/// gets the flat `key = "value"` pairs (overwriting any previous
/// values). Idempotent — re-running just rewrites `contents`. Other
/// blocks in `fastly.toml` (setup, scripts, the actual `[local_server]`
/// secret stores, etc.) are preserved via `toml_edit`.
pub(super) fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    use std::io::ErrorKind;
    use toml_edit::{table, DocumentMut, Item, Table, Value};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let local_server_entry = doc.entry("local_server").or_insert_with(table);
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    let config_stores_entry = local_server_tbl
        .entry("config_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let config_stores_tbl = config_stores_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;

    // Upsert into the existing per-store contents table so a
    // `config push --key app_config_staging` does NOT wipe the
    // previously-pushed `app_config` blob. Spec 12.7 requires
    // default + staging keys to coexist so the runtime
    // EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY env var can
    // switch between them. (Earlier wholesale-replace was a
    // misread of the "stale entries don't linger" property:
    // that applies WITHIN a key (old chunks for the same root
    // become unreferenced when a new chunk-set installs a new
    // pointer), NOT across sibling keys.)
    let store_entry = config_stores_tbl.entry(platform_name).or_insert_with(|| {
        let mut tbl = Table::new();
        tbl.insert("format", toml_edit::value("inline-toml"));
        tbl.insert("contents", Item::Table(Table::new()));
        Item::Table(tbl)
    });
    let store_tbl = store_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores.{platform_name}` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    // Ensure the `format` key is present even on a pre-existing
    // entry that omitted it.
    if !store_tbl.contains_key("format") {
        store_tbl.insert("format", toml_edit::value("inline-toml"));
    }
    let contents_entry = store_tbl
        .entry("contents")
        .or_insert_with(|| Item::Table(Table::new()));
    let contents_tbl = contents_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores.{platform_name}.contents` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    for (key, value) in entries {
        contents_tbl.insert(key, Item::Value(Value::from(value.clone())));
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::path_mutation_guard;
    use super::super::run::synthesise_fastly_toml;
    use super::super::FastlyCliAdapter;
    use super::*;
    use edgezero_adapter::registry::{
        Adapter as _, ProvisionMode, ResolvedStoreId, TypedSecretEntry,
    };
    #[cfg(unix)]
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsString;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` per call site) keeps the
    // setup-vs-assertion pair in sync -- a typo in one place no
    // longer silently divorces from the other, because both reference
    // the same const. Also names the intent: these are the LOGICAL
    // store ids the fastly adapter operates on, not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_CONFIG_ID: &str = "app_config";

    /// RAII guard: prepends a directory to `$PATH` and restores the original
    /// value on drop.
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

    /// A shell script named `fastly` that exits non-zero and prints an
    /// unambiguous diagnostic to stderr — installed on `$PATH` to
    /// detect any (forbidden) invocation of the platform CLI during a
    /// Local-mode provision. Any call fails the test with `exit 42`.
    #[cfg(unix)]
    fn fake_fastly_panicking() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("fastly");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho 'fastly was called during local provision' >&2\nexit 42\n",
        )
        .expect("write fake fastly");
        let mut perms = fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod +x");
        dir
    }

    // ---------- provision (local mode) ----------

    #[test]
    fn synthesised_fastly_toml_honors_renamed_adapter_crate() {
        use std::path::PathBuf;

        // Reviewer regression: with
        // `[adapters.fastly.adapter].manifest = "crates/fast-edge/svc/fastly.toml"`
        // + `[package].name = "fast-edge"`, clean-clone provision
        // must emit `name = "fast-edge"` — NOT the fallback
        // `demo-app-adapter-fastly`. Also covers the nested
        // manifest shape (`crates/fast-edge/svc/fastly.toml`).
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        let crate_dir = root.join("crates/fast-edge");
        fs::create_dir_all(crate_dir.join("svc")).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"fast-edge\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let outcome = FastlyCliAdapter
            .synthesise_baseline_manifest(
                root,
                Some("crates/fast-edge/svc/fastly.toml"),
                None,
                "demo-app",
                None,
            )
            .expect("baseline synthesis succeeds for nested renamed crate");
        let (rel, body) = outcome.into_iter().next().unwrap();
        assert_eq!(rel, PathBuf::from("crates/fast-edge/svc/fastly.toml"));
        assert!(
            body.contains(r#"name = "fast-edge""#),
            "fastly.toml must name the renamed adapter crate (fast-edge): {body}"
        );
        assert!(
            !body.contains(r#"name = "demo-app-adapter-fastly""#),
            "MUST NOT fall back to scaffold convention when the Cargo.toml exists further up: {body}"
        );
    }

    /// Local provision writes `[[local_server.kv_stores.<platform>]]`
    /// and `[local_server.config_stores.<platform>]` blocks. The
    /// config-store block's `contents` MUST be a TOML table (not
    /// `contents = ""`), because the Fastly `config push --local`
    /// writer edits it in place via `as_table_mut()`.
    #[test]
    fn fastly_local_provision_writes_kv_and_config_store_blocks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        // KV: array-of-tables with the stub row.
        assert!(
            after.contains("[[local_server.kv_stores.sessions]]"),
            "kv block present: {after}"
        );
        // Reparse-and-index instead of `.contains("key = \"__init__\"")`
        // + `.contains("data = \"\"")`: those substrings would pass for
        // BOTH the correct nested-row form AND the scenario where the
        // stub keys land at the doc root (same class as the shipped
        // service_id bug). Lock the row on the actual
        // `[[local_server.kv_stores.sessions]]` block.
        let after_doc: toml_edit::DocumentMut = after.parse().expect("re-parse merged fastly.toml");
        let kv_row = after_doc
            .get("local_server")
            .and_then(|item| item.get("kv_stores"))
            .and_then(|item| item.get("sessions"))
            .and_then(toml_edit::Item::as_array_of_tables)
            .and_then(|arr| arr.get(0))
            .expect("[[local_server.kv_stores.sessions]] with at least one row");
        assert_eq!(
            kv_row.get("key").and_then(toml_edit::Item::as_str),
            Some("__init__"),
            "kv stub `key = \"__init__\"` must sit inside [[local_server.kv_stores.sessions]]: {after}"
        );
        assert_eq!(
            kv_row.get("data").and_then(toml_edit::Item::as_str),
            Some(""),
            "kv stub `data = \"\"` must sit inside [[local_server.kv_stores.sessions]]: {after}"
        );
        // CONFIG: table block plus empty contents SUB-TABLE (not
        // `contents = ""`). Re-parse to confirm shape.
        assert!(
            after.contains("[local_server.config_stores.app_config]"),
            "config-store block header present: {after}"
        );
        assert!(
            after.contains(r#"format = "inline-toml""#),
            "config-store format key present: {after}"
        );
        assert!(
            after.contains("[local_server.config_stores.app_config.contents]"),
            "config-store contents sub-table header present: {after}"
        );
        assert!(
            !after.contains(r#"contents = """#),
            "contents MUST NOT be an empty string: {after}"
        );
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        assert!(
            doc["local_server"]["config_stores"]["app_config"]["contents"]
                .as_table()
                .is_some(),
            "contents parses as a table (required by config push --local)"
        );
    }

    /// Local provision writes the `edgezero_runtime_env` runtime-
    /// override block: `__NAME` lines for ALL declared kinds and
    /// commented `__KEY` placeholders for CONFIG stores only.
    #[test]
    fn fastly_local_provision_writes_edgezero_runtime_env() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("[local_server.config_stores.edgezero_runtime_env]"),
            "runtime-env block header present: {after}"
        );
        assert!(
            after.contains("[local_server.config_stores.edgezero_runtime_env.contents]"),
            "runtime-env contents sub-table header present: {after}"
        );
        assert!(
            after.contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME = "app_config""#),
            "CONFIG __NAME line: {after}"
        );
        assert!(
            after.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME = "sessions""#),
            "KV __NAME line: {after}"
        );
        assert!(
            after.contains(r#"# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY = "app_config_staging""#),
            "commented CONFIG __KEY placeholder present: {after}"
        );
    }

    /// Regression: re-provision after adding a store MUST add the new
    /// store's `__NAME` line into the existing `edgezero_runtime_env`
    /// block's `.contents` sub-table. Prior impl short-circuited
    /// `Ok(false)` as soon as the block existed, leaving new stores
    /// invisible to the local runtime. Violates spec §"Merge
    /// mechanics" — "preserve operator-set values; only add what's
    /// missing".
    #[test]
    fn fastly_local_provision_additively_merges_new_stores_into_existing_runtime_env() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");

        // First provision: only the KV store is declared.
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ProvisionStores {
                    config: &[],
                    kv: &kv_ids,
                    secrets: &[],
                },
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("first provision succeeds");

        let after_first = fs::read_to_string(&path).expect("read");
        assert!(
            after_first.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME = "sessions""#),
            "first provision wrote the KV __NAME line: {after_first}"
        );
        assert!(
            !after_first.contains("APP_CONFIG__NAME"),
            "first provision must NOT emit a CONFIG line for a store that wasn't declared: {after_first}"
        );

        // Second provision: operator added a CONFIG store (and the
        // block from run 1 already exists). The new store's __NAME
        // line MUST land inside the existing runtime-env contents
        // sub-table.
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ProvisionStores {
                    config: &config_ids,
                    kv: &kv_ids,
                    secrets: &[],
                },
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("second provision succeeds");

        let after_second = fs::read_to_string(&path).expect("read");
        // Additive: original KV line preserved.
        assert!(
            after_second.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME = "sessions""#),
            "second provision must preserve the KV __NAME line: {after_second}"
        );
        // Additive: new CONFIG line inserted into the existing block.
        assert!(
            after_second
                .contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME = "app_config""#),
            "second provision must ADD the new CONFIG __NAME line into the existing runtime-env block: {after_second}"
        );
        // No duplicate runtime-env block header.
        let block_header = "[local_server.config_stores.edgezero_runtime_env]";
        assert_eq!(
            after_second.matches(block_header).count(),
            1,
            "runtime-env block header must appear exactly once (no duplicate block emitted): {after_second}"
        );
    }

    /// A missing `fastly.toml` is a bug in the Task 8b bootstrap path.
    /// Provision must error CLEARLY -- naming the expected path --
    /// rather than silently re-synthesising (we don't have the app
    /// name in scope here).
    #[test]
    fn fastly_local_provision_errors_if_manifest_absent() {
        let dir = tempdir().expect("tempdir");
        // Do NOT pre-seed fastly.toml.
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let err = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing manifest must error");
        assert!(
            err.contains("fastly.toml"),
            "error names the missing path: {err}"
        );
        assert!(
            err.contains(&dir.path().join("fastly.toml").display().to_string()),
            "error contains the resolved absolute path: {err}"
        );
    }

    /// Spec §"Fastly": the deployed `service_id` must be upserted
    /// during BOTH synthesis AND merge. Task 21 handles synthesis
    /// (first-run bootstrap); THIS lock covers the merge case where
    /// the operator pre-seeded fastly.toml from a stale template
    /// before a deploy happened.
    #[test]
    fn fastly_local_provision_upserts_deployed_service_id_into_existing_manifest() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Pre-seed WITHOUT service_id.
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        assert!(
            !fs::read_to_string(&path)
                .expect("read")
                .contains("service_id"),
            "baseline has no service_id"
        );
        let mut deployed = AdapterDeployedState::default();
        deployed
            .fields
            .insert("service_id".to_owned(), "SVC1".to_owned());
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                Some(&deployed),
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains(r#"service_id = "SVC1""#),
            "deployed service_id pinned into merged manifest: {after}"
        );
        // Regression: `toml_edit::DocumentMut::insert` on a doc that
        // already parsed `[local_server]` was appending `service_id`
        // AFTER the header, so re-parse read it as
        // `local_server.service_id` — a silent divergence that
        // `.contains("service_id = \"SVC1\"")` never caught. Parse the
        // re-emitted file and assert the key lives at the ROOT.
        let reparsed: toml_edit::DocumentMut =
            after.parse().expect("re-parse must succeed after upsert");
        assert_eq!(
            reparsed.get("service_id").and_then(toml_edit::Item::as_str),
            Some("SVC1"),
            "service_id must live at the TOML root (not as local_server.service_id): {after}"
        );
    }

    /// Inverse of the previous lock: when there's no cloud authority
    /// (deployed = None), operator's local value wins. Provision must
    /// NOT overwrite a `service_id` the operator set themselves.
    #[test]
    fn fastly_local_provision_leaves_operator_service_id_alone_when_deployed_absent() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", Some("operator-set"))).expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains(r#"service_id = "operator-set""#),
            "operator-set service_id survives when deployed absent: {after}"
        );
    }

    /// `adapter_manifest_path` may be a NESTED relative path (e.g.
    /// `crates/fastly/fastly.toml`). Provision must land its writes
    /// in the nested file, NOT at a sibling under `manifest_root`.
    #[test]
    fn fastly_local_provision_resolves_nested_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let nested_rel = "crates/fastly/fastly.toml";
        let nested_path = dir.path().join(nested_rel);
        fs::create_dir_all(nested_path.parent().expect("parent")).expect("mkdir");
        fs::write(&nested_path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some(nested_rel),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        let after = fs::read_to_string(&nested_path).expect("read nested");
        assert!(
            after.contains("[[local_server.kv_stores.sessions]]"),
            "merge lands in nested manifest: {after}"
        );
        // And no sibling was created at manifest_root level.
        let sibling = dir.path().join("fastly.toml");
        assert!(
            !sibling.exists(),
            "no sibling fastly.toml created at manifest_root"
        );
    }

    /// Idempotency lock: running local provision twice on the same
    /// fixture must leave the manifest bit-for-bit unchanged (mod the
    /// first-run mutation).
    #[test]
    fn fastly_local_provision_is_idempotent_on_second_run() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("first run succeeds");
        let after_first = fs::read_to_string(&path).expect("read after first");
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("second run succeeds");
        let after_second = fs::read_to_string(&path).expect("read after second");
        assert_eq!(
            after_first, after_second,
            "second provision is a no-op -- fastly.toml must be bit-for-bit unchanged"
        );
    }

    // ---------- provision_typed (secret stores) ----------

    /// Local `provision_typed` appends
    /// `[[local_server.secret_stores.<store_id>]]` entries with
    /// `key = "<key_value>"` and `env = "<KEY_VALUE_UPPER>"` per
    /// `TypedSecretEntry`, grouped by the entry's `store_id`.
    #[test]
    fn fastly_provision_typed_writes_secret_store_entries_under_resolved_store_id() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let entries = [
            TypedSecretEntry::new("default", "api_token", "demo_api_token"),
            TypedSecretEntry::new("vendor_secrets", "vendor_key", "vendor_demo_key"),
        ];
        FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("[[local_server.secret_stores.default]]"),
            "default store array-of-tables header present: {after}"
        );
        assert!(
            after.contains(r#"key = "demo_api_token""#),
            "default store key line present: {after}"
        );
        assert!(
            after.contains(r#"env = "DEMO_API_TOKEN""#),
            "default store env line uppercased: {after}"
        );
        assert!(
            after.contains("[[local_server.secret_stores.vendor_secrets]]"),
            "vendor_secrets store array-of-tables header present: {after}"
        );
        assert!(
            after.contains(r#"key = "vendor_demo_key""#),
            "vendor_secrets store key line present: {after}"
        );
        assert!(
            after.contains(r#"env = "VENDOR_DEMO_KEY""#),
            "vendor_secrets store env line uppercased: {after}"
        );
        // Confirm shape via re-parse: the per-store slot MUST be an
        // ArrayOfTables (not a plain table) — Viceroy expects the
        // array-of-tables form for secret-store entries.
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        assert!(
            doc["local_server"]["secret_stores"]["default"]
                .as_array_of_tables()
                .is_some(),
            "default is array-of-tables"
        );
        assert!(
            doc["local_server"]["secret_stores"]["vendor_secrets"]
                .as_array_of_tables()
                .is_some(),
            "vendor_secrets is array-of-tables"
        );
    }

    /// `adapter_manifest_path` may be a NESTED relative path. Entries
    /// land in the nested `fastly.toml`, not at a sibling under
    /// `manifest_root`.
    #[test]
    fn fastly_provision_typed_lands_in_resolved_fastly_toml_not_manifest_root() {
        let dir = tempdir().expect("tempdir");
        let nested_rel = "crates/fastly/fastly.toml";
        let nested_path = dir.path().join(nested_rel);
        fs::create_dir_all(nested_path.parent().expect("parent")).expect("mkdir");
        fs::write(&nested_path, synthesise_fastly_toml("demo", None)).expect("write");
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some(nested_rel),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let after = fs::read_to_string(&nested_path).expect("read nested");
        assert!(
            after.contains("[[local_server.secret_stores.default]]"),
            "entries land in nested manifest: {after}"
        );
        assert!(
            after.contains(r#"key = "demo_api_token""#),
            "key line in nested manifest: {after}"
        );
        let sibling = dir.path().join("fastly.toml");
        assert!(
            !sibling.exists(),
            "no sibling fastly.toml created at manifest_root"
        );
    }

    /// Idempotency: a matching `key = "<key_value>"` entry already in
    /// the target array is preserved (including operator's non-matching
    /// `env` override). No duplicate row is appended.
    #[test]
    fn fastly_provision_typed_deduplicates_matching_key() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let mut seed = synthesise_fastly_toml("demo", None);
        seed.push_str(
            "\n[[local_server.secret_stores.default]]\nkey = \"demo_api_token\"\nenv = \"CUSTOM_ENV\"\n",
        );
        fs::write(&path, &seed).expect("write");
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let after = fs::read_to_string(&path).expect("read");
        // Operator's env override is preserved (not overwritten to the
        // default `DEMO_API_TOKEN`).
        assert!(
            after.contains(r#"env = "CUSTOM_ENV""#),
            "operator's env override preserved: {after}"
        );
        assert!(
            !after.contains(r#"env = "DEMO_API_TOKEN""#),
            "adapter did NOT overwrite operator env: {after}"
        );
        // Exactly one entry for the store with key = "demo_api_token".
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        let arr = doc["local_server"]["secret_stores"]["default"]
            .as_array_of_tables()
            .expect("default is array-of-tables");
        let matches: usize = arr
            .iter()
            .filter(|tbl| tbl.get("key").and_then(|item| item.as_str()) == Some("demo_api_token"))
            .count();
        assert_eq!(matches, 1, "exactly one matching key entry: {after}");
    }

    /// Absent `fastly.toml` is a Task 8b bootstrap bug — error clearly
    /// with the resolved absolute path, matching the Task 22
    /// `provision_local` error style so both flows fail the same way.
    #[test]
    fn fastly_provision_typed_errors_if_manifest_absent() {
        let dir = tempdir().expect("tempdir");
        // Do NOT pre-seed fastly.toml.
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        let err = FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect_err("missing manifest must error");
        assert!(
            err.contains("fastly.toml"),
            "error names the missing path: {err}"
        );
        assert!(
            err.contains(&dir.path().join("fastly.toml").display().to_string()),
            "error contains the resolved absolute path: {err}"
        );
    }

    // ---------- Section 9: provision_local_* contract suite ----------
    //
    // Cross-adapter contract for `provision(mode=Local)`. Mirrors the
    // Cloudflare/Spin/Axum suites so the four adapters share a single
    // observable specification: the first run writes an expected set
    // of files, re-provision is byte-identical, operator hand-edits to
    // sibling entries survive a subsequent write, and Local mode never
    // shells out to the platform CLI.
    //
    // Test #5 (additive merge of a new store into the existing
    // `edgezero_runtime_env` block) is already covered by
    // `fastly_local_provision_additively_merges_new_stores_into_existing_runtime_env`
    // above — not re-implemented here to avoid duplicate coverage.

    /// Section 9.1 — First run: empty fixture with one KV and one
    /// CONFIG store yields a `fastly.toml` with the edgezero-provision
    /// header, per-kind `[local_server.*_stores.*]` blocks in their
    /// expected shape, and an `edgezero_runtime_env.contents`
    /// sub-table populated with `__NAME` lines for every declared
    /// store. `contents` MUST remain a TABLE (spec regression guard).
    #[test]
    fn provision_local_first_run_writes_expected_files() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds");
        assert!(
            path.exists(),
            "fastly.toml exists after first-run provision"
        );
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.starts_with("# edgezero-provision: v1"),
            "manifest starts with edgezero-provision header: {after}"
        );
        // KV: array-of-tables with the stub row. Reparse-and-index --
        // see the sibling test's rationale (bare `.contains(...)`
        // passes for both correct-nested and shipped root-drift bug).
        let after_doc: toml_edit::DocumentMut =
            after.parse().expect("re-parse first-run fastly.toml");
        let kv_row = after_doc
            .get("local_server")
            .and_then(|item| item.get("kv_stores"))
            .and_then(|item| item.get("sessions"))
            .and_then(toml_edit::Item::as_array_of_tables)
            .and_then(|arr| arr.get(0))
            .expect("[[local_server.kv_stores.sessions]] with at least one row");
        assert_eq!(
            kv_row.get("key").and_then(toml_edit::Item::as_str),
            Some("__init__"),
            "kv stub `key` inside the sessions block: {after}"
        );
        assert_eq!(
            kv_row.get("data").and_then(toml_edit::Item::as_str),
            Some(""),
            "kv stub `data` inside the sessions block: {after}"
        );
        // CONFIG: table block with `format = "inline-toml"` plus an
        // empty `contents` SUB-TABLE (never `contents = ""`).
        assert!(
            after.contains("[local_server.config_stores.app_config]"),
            "config-store block header present: {after}"
        );
        assert!(
            after.contains(r#"format = "inline-toml""#),
            "config-store format key present: {after}"
        );
        assert!(
            after.contains("[local_server.config_stores.app_config.contents]"),
            "config-store contents sub-table header present: {after}"
        );
        assert!(
            !after.contains(r#"contents = """#),
            "contents MUST NOT be an empty string (spec regression guard): {after}"
        );
        // Runtime-env: __NAME line for every declared store.
        assert!(
            after.contains("[local_server.config_stores.edgezero_runtime_env.contents]"),
            "runtime-env contents sub-table header present: {after}"
        );
        assert!(
            after.contains(r#"EDGEZERO__STORES__KV__SESSIONS__NAME = "sessions""#),
            "KV __NAME line: {after}"
        );
        assert!(
            after.contains(r#"EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME = "app_config""#),
            "CONFIG __NAME line: {after}"
        );
        // Re-parse to confirm both `contents` slots are tables (the
        // shape Viceroy + `config push --local` expect).
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        assert!(
            doc["local_server"]["config_stores"]["app_config"]["contents"]
                .as_table()
                .is_some(),
            "app_config.contents parses as a table"
        );
        assert!(
            doc["local_server"]["config_stores"]["edgezero_runtime_env"]["contents"]
                .as_table()
                .is_some(),
            "edgezero_runtime_env.contents parses as a table"
        );
    }

    /// Section 9.2 — Re-provision must be byte-identical. This is the
    /// operator's contract that `provision --adapter fastly` is safe
    /// to re-run: no drift in whitespace, no reordering, no re-emit of
    /// entries that were already present.
    #[test]
    fn provision_local_re_provision_is_byte_identical() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("first provision succeeds");
        let after_first = fs::read_to_string(&path).expect("read after first");
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("second provision succeeds");
        let after_second = fs::read_to_string(&path).expect("read after second");
        assert_eq!(
            after_first, after_second,
            "second provision is byte-identical to the first"
        );
    }

    /// Section 9.3 — Fastly-specific: after the base `fastly.toml` is
    /// provisioned and the operator hand-edits a
    /// `[[local_server.secret_stores.default]]` entry with a custom
    /// `env` mapping, a subsequent `provision_typed` call that adds a
    /// DIFFERENT key must land the new entry as a sibling in the same
    /// array-of-tables — WITHOUT rewriting the operator's `env`
    /// mapping on the pre-existing row (idempotent-append semantics).
    ///
    /// Fastly is the only adapter where the operator maps a secret
    /// store `key` to an OS env var via the `env` field; the writer
    /// MUST NOT clobber that mapping when appending new keys.
    ///
    /// Renamed 2026-07 (deep self-review finding P1-f): the prior
    /// name `provision_local_push_after_provision_preserves_*`
    /// promised a push→provision integration test but the body only
    /// re-runs `provision_typed` twice; the invariant is
    /// re-provision idempotency, not push semantics.
    #[test]
    fn provision_typed_local_re_run_preserves_operator_env_mapping_on_secret_store_entry() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Base manifest + the operator's hand-edited entry. The
        // operator maps their secret's local `key = "custom_key"` to
        // the real-world OS env var `REAL_ENV_MAPPING` — an override
        // that must survive future writer runs.
        let mut seed = synthesise_fastly_toml("demo", None);
        seed.push_str(
            "\n[[local_server.secret_stores.default]]\nkey = \"custom_key\"\nenv = \"REAL_ENV_MAPPING\"\n",
        );
        fs::write(&path, &seed).expect("write");
        // A new secret arrives — a DIFFERENT key under the same store.
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "different_key",
        )];
        FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let after = fs::read_to_string(&path).expect("read");
        // Operator's exact env mapping survives byte-for-byte.
        assert!(
            after.contains(r#"env = "REAL_ENV_MAPPING""#),
            "operator's `env = \"REAL_ENV_MAPPING\"` mapping preserved verbatim: {after}"
        );
        assert!(
            after.contains(r#"key = "custom_key""#),
            "operator's original key row still present: {after}"
        );
        // The new entry lands as a sibling row with the default
        // key→env uppercasing.
        assert!(
            after.contains(r#"key = "different_key""#),
            "new key row appended: {after}"
        );
        assert!(
            after.contains(r#"env = "DIFFERENT_KEY""#),
            "new entry defaults to `env = \"<KEY_UPPER>\"`: {after}"
        );
        // Re-parse: the array-of-tables now holds both rows, with the
        // operator's row untouched.
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        let arr = doc["local_server"]["secret_stores"]["default"]
            .as_array_of_tables()
            .expect("default is array-of-tables");
        assert_eq!(arr.len(), 2, "two sibling entries after append: {after}");
        let custom = arr
            .iter()
            .find(|tbl| tbl.get("key").and_then(|item| item.as_str()) == Some("custom_key"))
            .expect("custom_key row present");
        assert_eq!(
            custom.get("env").and_then(|item| item.as_str()),
            Some("REAL_ENV_MAPPING"),
            "custom_key row's env mapping locked to the operator's value"
        );
        let different = arr
            .iter()
            .find(|tbl| tbl.get("key").and_then(|item| item.as_str()) == Some("different_key"))
            .expect("different_key row present");
        assert_eq!(
            different.get("env").and_then(|item| item.as_str()),
            Some("DIFFERENT_KEY"),
            "different_key row's env defaults to KEY_UPPER"
        );
    }

    /// Section 9.4 — Zero cloud calls. Local-mode provision is a pure
    /// file writer; it must NEVER shell out to `fastly`. Install
    /// `fake_fastly_panicking()` (a script that exits 42 on any call)
    /// on `$PATH` before invoking provision. If provision ever calls
    /// the platform CLI, the fake short-circuits and the invocation
    /// bubbles up as an error — so `Ok(...)` is the load-bearing
    /// signal that no cloud call happened.
    #[cfg(unix)]
    #[test]
    fn provision_local_zero_cloud_calls() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let fake = fake_fastly_panicking();
        let _path = PathPrepend::new(fake.path());
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, synthesise_fastly_toml("demo", None)).expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .expect("local provision succeeds with a panicking fake fastly on PATH");
    }

    // ---------- write_fastly_local_config_store (config push --local) ----------
    //
    // The writer is exported from provision_local.rs (per the split
    // brief: local-server config-store writes are provision_local's
    // territory). These four tests exercise the writer directly.

    #[test]
    fn write_fastly_local_config_store_creates_inline_block_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        write_fastly_local_config_store(&path, TEST_CONFIG_ID, &entries).expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains(&format!("[local_server.config_stores.{TEST_CONFIG_ID}]")),
            "store table: {after}"
        );
        assert!(
            after.contains("format = \"inline-toml\""),
            "format field: {after}"
        );
        assert!(
            after.contains(&format!(
                "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
            )),
            "contents table: {after}"
        );
        assert!(after.contains("greeting = \"hello\""), "key 1: {after}");
        assert!(
            after.contains("\"service.timeout_ms\" = \"1500\""),
            "dotted key quoted: {after}"
        );
        assert!(after.contains("name = \"demo\""), "preserved: {after}");
    }

    #[test]
    fn write_fastly_local_config_store_replaces_existing_block_on_re_push() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "stale".to_owned())],
        )
        .expect("first write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "fresh".to_owned())],
        )
        .expect("second write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("greeting = \"fresh\""), "new value: {after}");
        assert!(
            !after.contains("greeting = \"stale\""),
            "stale value dropped: {after}"
        );
    }

    #[test]
    fn write_fastly_local_config_store_preserves_unrelated_blocks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let original = "\
[setup.kv_stores.sessions]

[[local_server.kv_stores.sessions]]
key = \"__init__\"
data = \"\"

[scripts]
build = \"cargo build --release\"
";
        fs::write(&path, original).expect("write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
        )
        .expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "setup KV kept: {after}"
        );
        assert!(after.contains("[scripts]"), "scripts table kept: {after}");
        assert!(
            after.contains("build = \"cargo build --release\""),
            "scripts value kept: {after}"
        );
        assert!(
            after.contains(&format!(
                "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
            )),
            "new config_stores block added: {after}"
        );
    }

    #[test]
    fn write_fastly_local_config_store_creates_file_when_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // No fs::write — file absent.
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
        )
        .expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains(&format!(
            "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
        )));
        assert!(after.contains("greeting = \"hi\""));
    }
}
