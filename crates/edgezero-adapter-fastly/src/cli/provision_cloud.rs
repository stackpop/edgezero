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
    deployed: Option<&AdapterDeployedState>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    // A user store named like the internal runtime-override config store
    // would be created remotely AND merged into the runtime-env store --
    // reject BEFORE any account mutation (dry-run too, so the preview
    // models the real outcome).
    super::reject_reserved_store_names(stores)?;
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

    // Cloud provision MUTATES REMOTE ACCOUNT STATE and then records the
    // resource link in fastly.toml. `fastly.toml` is gitignored, so on a
    // clean clone it is absent -- and creating the remote stores first
    // would then materialise a file containing ONLY `[setup.*]`: a
    // manifest with no `manifest_version` / `name` / `language` that
    // `fastly compute build` rejects, guarding stores that are now
    // orphaned in the account. Refuse BEFORE any account mutation (in
    // dry-run too, so the preview models the real outcome).
    if !fastly_path.exists() {
        return Err(format!(
            "{}: not found. Cloud provision records the stores it creates in fastly.toml, and \
             must not create remote resources against a manifest that does not exist yet. Run \
             `provision --adapter fastly --local` first to synthesise the baseline manifest, then \
             re-run cloud provision.",
            fastly_path.display()
        ));
    }

    // Reconcile the service_id in PREFLIGHT so a tracked/local conflict
    // aborts before any `fastly *-store create` runs or `[setup]` is
    // written -- otherwise a known conflict could leave an orphaned
    // remote store and a mutated manifest, and dry-run (which never
    // reached the old post-create check) would miss it entirely.
    let service_id = reconcile_service_id(&fastly_path, deployed)?;

    // Preflight the ENTIRE `[setup]` writeback shape BEFORE creating any
    // remote store. `append_fastly_setup` requires `setup` and each
    // `setup.<kind>_stores` to be standard tables; if a malformed (but
    // syntactically valid) manifest has e.g. `setup = "x"`, the writeback
    // would fail AFTER `fastly *-store create` already ran, orphaning the
    // remote resource. Checking here means a bad manifest aborts before
    // any account mutation, and dry-run predicts the same failure.
    assert_setup_writeback_shape(&fastly_path)?;

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
            // Check the skip condition FIRST, so dry-run models what the
            // real run does: if the `[setup.*]` block is already present
            // the real invocation skips the store, and dry-run must
            // report "would skip", not "would create".
            if setup_block_present(&fastly_path, kind, name)? {
                let mut line = format!(
                    "fastly {kind}-store `{name}` (logical id `{logical}`) already declared in {}; skipping. To force a fresh remote: delete the [setup.{kind}_stores.{name}] block AND run `fastly {kind}-store delete --name={name}` (the old remote store lingers otherwise), then re-run provision.",
                    fastly_path.display()
                );
                // Convergence: if the service is already deployed, `[setup]`
                // is never re-run, so a store declared-but-not-linked stays
                // unlinked. Re-emit the resource-link remediation on EVERY
                // skip run so an operator who missed the first message can
                // still recover -- provision is otherwise a dead end here.
                if let Some(note) = resource_link_note(service_id.as_deref(), kind, name) {
                    line.push('\n');
                    line.push_str(&note);
                }
                out.push(line);
                continue;
            }
            if dry_run {
                out.push(format!(
                    "would run `fastly {kind}-store create --name={name}` and append [setup.{kind}_stores.{name}] to {} (logical id `{logical}`)",
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
            let post_create_note = resource_link_note(service_id.as_deref(), kind, name);
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
    // Check the skip condition FIRST -- BEFORE the dry-run branch -- the
    // same way the declared-store loop above does. When the setup block
    // already exists the real invocation skips silently, so a dry-run
    // that reported "would create" would promise an account mutation
    // that never happens.
    if setup_block_present(&fastly_path, runtime_env_kind, runtime_env_name)? {
        // Already declared; nothing to do, and nothing to report.
    } else if dry_run {
        out.push(format!(
            "would run `fastly {runtime_env_kind}-store create --name={runtime_env_name}` and append [setup.{runtime_env_kind}_stores.{runtime_env_name}] to {} (EdgeZero runtime override store)",
            fastly_path.display()
        ));
    } else {
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
            resource_link_note(service_id.as_deref(), runtime_env_kind, runtime_env_name);
        let mut line = format!(
            "created fastly {runtime_env_kind}-store `{runtime_env_name}` (EdgeZero runtime override store); appended setup tables to {}\n  Populate per-environment override keys with:\n    fastly config-store-entry update --store-id=<STORE-ID> --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY --value=app_config_staging --upsert",
            fastly_path.display()
        );
        if let Some(note) = post_create_note {
            line.push('\n');
            line.push_str(&note);
        }
        out.push(line);
    }

    if out.is_empty() {
        out.push("fastly has no declared stores to provision".to_owned());
    }
    // Cloud provision does NOT write back `service_id`. Per spec
    // §"Writeback ownership" (and the plan's "Fastly note"), Fastly's
    // `service_id` is populated by `fastly compute deploy` -- which
    // runs as the manifest `[adapters.fastly.commands].deploy` shell
    // command, bypassing the adapter dispatch entirely -- and the
    // operator does a documented ONE-TIME copy from `fastly.toml` into
    // `[adapters.fastly.deployed].service_id`. An earlier build
    // auto-captured the id here; that both exceeded the v1 contract
    // AND opened a data-loss path where a stale, gitignored,
    // per-machine `fastly.toml` silently replaced the team's committed
    // id. Local provision still pins the
    // TRACKED id INTO fastly.toml (the spec-blessed direction); only
    // this reverse auto-capture is removed.
    Ok(ProvisionOutcome::from_status_lines(out))
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

/// Reconcile the tracked vs local `service_id` BEFORE any account
/// mutation, returning the authoritative id (or `None` when the service
/// hasn't been deployed yet).
///
/// Tracked `[adapters.fastly.deployed].service_id` in edgezero.toml is
/// the DURABLE AUTHORITY (spec §"Deployed state"): fastly.toml is
/// gitignored and per-machine, so a stale or regenerated copy must not
/// steer the resource-link command at the wrong service. Prefer tracked;
/// if the local file DISAGREES, refuse. Fall back to the local id only
/// when nothing is tracked (a service deployed via `fastly compute
/// deploy` before any provision captured its id).
///
/// Called in preflight so a known conflict aborts BEFORE `provision`
/// creates any remote store or edits the manifest -- and so dry-run
/// surfaces the same conflict a real run would.
fn reconcile_service_id(
    path: &Path,
    deployed: Option<&AdapterDeployedState>,
) -> Result<Option<String>, String> {
    let tracked = deployed
        .and_then(|state| state.fields.get("service_id"))
        .filter(|svc_id| !svc_id.is_empty())
        .cloned();
    let local = read_fastly_service_id(path)?;
    match (tracked, local) {
        (Some(tracked_id), Some(local_id)) if tracked_id != local_id => Err(format!(
            "service_id conflict for the fastly adapter: gitignored `{}` declares `{local_id}`, \
             but tracked `[adapters.fastly.deployed].service_id` is `{tracked_id}`. fastly.toml \
             is per-machine, so provision will not recommend linking resources to the local id. \
             Resolve by hand: update the tracked value in edgezero.toml, or delete the stale \
             `service_id` from fastly.toml.",
            path.display()
        )),
        (Some(tracked_id), _) => Ok(Some(tracked_id)),
        (None, local_id) => Ok(local_id),
    }
}

/// If a `service_id` is recorded, the next `fastly compute deploy` skips
/// `[setup]` entirely (it only runs on the FIRST deploy of a service),
/// so any store provision creates afterwards needs a separate
/// `fastly resource-link create`. Build that remediation note from the
/// already-reconciled `service_id` (see [`reconcile_service_id`]), or
/// `None` when the service hasn't been deployed yet.
fn resource_link_note(service_id: Option<&str>, kind: &str, name: &str) -> Option<String> {
    service_id.map(|svc_id| {
        format!(
            "  `service_id = \"{svc_id}\"` is recorded (tracked `[adapters.fastly.deployed]` takes precedence over the local fastly.toml), so this service is already deployed -- `[setup]` will NOT be re-run on the next `fastly compute deploy`. The store exists in the account but is NOT yet linked to the service. To finish provisioning, look up the store id with `fastly {kind}-store list --json` (match by name=`{name}`), then run:\n    fastly resource-link create --service-id={svc_id} --resource-id=<STORE-ID> --version=latest --autoclone --name={name}\n  (the link clones the active version so existing traffic is not affected until you `fastly service-version activate`)."
        )
    })
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
/// Validate that the `[setup]` writeback target is well-formed for EVERY
/// store kind before any remote store is created. `append_fastly_setup`
/// requires `setup` and each `setup.<kind>_stores` to be standard tables;
/// this mirrors that requirement so a malformed-but-valid-TOML manifest is
/// rejected up front rather than after a `fastly *-store create` orphans a
/// remote resource. A missing file is fine -- the writeback creates it.
fn assert_setup_writeback_shape(path: &Path) -> Result<(), String> {
    use toml_edit::DocumentMut;

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let Some(setup) = doc.get("setup") else {
        return Ok(());
    };
    let Some(setup_tbl) = setup.as_table() else {
        return Err(format!(
            "{}: `setup` exists but is not a table; refusing to create any remote store",
            path.display()
        ));
    };
    for plural in ["kv_stores", "config_stores", "secret_stores"] {
        let Some(item) = setup_tbl.get(plural) else {
            continue;
        };
        let Some(kind_tbl) = item.as_table() else {
            return Err(format!(
                "{}: `setup.{plural}` exists but is not a table; refusing to create any remote store",
                path.display()
            ));
        };
        // Validate EVERY managed child too. `setup_block_present` treats any
        // existing `setup.<plural>.<name>` value as "already provisioned"
        // and skips it -- so a malformed scalar like `sessions = "broken"`
        // would be silently skipped AFTER an earlier store was already
        // created remotely, leaving partial state. A legitimate setup entry
        // is a `[setup.<plural>.<name>]` block (standard or inline table).
        for (name, child) in kind_tbl {
            if child.as_table_like().is_none() {
                return Err(format!(
                    "{}: `setup.{plural}.{name}` is not a table; a store setup entry must be a `[setup.{plural}.{name}]` block. Refusing to create any remote store against a malformed manifest.",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

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
        Adapter as _, AdapterDeployedState, ProvisionMode, ResolvedStoreId, TypedSecretEntry,
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
        // `setup_block_present` only checks
        // `[setup.<kind>_stores.<id>]`. An earlier check
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

    // ---------- assert_setup_writeback_shape ----------

    #[test]
    fn assert_setup_writeback_shape_accepts_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        assert_setup_writeback_shape(&path).expect("missing file is writeable");
    }

    #[test]
    fn assert_setup_writeback_shape_accepts_well_formed_setup() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup.kv_stores.cache]\n").expect("write");
        assert_setup_writeback_shape(&path).expect("well-formed setup accepted");
    }

    #[test]
    fn assert_setup_writeback_shape_rejects_non_table_setup() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "setup = \"nope\"\n").expect("write");
        let err = assert_setup_writeback_shape(&path).expect_err("non-table setup rejected");
        assert!(err.contains("`setup` exists but is not a table"), "{err}");
    }

    #[test]
    fn assert_setup_writeback_shape_rejects_non_table_kind_stores() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup]\nkv_stores = \"nope\"\n").expect("write");
        let err = assert_setup_writeback_shape(&path).expect_err("non-table kind rejected");
        assert!(
            err.contains("`setup.kv_stores` exists but is not a table"),
            "{err}"
        );
    }

    #[test]
    fn assert_setup_writeback_shape_rejects_scalar_child_entry() {
        // A scalar child (`sessions = "broken"`) would be misread by
        // `setup_block_present` as an already-provisioned store and skipped
        // -- after earlier stores were created remotely. The preflight must
        // reject it before the first remote mutation.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup.kv_stores]\nsessions = \"broken\"\n").expect("write");
        let err = assert_setup_writeback_shape(&path).expect_err("scalar child rejected");
        assert!(
            err.contains("`setup.kv_stores.sessions` is not a table"),
            "{err}"
        );
    }

    #[test]
    fn assert_setup_writeback_shape_accepts_inline_table_child() {
        // An inline-table child is a legitimate declaration.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup.kv_stores]\nsessions = {}\n").expect("write");
        assert_setup_writeback_shape(&path).expect("inline-table child accepted");
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

    /// Spec contract: cloud provision
    /// NEVER writes back `service_id`, even when `fastly.toml` already
    /// declares one. Per spec §"Writeback ownership" the id is
    /// populated by `fastly compute deploy` and copied into
    /// `edgezero.toml` once, by hand. Auto-capturing it here exceeded
    /// the v1 contract and let a stale gitignored `fastly.toml`
    /// overwrite the team's committed id.
    #[test]
    fn cloud_provision_never_writes_back_service_id() {
        let dir = tempdir().expect("tempdir");
        // fastly.toml declares a service_id (as it would after a first
        // successful `fastly compute deploy`) -- and cloud provision
        // must STILL leave `deployed` empty.
        fs::write(
            dir.path().join("fastly.toml"),
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
                // Tracked id AGREES with fastly.toml's (a differing id is
                // a conflict the preflight refuses; see
                // `provision_cloud_refuses_service_id_conflict_before_any_mutation`).
                // The point here is that even a KNOWN service_id is never
                // captured back into `deployed`.
                Some(&{
                    let mut state = AdapterDeployedState::default();
                    state
                        .fields
                        .insert("service_id".to_owned(), "SVC_ALREADY_DEPLOYED".to_owned());
                    state
                }),
                ProvisionMode::Cloud,
                true, // dry-run avoids invoking the real fastly CLI
            )
            .expect("dry-run succeeds");
        assert!(
            outcome.deployed.is_none(),
            "cloud provision must never write back service_id: {:?}",
            outcome.deployed
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
    fn provision_skip_path_emits_resource_link_note_on_existing_service() {
        // A store already declared in `[setup]` on an already-deployed
        // service (service_id present) is SKIPPED -- but `[setup]` is never
        // re-run, so the store stays unlinked. The skip line must re-emit
        // the resource-link remediation so an operator who missed the first
        // run can still recover.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "name = \"demo\"\nservice_id = \"SVC1\"\n\n[setup.kv_stores.sessions]\n",
        )
        .expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let outcome = provision(dir.path(), Some("fastly.toml"), &stores, None, true)
            .expect("dry-run provision succeeds");
        let joined = outcome.status_lines.join("\n");
        assert!(
            joined.contains("skipping"),
            "the store must be skipped: {joined}"
        );
        assert!(
            joined.contains("resource-link create") && joined.contains("SVC1"),
            "the skip path must re-emit the resource-link remediation: {joined}"
        );
    }

    #[test]
    fn cloud_provision_refuses_when_fastly_toml_is_missing() {
        // fastly.toml is gitignored, so a clean clone has none. Creating
        // remote stores first and then writing a `[setup.*]`-only file
        // would orphan those stores behind a manifest `fastly compute
        // build` rejects. Refuse BEFORE any account mutation.
        let dir = tempdir().expect("tempdir");
        // No fs::write -- fastly.toml absent.
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = provision(dir.path(), Some("fastly.toml"), &stores, None, false)
            .expect_err("cloud provision must refuse without a baseline manifest");
        assert!(
            err.contains("provision --adapter fastly --local"),
            "error points at local provision to synthesise the baseline: {err}"
        );
        assert!(
            !dir.path().join("fastly.toml").exists(),
            "refusal must not materialise a manifest"
        );
    }

    #[test]
    fn cloud_provision_dry_run_also_refuses_when_fastly_toml_is_missing() {
        // The dry-run preview must model the real outcome, not promise
        // creations the real run would refuse to perform.
        let dir = tempdir().expect("tempdir");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = provision(dir.path(), Some("fastly.toml"), &stores, None, true)
            .expect_err("dry-run must refuse too");
        assert!(err.contains("provision --adapter fastly --local"), "{err}");
    }

    #[test]
    fn cloud_dry_run_does_not_claim_to_create_existing_runtime_env_store() {
        // Regression: the runtime-env arm reported "would create"
        // unconditionally, so a dry-run promised an account mutation the
        // real run skips.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
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
        let out = provision(dir.path(), Some("fastly.toml"), &stores, None, true)
            .expect("dry-run succeeds");
        let combined = out.status_lines.join("\n");
        assert!(
            !combined
                .contains("would run `fastly config-store create --name=edgezero_runtime_env`"),
            "dry-run must not claim to create an already-declared store: {combined}"
        );
    }

    #[test]
    fn reconcile_service_id_falls_back_to_tracked() {
        // fastly.toml is gitignored: a teammate's regenerated manifest has
        // no `service_id` even though the team's tracked deployed state
        // says the service IS deployed. Reading only the local file would
        // skip the remediation and leave the new store unlinked.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let mut tracked = AdapterDeployedState::default();
        tracked
            .fields
            .insert("service_id".to_owned(), "tracked123".to_owned());
        let resolved = reconcile_service_id(&path, Some(&tracked))
            .expect("read")
            .expect("tracked service_id must be used when the local file lacks one");
        assert_eq!(resolved, "tracked123");
        let note = resource_link_note(Some(&resolved), "kv", "sessions")
            .expect("note present for a deployed service");
        assert!(note.contains("tracked123"), "note uses the id: {note}");
    }

    #[test]
    fn reconcile_service_id_refuses_tracked_local_conflict_in_preflight() {
        // The tracked id is the durable authority: even when a stale
        // local fastly.toml carries a DIFFERENT id, provision must refuse
        // rather than recommend linking to the wrong service.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\nservice_id = \"local999\"\n").expect("write");
        let mut tracked = AdapterDeployedState::default();
        tracked
            .fields
            .insert("service_id".to_owned(), "tracked123".to_owned());
        let err = reconcile_service_id(&path, Some(&tracked))
            .expect_err("a tracked/local service_id conflict must be refused");
        assert!(
            err.contains("tracked123") && err.contains("local999") && err.contains("conflict"),
            "error names both ids and the conflict: {err}"
        );
    }

    #[test]
    fn reconcile_service_id_uses_tracked_when_it_matches_local() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\nservice_id = \"same123\"\n").expect("write");
        let mut tracked = AdapterDeployedState::default();
        tracked
            .fields
            .insert("service_id".to_owned(), "same123".to_owned());
        let resolved = reconcile_service_id(&path, Some(&tracked))
            .expect("matching ids are fine")
            .expect("id present");
        assert_eq!(resolved, "same123");
    }

    #[test]
    fn provision_cloud_refuses_service_id_conflict_before_any_mutation() {
        // The conflict must abort in preflight -- BEFORE any store is
        // created or fastly.toml is edited -- and in dry-run too.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let original = "name = \"demo\"\nservice_id = \"local999\"\n";
        fs::write(&path, original).expect("write");
        let mut tracked = AdapterDeployedState::default();
        tracked
            .fields
            .insert("service_id".to_owned(), "tracked123".to_owned());
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        for dry_run in [true, false] {
            let err = provision(
                dir.path(),
                Some("fastly.toml"),
                &stores,
                Some(&tracked),
                dry_run,
            )
            .expect_err("a service_id conflict must abort provision");
            assert!(err.contains("conflict"), "dry_run={dry_run}: {err}");
        }
        // fastly.toml must be byte-identical -- no mutation happened.
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            original,
            "conflict must abort before any manifest edit"
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

    #[test]
    fn provision_dry_run_reports_skip_for_already_declared_store() {
        // Dry-run must model the real operation: a store whose
        // `[setup.*]` block already exists is skipped by the real run,
        // so dry-run reports "already declared; skipping", NOT
        // "would create".
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("fastly.toml"),
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
                true,
            )
            .expect("dry-run succeeds");
        assert!(
            out.status_lines[0].contains("already declared")
                && !out.status_lines[0].contains("would run"),
            "dry-run must report the skip, not a would-create: {out:?}"
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
        let service_id = reconcile_service_id(&path, None).expect("read service_id");
        let note = resource_link_note(service_id.as_deref(), "config", "edgezero_runtime_env")
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
        let service_id = reconcile_service_id(&path, None).expect("read service_id");
        let note = resource_link_note(service_id.as_deref(), "config", "edgezero_runtime_env");
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
