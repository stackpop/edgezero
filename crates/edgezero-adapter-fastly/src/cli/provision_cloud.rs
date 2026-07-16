use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use edgezero_adapter::registry::{AdapterDeployedState, ProvisionOutcome, ProvisionStores};

use super::FASTLY_INSTALL_HINT;

/// Cloud-mode `provision`: create Fastly platform stores via
/// `fastly <kind>-store create`, then write the corresponding
/// `[setup.<kind>_stores.<id>]` block to `fastly.toml`. Also
/// creates the `edgezero_runtime_env` config-store the runtime
/// override path depends on.
///
/// Callers in `mod.rs` gate this on `ProvisionMode::Cloud`; Local
/// mode dispatches to `provision_local::provision`.
pub(super) fn provision(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    // Fastly is Multi for every store kind. Each id maps 1:1
    // to a Fastly resource (kv-store / config-store /
    // secret-store) created via the Fastly CLI; the manifest
    // writeback declares the resource link for `fastly
    // compute deploy` and the local viceroy server.
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.fastly.adapter].manifest must point at fastly.toml for provision".to_owned(),
        );
    };
    let fastly_path = manifest_root.join(rel);

    let mut out = Vec::new();
    for (kind, ids) in [
        ("kv", stores.kv),
        ("config", stores.config),
        ("secret", stores.secrets),
    ] {
        for store in ids {
            // Fastly setup tables key on the resource name the
            // CLI creates. The runtime resolves that same name
            // via `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`,
            // so provision must use the env-resolved PLATFORM
            // name -- the logical id stays in status lines for
            // human-facing wording.
            let logical = store.logical.as_str();
            let name = store.platform.as_str();
            if dry_run {
                out.push(format!(
                    "would run `fastly {kind}-store create --name={name}` and append [setup.{kind}_stores.{name}] to {} (logical id `{logical}`)",
                    fastly_path.display()
                ));
                continue;
            }
            if setup_block_present(&fastly_path, kind, name)? {
                out.push(format!(
                    "fastly {kind}-store `{name}` (logical id `{logical}`) already declared in {}; skipping. To force a fresh remote: delete the [setup.{kind}_stores.{name}] block AND run `fastly {kind}-store delete --name={name}` (the old remote store lingers otherwise), then re-run provision.",
                    fastly_path.display()
                ));
                continue;
            }
            create_fastly_store(kind, name)?;
            // If the platform store was created but the
            // writeback fails, remote state and the local
            // manifest are out of sync. Re-running `provision`
            // would attempt to create the platform store again
            // and fail with "already exists". Surface the
            // recovery path explicitly so the operator isn't
            // stuck.
            append_fastly_setup(&fastly_path, kind, name).map_err(|err| {
                format!(
                    "fastly {kind}-store `{name}` (logical id `{logical}`) was created remotely, but writeback to {path} failed: {err}\n  To recover, either:\n    1. Manually append `[setup.{kind}_stores.{name}]` to {path} and re-run, or\n    2. Delete the orphan remote store via `fastly {kind}-store delete --name={name}` and re-run `edgezero provision --adapter fastly`.",
                    path = fastly_path.display()
                )
            })?;
            // Fastly's `[setup.<kind>_stores.<name>]` table is
            // consumed ONLY when `fastly compute deploy` is
            // creating a NEW service. If `service_id` is
            // already present in fastly.toml, the service has
            // been deployed at least once and subsequent
            // deploys skip `[setup]` entirely — so the store
            // exists in the account but has no resource link
            // tying it to a service version, and the running
            // Compute service can't open it.
            //
            // Detect that case and EMIT the exact one-shot
            // command the operator should run to link the
            // store. We deliberately don't auto-run it: the
            // link cones the active version (`--autoclone`),
            // and silently mutating an already-deployed
            // service is surprising. The instruction names
            // both the store-id lookup AND the link command so
            // the operator can audit before committing.
            let post_create_note = resource_link_note(&fastly_path, kind, name)?;
            let mut line = format!(
                "created fastly {kind}-store `{name}` (logical id `{logical}`); appended setup tables to {}",
                fastly_path.display()
            );
            if let Some(note) = post_create_note {
                line.push('\n');
                line.push_str(&note);
            }
            out.push(line);
        }
    }
    // EdgeZero runtime overrides live in a dedicated Fastly Config
    // Store named `edgezero_runtime_env`. Compute@Edge has no
    // process env, so `EDGEZERO__STORES__CONFIG__<ID>__KEY` and
    // similar overrides have to come from a platform Config Store
    // the runtime opens by name (see
    // `env_config_from_runtime_dictionary` in lib.rs). Provision
    // owns the store creation alongside the operator's declared
    // stores so the runtime override path is wired correctly out
    // of the box; if the store already appears in
    // `[setup.config_stores.edgezero_runtime_env]`, skip.
    let runtime_env_kind = "config";
    let runtime_env_name = "edgezero_runtime_env";
    if dry_run {
        out.push(format!(
            "would run `fastly {runtime_env_kind}-store create --name={runtime_env_name}` and append [setup.{runtime_env_kind}_stores.{runtime_env_name}] to {} (EdgeZero runtime override store)",
            fastly_path.display()
        ));
    } else if !setup_block_present(&fastly_path, runtime_env_kind, runtime_env_name)? {
        create_fastly_store(runtime_env_kind, runtime_env_name)?;
        append_fastly_setup(&fastly_path, runtime_env_kind, runtime_env_name).map_err(|err| {
            format!(
                "fastly {runtime_env_kind}-store `{runtime_env_name}` was created remotely, but writeback to {path} failed: {err}\n  Recover via `fastly {runtime_env_kind}-store delete --name={runtime_env_name}` then re-run `edgezero provision --adapter fastly`.",
                path = fastly_path.display()
            )
        })?;
        // Same already-deployed-service caveat as the declared-store
        // path: if `service_id` is set in fastly.toml, the
        // `[setup.config_stores.edgezero_runtime_env]` table won't
        // be re-applied by the next `fastly compute deploy`, so the
        // runtime can't open the store. Emit the resource-link
        // remediation alongside the populate-keys hint.
        let post_create_note =
            resource_link_note(&fastly_path, runtime_env_kind, runtime_env_name)?;
        let mut line = format!(
            "created fastly {runtime_env_kind}-store `{runtime_env_name}` (EdgeZero runtime override store); appended setup tables to {}\n  Populate per-environment override keys with:\n    fastly config-store-entry update --store-id=<STORE-ID> --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY --value=app_config_staging --upsert",
            fastly_path.display()
        );
        if let Some(note) = post_create_note {
            line.push('\n');
            line.push_str(&note);
        }
        out.push(line);
    } else {
        // Already declared; nothing to do.
    }

    if out.is_empty() {
        out.push("fastly has no declared stores to provision".to_owned());
    }
    // Read-back the service_id from fastly.toml (if the operator has
    // already run `fastly compute deploy` at least once) and thread it
    // into ProvisionOutcome.deployed so the CLI's writeback path lands
    // `[adapters.fastly.deployed].service_id` in `edgezero.toml`.
    // deployed_fields() advertises ownership of `service_id`; without
    // this population the writeback is silently dropped and the
    // operator has to hand-copy from fastly.toml. Dry-run still
    // populates: the CLI's `merge_deployed_into_manifest` respects
    // its own dry_run flag and will only report (not write) the
    // pending edgezero.toml change.
    let deployed = match read_fastly_service_id(&fastly_path)? {
        Some(sid) => {
            let mut state = AdapterDeployedState::default();
            state.fields.insert("service_id".to_owned(), sid);
            Some(state)
        }
        None => None,
    };
    Ok(match deployed {
        Some(state) => ProvisionOutcome::with_deployed(out, state),
        None => ProvisionOutcome::from_status_lines(out),
    })
}

/// Shell out to `fastly <kind>-store create --name=<platform-name>`. The
/// caller resolves `<platform-name>` from `EDGEZERO__STORES__<KIND>__<ID>__NAME`
/// (falling back to the logical id), so this helper takes whatever the
/// caller hands it and does not re-translate. Returns `Ok(())` on success;
/// surfaces the CLI's stderr verbatim on failure (including the "already
/// exists" error, which is the caller's signal to fix the toml or use a
/// different name).
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
    // Idempotency: the fastly CLI returns non-zero with an
    // "already exists" message when a store of this name was
    // created by a prior provision run. Treat that as success so
    // the operator's recovery path -- "either manually append the
    // setup block or delete the remote and re-run provision" --
    // doesn't get blocked. The append step is itself idempotent,
    // so re-running provision after a writeback failure is the
    // documented recovery and now actually works.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if looks_like_already_exists(&stderr, kind) {
        return Ok(());
    }
    Err(format!(
        "`fastly {subcommand} create --name={name}` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

/// Heuristic: does the stderr blob look like a "store of this
/// kind, by this name, already exists" failure from the fastly
/// CLI? Different CLI versions phrase this slightly differently
/// ("a kv-store with that name already exists",
/// `"Conflict: duplicate kv_store name"`, etc.); we require BOTH
/// a conflict-signal keyword AND a store-kind reference so an
/// unrelated 409 ("Error: 409 Conflict on /service/...") cannot
/// be misread as idempotent success. The earlier wider heuristic
/// would have swallowed any stderr containing the word
/// "conflict" and let provision march on to writeback against a
/// nonexistent store, surfacing as a confusing deploy-time error.
fn looks_like_already_exists(stderr: &str, kind: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    let conflict_signal = lower.contains("already exists")
        || (lower.contains("duplicate") && lower.contains("name"))
        || lower.contains("conflict");
    if !conflict_signal {
        return false;
    }
    // Accept the three common spellings of `<kind>-store` /
    // `<kind>_store` / `<kind> store` so a fastly CLI version
    // bump that reshuffles punctuation still hits.
    let dashed = format!("{kind}-store");
    let underscored = format!("{kind}_store");
    let spaced = format!("{kind} store");
    lower.contains(&dashed) || lower.contains(&underscored) || lower.contains(&spaced)
}

/// Read the top-level `service_id` from `fastly.toml`. Returns
/// `Ok(None)` when the file is absent (scaffold state before first
/// `fastly compute deploy`) or when `service_id` is missing /
/// empty. Used by `provision` to detect when an already-deployed
/// service needs a separate resource-link step beyond `[setup]`
/// (which `compute deploy` only consumes on the FIRST deploy).
fn read_fastly_service_id(path: &Path) -> Result<Option<String>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let svc = doc
        .get("service_id")
        .and_then(|item| item.as_str())
        .map(str::to_owned)
        .filter(|svc_id| !svc_id.is_empty());
    Ok(svc)
}

/// If fastly.toml declares `service_id`, the next
/// `fastly compute deploy` skips `[setup]` entirely (it only runs on
/// the FIRST deploy of a service). Any store created by provision
/// after that needs a separate `fastly resource-link create` to link
/// the platform store to the service version. This helper returns the
/// remediation note to surface in the provision output, or `None`
/// when the service hasn't been deployed yet (so the next
/// `compute deploy` will pick up the `[setup]` row automatically).
fn resource_link_note(path: &Path, kind: &str, name: &str) -> Result<Option<String>, String> {
    let note = read_fastly_service_id(path)?.map(|svc_id| {
        format!(
            "  fastly.toml declares `service_id = \"{svc_id}\"`, so this service is already deployed -- `[setup]` will NOT be re-run on the next `fastly compute deploy`. The store exists in the account but is NOT yet linked to the service. To finish provisioning, look up the store id with `fastly {kind}-store list --json` (match by name=`{name}`), then run:\n    fastly resource-link create --service-id={svc_id} --resource-id=<STORE-ID> --version=latest --autoclone --name={name}\n  (the link clones the active version so existing traffic is not affected until you `fastly service-version activate`)."
        )
    });
    Ok(note)
}

/// Probe `fastly.toml` for the existence of `[setup.<kind>_stores.<id>]`.
/// Treats a missing file as "not present" so the first provision call
/// can create it.
///
/// Why only `[setup]` (no longer `[local_server]`): an empty
/// `[local_server.<kind>_stores.<id>]` table doesn't satisfy
/// fastly's local-server schema — config-stores need
/// `format = "inline-toml"` + a contents table, kv/secret stores
/// need a JSON `file = "..."` or an array of `{key, data}` entries.
/// Writing an empty table makes `fastly compute serve` skip the
/// declared store or error at startup. `provision`'s job is the
/// remote / `[setup]` half; local-server stanzas are written by
/// `edgezero config push --adapter fastly --local`
/// (config-stores only), and kv/secret local-server seeding is
/// hand-edited until we add equivalent writers for those kinds.
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
    Ok(doc
        .get("setup")
        .and_then(|root| root.get(plural.as_str()))
        .and_then(|kind_tbl| kind_tbl.get(id))
        .is_some())
}

/// Append `[setup.<kind>_stores.<id>]` to `fastly.toml`. Creates
/// the file (and the parent `[setup]` table) if absent. The block
/// is written as an empty table — that's what
/// `fastly compute deploy` consumes the first time it creates a
/// service: the resource-link declaration is enough, and the
/// account-level resource itself is already created in the
/// preceding `create_fastly_store` shellout.
///
/// We DON'T write `[local_server.<kind>_stores.<id>]` here: see
/// `setup_block_present`'s doc for the schema rationale. The local-
/// server seeding moved to `config push --local` (config-stores
/// only), so provision only owns the remote / setup half.
fn append_fastly_setup(path: &Path, kind: &str, id: &str) -> Result<(), String> {
    use toml_edit::{DocumentMut, Item, table};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let plural = format!("{kind}_stores");
    let parent_entry = doc.entry("setup").or_insert_with(table);
    let parent_tbl = parent_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `setup` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    let kind_entry = parent_tbl
        .entry(plural.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let kind_tbl = kind_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `setup.{plural}` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    if !kind_tbl.contains_key(id) {
        kind_tbl.insert(id, Item::Table(toml_edit::Table::new()));
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::FastlyCliAdapter;
    use super::super::run::synthesise_fastly_toml;
    use super::*;
    use edgezero_adapter::registry::{
        Adapter as _, ProvisionMode, ResolvedStoreId, TypedSecretEntry,
    };
    use tempfile::tempdir;

    // Shared fixture names.
    const TEST_KV_ID: &str = "sessions";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

    // ---------- looks_like_already_exists ----------

    #[test]
    fn looks_like_already_exists_recognises_common_phrasings() {
        // Real-shaped fastly CLI error strings (paraphrased; the
        // CLI varies across versions). Each must be detected so
        // create_fastly_store can treat it as idempotent success.
        assert!(looks_like_already_exists(
            "Error: a kv-store with that name already exists",
            "kv",
        ));
        assert!(looks_like_already_exists(
            "ERROR: Conflict (409): duplicate kv_store name",
            "kv",
        ));
        assert!(looks_like_already_exists(
            "A config-store with this name already exists",
            "config",
        ));
        // Spaced form: some fastly CLI versions emit prose
        // ("kv store"); accept it alongside the punctuated forms.
        assert!(looks_like_already_exists(
            "Error: kv store conflict: name already in use",
            "kv",
        ));
    }

    #[test]
    fn looks_like_already_exists_rejects_unrelated_errors() {
        assert!(!looks_like_already_exists(
            "Error: unauthenticated; run `fastly profile create`",
            "kv",
        ));
        assert!(!looks_like_already_exists(
            "Error: network unreachable",
            "kv",
        ));
        assert!(!looks_like_already_exists("", "kv"));
    }

    #[test]
    fn looks_like_already_exists_rejects_unrelated_conflict_errors() {
        // The earlier wider heuristic swallowed ANY stderr
        // containing "conflict" or "already exists", which would
        // misread an unrelated 409 from a different fastly
        // subcommand (e.g. a service-version conflict during a
        // parallel deploy) as idempotent store-create success.
        // Now we require the kind context too, so unrelated
        // conflicts surface as failures.
        assert!(
            !looks_like_already_exists(
                "Error: 409 Conflict on /service/abc/version/42 -- already exists",
                "kv",
            ),
            "service-version conflict must NOT be misread as kv-store idempotency"
        );
        assert!(
            !looks_like_already_exists(
                "Error: invalid duplicate request; check name resolution",
                "kv",
            ),
            "unrelated `duplicate ... name` AND-match must NOT trigger"
        );
        // And the kind must match: a config-store conflict must
        // not look-like-already-exists for a kv-store create call.
        assert!(
            !looks_like_already_exists("Error: a config-store with that name already exists", "kv",),
            "wrong-kind conflict must NOT trigger"
        );
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
        assert!(setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_false_when_id_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n[setup.kv_stores.other]\n").expect("write");
        assert!(!setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_false_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert!(!setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_true_when_only_setup_exists() {
        // Post-F6 (PR #269 round 2): `setup_block_present` only
        // checks `[setup.<kind>_stores.<id>]`. The pre-fix check
        // ALSO required `[local_server.<kind>_stores.<id>]`, but
        // writing an empty `[local_server.*]` table didn't match
        // fastly's local-server schema (config-stores need
        // `format` + contents, kv/secret stores need a JSON file
        // or `{key, data}` entries). Local-server seeding moved
        // to `config push --adapter fastly --local`, so probe
        // only cares about `[setup]` now.
        let dir = tempdir().expect("tempdir");
        let only_setup = dir.path().join("only_setup.toml");
        fs::write(&only_setup, "name = \"demo\"\n[setup.kv_stores.sessions]\n").expect("write");
        assert!(
            setup_block_present(&only_setup, "kv", TEST_KV_ID).expect("probe"),
            "[setup.*] alone is now sufficient: {only_setup:?}"
        );

        let only_local = dir.path().join("only_local.toml");
        fs::write(
            &only_local,
            "name = \"demo\"\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        assert!(
            !setup_block_present(&only_local, "kv", TEST_KV_ID).expect("probe"),
            "[local_server.*] alone is NOT a provisioned-setup signal"
        );
    }

    // ---------- append_fastly_setup ----------

    #[test]
    fn append_fastly_setup_creates_setup_table_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "setup table added: {after}"
        );
        // Post-F6: no `[local_server.*]` write — that empty stanza
        // didn't satisfy fastly's local-server schema and made
        // `fastly compute serve` error or skip the store. Local-
        // server seeding is now `config push --adapter fastly
        // --local`'s job.
        assert!(
            !after.contains("[local_server.kv_stores.sessions]"),
            "[local_server.*] empty table no longer written by provision: {after}"
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
        fs::write(&path, "[setup.kv_stores.cache]\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("append");
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
        fs::write(&path, "[setup.kv_stores.sessions]\nfoo = \"keep\"\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("idempotent append");
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
        append_fastly_setup(&path, "config", TEST_CONFIG_ID).expect("create");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("[setup.config_stores.app_config]"));
        assert!(
            !after.contains("[local_server.config_stores.app_config]"),
            "[local_server.*] no longer written by provision: {after}"
        );
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
        append_fastly_setup(&path, "secret", TEST_SECRET_ID).expect("append");
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
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        // 1 KV + 1 config + 1 secret + 1 runtime-env = 4 status lines.
        assert_eq!(out.status_lines.len(), 4);
        assert!(out.status_lines[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(
            out.status_lines[1]
                .contains("would run `fastly config-store create --name=app_config`")
        );
        assert!(
            out.status_lines[2].contains("would run `fastly secret-store create --name=default`")
        );
        assert!(
            out.status_lines[3]
                .contains("would run `fastly config-store create --name=edgezero_runtime_env`"),
            "runtime-env store row: {out:?}",
        );
        // Manifest untouched.
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated fastly.toml");
    }

    /// Cloud provision must populate `ProvisionOutcome.deployed` with
    /// `service_id` when fastly.toml declares it. `deployed_fields()`
    /// claims ownership of `service_id`; without this the writeback to
    /// `[adapters.fastly.deployed]` in edgezero.toml is silently
    /// dropped and the operator has to hand-copy from fastly.toml.
    ///
    /// Regression test: pre-`Adapters` fix, the cloud arm unconditionally
    /// returned `deployed: None`.
    #[test]
    fn provision_populates_deployed_service_id_from_fastly_toml() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // fastly.toml declares a service_id (as it would after a first
        // successful `fastly compute deploy`).
        fs::write(
            &path,
            "manifest_version = 3\nname = \"demo\"\nservice_id = \"SVC_ALREADY_DEPLOYED\"\n\n[local_server]\n",
        )
        .expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let outcome = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true, // dry-run avoids invoking the real fastly CLI
            )
            .expect("dry-run succeeds");
        let deployed = outcome
            .deployed
            .as_ref()
            .expect("deployed must be Some when fastly.toml declares service_id");
        assert_eq!(
            deployed.fields.get("service_id").map(String::as_str),
            Some("SVC_ALREADY_DEPLOYED"),
            "service_id must flow into ProvisionOutcome.deployed"
        );
    }

    /// Inverse: when `fastly.toml` has no `service_id` (fresh project,
    /// not yet deployed), cloud provision returns `deployed: None`.
    /// Nothing to write back -- the operator hasn't picked a service
    /// yet.
    #[test]
    fn provision_returns_none_deployed_when_fastly_toml_has_no_service_id() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "manifest_version = 3\nname = \"demo\"\n\n[local_server]\n",
        )
        .expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let outcome = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert!(
            outcome.deployed.is_none(),
            "no service_id in fastly.toml means deployed must be None"
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
        let err = FastlyCliAdapter
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
            err.contains("fastly.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Pre-populate the runtime-env block so the provision flow's
        // unconditional runtime-env step skips (otherwise it would
        // shell out to real `fastly` to create the store).
        fs::write(
            &path,
            "name = \"demo\"\n[setup.config_stores.edgezero_runtime_env]\n",
        )
        .expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("no-store provision is fine");
        assert_eq!(
            out.status_lines,
            vec!["fastly has no declared stores to provision"]
        );
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
            "[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n\
             [setup.config_stores.edgezero_runtime_env]\n",
        )
        .expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(
                dir.path(),
                Some("fastly.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("skip path succeeds without invoking fastly");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("already declared"),
            "got: {out:?}"
        );
    }

    /// When `fastly.toml` declares `service_id`, the next
    /// `fastly compute deploy` skips `[setup]` entirely. provision
    /// must emit the `fastly resource-link create` remediation for
    /// every store it creates -- including the implicit
    /// `edgezero_runtime_env` store the runtime override path
    /// depends on. Without this, a freshly-provisioned override
    /// store would not be linked to the already-deployed service
    /// and the runtime would silently fall back to baked defaults.
    #[test]
    fn provision_emits_resource_link_note_for_runtime_env_on_existing_service() {
        // Dry-run only -- we just want to drive the resource_link_note
        // helper for the runtime-env store branch. The real-create
        // path can't run in tests (would shell out to `fastly`).
        // The dry-run output line for runtime-env doesn't include the
        // note (the helper only fires on real create), so we test the
        // helper directly here.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\nservice_id = \"abc123svc\"\n").expect("write");
        let note = resource_link_note(&path, "config", "edgezero_runtime_env")
            .expect("read service_id")
            .expect("note present when service_id set");
        assert!(
            note.contains("service_id = \"abc123svc\""),
            "note quotes the service id: {note}"
        );
        assert!(
            note.contains("fastly config-store list --json"),
            "note tells operator how to find the store id: {note}"
        );
        assert!(
            note.contains("name=`edgezero_runtime_env`"),
            "note names the runtime override store: {note}"
        );
        assert!(
            note.contains(
                "fastly resource-link create --service-id=abc123svc --resource-id=<STORE-ID> --version=latest --autoclone --name=edgezero_runtime_env"
            ),
            "note carries the full resource-link command: {note}"
        );
    }

    /// And the inverse: no `service_id` (a service that hasn't been
    /// deployed yet) means `[setup]` will be applied on the next
    /// `compute deploy`, so no manual resource-link step is needed.
    /// The helper must return `None` to avoid noisy false-positive
    /// guidance.
    #[test]
    fn provision_skips_resource_link_note_when_service_undeployed() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let note =
            resource_link_note(&path, "config", "edgezero_runtime_env").expect("read service_id");
        assert!(
            note.is_none(),
            "no service_id => no resource-link prompt: {note:?}"
        );
    }

    /// Cloud mode is a no-op — real cloud secret storage uses
    /// `fastly secret-store-entry create` at deploy time, not local
    /// `.toml` writeback. Assert empty outcome + untouched manifest.
    #[test]
    fn fastly_provision_typed_cloud_mode_is_a_no_op() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let baseline = synthesise_fastly_toml("demo", None);
        fs::write(&path, &baseline).expect("write");
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        let outcome = FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &entries,
                ProvisionMode::Cloud,
                false,
            )
            .expect("cloud mode is a no-op, must succeed");
        assert!(
            outcome.status_lines.is_empty(),
            "cloud outcome status_lines empty: {:?}",
            outcome.status_lines
        );
        assert!(outcome.deployed.is_none(), "cloud outcome deployed is None");
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, baseline, "fastly.toml untouched in cloud mode");
    }
}
