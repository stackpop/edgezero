use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chunked_config::{
    CHUNK_KEY_INFIX, GcPointer, GcRootValue, chunk_key_generation, gc_classify_root,
    gc_verify_generation, prepare_fastly_config_entries, prior_chunk_keys,
    resolve_fastly_config_value, sha256_hex, value_is_pointer_kind,
};
use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    Adapter, AdapterAction, AdapterPushContext, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
    register_adapter,
};
use edgezero_adapter::scaffold::{
    AdapterBlueprint, AdapterFileSpec, CommandTemplates, DependencySpec, LoggingDefaults,
    ManifestSpec, ReadmeInfo, TemplateRegistration, register_adapter_blueprint,
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
        fallback: "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly_wasm",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback: "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false, features = [\"fastly\"] }",
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

const FASTLY_INSTALL_HINT: &str = "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

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

/// The reclamation plan for `config gc`: the orphan chunk entries to delete
/// (with their ages) plus the counts for the summary line. Produced by
/// `plan_gc_reclamation` (which owns every safety guard); consumed by
/// `gc_fastly_config_store` (which reports and deletes).
struct GcPlan {
    /// Whole generations to reclaim, each a list of `(key, age_secs)`. Grouped,
    /// not flat: a generation is provable only as a UNIT (see
    /// `prove_generation`), so deleting part of one destroys the very evidence
    /// that licenses deleting the rest.
    doomed: Vec<Vec<(String, u64)>>,
    live_count: usize,
    retained_recent: usize,
    roots: usize,
    /// Chunk-shaped entries we could NOT prove our writer produced, so left
    /// untouched. Surfaced so an operator can see we declined to judge them.
    unprovable: usize,
}

/// What one pass of `config gc`'s delete loop actually did.
struct GcDeleteOutcome {
    /// Entries whose delete returned success.
    deleted: usize,
    /// Keys whose delete returned non-zero.
    failed: Vec<String>,
    /// Survivors of a generation in which an earlier sibling's delete had
    /// ALREADY succeeded before a later one failed. These are definitely an
    /// incomplete generation now, so they can never be proved (or reclaimed)
    /// again -- manual removal only.
    stranded: Vec<String>,
    /// Members of a generation whose ONLY failure was on a delete with no
    /// confirmed prior sibling success. A failed remote delete has UNKNOWN
    /// outcome (Fastly may have committed it before returning an error), so we
    /// cannot say whether the generation is still whole. A re-run reclaims it if
    /// it is, or reports it as an unprovable fragment if it is not.
    uncertain: Vec<String>,
}

/// The result of classifying a store's entries for reclamation.
struct GcClassification {
    /// Chunk keys a live root pointer references, each verified against its
    /// content-address. Never deletable.
    live: HashSet<String>,
    /// Keys whose OWN value is a runtime-readable root — a valid direct envelope
    /// or a pointer — regardless of what their key looks like. Never deletable.
    protected: HashSet<String>,
    /// Count of entries classified as roots, for the summary line.
    roots: usize,
}

/// One `config-store-entry list` item.
///
/// `item_value` IS captured — `config gc` must parse root pointers to learn
/// which chunks are live, and one listing avoids a `describe` per root. It is
/// the config payload: it may be read in memory but must NEVER be logged or
/// surfaced (see `redact_describe_response` / `redact_stderr`).
struct ConfigStoreItem {
    created_at: String,
    item_key: String,
    item_value: String,
}

/// Per-root plan for the LOCAL path's eager prune.
///
/// Local reclamation is safe to do immediately: `fastly.toml` is a single
/// file that Viceroy reads at startup — there is no propagation window and no
/// POP that could still be serving the previous pointer. (The cloud path
/// cannot do this; see `reclaim_orphan_generations`.)
struct FastlyConfigGcPlan {
    /// Exact keep-set this push writes for the root (chunk keys + root key).
    new_keys: HashSet<String>,
    /// Prior chunk keys to consider deleting, or a warning to surface
    /// (suspicious prior pointer) that skips GC for this root.
    prior_keys: Result<Vec<String>, String>,
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
    reason = "see the explanatory block comment immediately above; fastly's no-op defaults for the three validate_* hooks are intentional and documented. `read_config_entry` and `read_config_entry_local` are both overridden below. `single_store_kinds` IS overridden below (returns `&[]`)."
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

    fn gc_config_entries(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        _push_ctx: &AdapterPushContext<'_>,
        older_than_secs: u64,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        gc_fastly_config_store(store.platform.as_str(), older_than_secs, dry_run)
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
            append_fastly_setup(&fastly_path, runtime_env_kind, runtime_env_name).map_err(
                |err| {
                    format!(
                        "fastly {runtime_env_kind}-store `{runtime_env_name}` was created remotely, but writeback to {path} failed: {err}\n  Recover via `fastly {runtime_env_kind}-store delete --name={runtime_env_name}` then re-run `edgezero provision --adapter fastly`.",
                        path = fastly_path.display()
                    )
                },
            )?;
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
        Ok(out)
    }

    fn push_config_entries(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Resolve the platform config-store id on demand via
        // `fastly config-store list --json` (matched by name =
        // `store.platform`), then `fastly config-store-entry update
        // --store-id=<id> --key=<k> --upsert --stdin` per physical
        // entry. Entries are logical blob-envelope entries from
        // the CLI (one (key, envelope_json) per push); oversized
        // Fastly values are expanded below into chunk entries plus
        // a root pointer by `chunked_config::prepare_fastly_config_entries`.
        let logical = store.logical.as_str();
        let name = store.platform.as_str();
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to fastly config-store `{name}` (logical id `{logical}`)"
            )]);
        }
        // Reject reserved keys before any expansion or I/O.
        reject_reserved_root_keys(entries)?;
        reject_duplicate_root_keys(entries)?;
        // Expand each logical root once: flatten for the commit, and keep
        // the exact per-root keep-set + the value written at the root key
        // for GC (no prefix scan of the flattened set). Collecting all
        // physical entries first also surfaces pointer-too-large errors
        // before touching the remote store.
        let mut physical_entries: Vec<(String, String)> = Vec::new();
        let mut roots: Vec<(String, HashSet<String>, String)> = Vec::with_capacity(entries.len());
        for (key, body) in entries {
            let (expanded, new_keys, new_root_value) = expand_root(key, body)?;
            physical_entries.extend(expanded);
            roots.push((key.clone(), new_keys, new_root_value));
        }
        if dry_run {
            // Report intent without shelling out. Stays fully offline: no
            // store-id resolution, no remote read (so no GC count).
            let mut out = Vec::with_capacity(entries.len().saturating_mul(2).saturating_add(1));
            out.push(format!(
                "would resolve fastly config-store `{name}` (logical id `{logical}`) via `fastly config-store list --json` and push entries:"
            ));
            for (key, body) in entries {
                let expanded = prepare_fastly_config_entries(key, body)
                    .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
                if expanded.len() == 1 {
                    out.push(format!(
                        "  would push `{key}` as direct entry ({}B)",
                        body.len()
                    ));
                } else {
                    let chunk_count = expanded.len().saturating_sub(1);
                    out.push(format!(
                        "  would push `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                        body.len()
                    ));
                }
            }
            return Ok(out);
        }
        let resolved_id = resolve_remote_config_store_id(name)?;
        // NOTE: a cloud push does NOT reclaim orphaned chunks.
        //
        // Fastly's config store is eventually consistent, so a generation may
        // only be deleted once the pointer that referenced it has stopped being
        // served everywhere. Fastly records no pointer-supersession time
        // (`updated_at` is NOT bumped by `update --upsert` -- verified against
        // the live API), offers no compare-and-swap with which to record one
        // safely, and chunk `created_at` is NOT a proxy for it (a chunked ->
        // direct -> direct transition leaves the old generation with no
        // "successor" at all). Every attempt to synthesise that fact is unsound.
        //
        // So reclamation is an explicit, operator-invoked `config gc`: the
        // operator supplies the one fact the platform cannot -- that the current
        // config has been live long enough that nothing is serving the old
        // pointers. See the spec's "Cloud reclamation".
        push_entries_with_committer(&physical_entries, |key, value| {
            create_config_store_entry(&resolved_id, key, value)
        })?;
        Ok(vec![format!(
            "pushed {} physical entries ({} logical) to fastly config-store `{name}` (logical id `{logical}`, id={resolved_id})",
            physical_entries.len(),
            entries.len()
        )])
    }

    fn push_config_entries_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Local-emulator path: edit
        // `[local_server.config_stores.<platform>.contents]` in
        // `fastly.toml`. Viceroy reads it on startup, so a
        // subsequent `fastly compute serve` exposes the new values
        // to the wasm component. No shell-out to the production
        // Fastly CLI -- the operator may not be authenticated and
        // wouldn't want a local push to touch production anyway.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for config push --local"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);
        let logical = store.logical.as_str();
        let name = store.platform.as_str();
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to `[local_server.config_stores.{name}]` in {} (logical id `{logical}`)",
                fastly_path.display()
            )]);
        }
        // Reject reserved keys before any expansion or I/O.
        reject_reserved_root_keys(entries)?;
        reject_duplicate_root_keys(entries)?;
        // Expand each logical root once: flatten for the write, keep the
        // exact per-root keep-set for GC (no prefix scan of the flattened set).
        let mut physical_entries: Vec<(String, String)> = Vec::new();
        let mut gc_roots: Vec<(String, HashSet<String>)> = Vec::with_capacity(entries.len());
        for (key, body) in entries {
            let (expanded, new_keys, _new_root) = expand_root(key, body)?;
            physical_entries.extend(expanded);
            gc_roots.push((key.clone(), new_keys));
        }
        if dry_run {
            let counts = local_orphan_counts_for_dry_run(&fastly_path, name, entries);
            let mut out = Vec::with_capacity(entries.len().saturating_mul(2).saturating_add(1));
            out.push(format!(
                "would edit `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`) with entries:",
                fastly_path.display(),
            ));
            for (idx, (key, body)) in entries.iter().enumerate() {
                let expanded = prepare_fastly_config_entries(key, body)
                    .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
                if expanded.len() == 1 {
                    out.push(format!(
                        "  would set `{key}` as direct entry ({}B)",
                        body.len()
                    ));
                } else {
                    let chunk_count = expanded.len().saturating_sub(1);
                    out.push(format!(
                        "  would set `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                        body.len()
                    ));
                }
                match counts.get(idx).map(|(_, count)| count) {
                    Some(Ok(n)) => out.push(format!(
                        "  would delete {n} orphan chunks from the previous generation of `{key}`"
                    )),
                    Some(Err(reason)) => out.push(format!(
                        "  would delete an unknown number of orphan chunks from the previous generation of `{key}` (unknown: {reason})"
                    )),
                    None => {}
                }
            }
            return Ok(out);
        }
        let warnings =
            write_fastly_local_config_store(&fastly_path, name, &physical_entries, &gc_roots)?;
        let mut out = vec![format!(
            "wrote {} physical entries ({} logical) to `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`); restart `fastly compute serve` to pick up changes",
            physical_entries.len(),
            entries.len(),
            fastly_path.display()
        )];
        out.extend(warnings);
        Ok(out)
    }

    fn read_config_entry(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        // Shell out to `fastly config-store-entry describe
        // --store-id=<id> --key=<key> --json`, resolve the store id on
        // demand via `fastly config-store list --json`, then parse the
        // JSON response.
        let name = store.platform.as_str();
        let store_id = match resolve_remote_config_store_id(name) {
            Ok(id) => id,
            Err(err) => {
                // "not found" from resolve means the store doesn't exist.
                let lower = err.to_ascii_lowercase();
                if lower.contains("not found")
                    || lower.contains("did you run")
                    || lower.contains("no fastly config-store matches")
                {
                    return Ok(ReadConfigEntry::MissingStore);
                }
                return Err(err);
            }
        };
        let store_arg = format!("--store-id={store_id}");
        let key_arg = format!("--key={key}");
        let output = Command::new("fastly")
            .args([
                "config-store-entry",
                "describe",
                store_arg.as_str(),
                key_arg.as_str(),
                "--json",
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
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse the JSON and extract the `item_value` field.
            let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|err| {
                format!(
                    "failed to parse `fastly config-store-entry describe` JSON: {err} (response: {})",
                    redact_describe_response(&stdout)
                )
            })?;
            let value = parsed
                .get("item_value")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "`fastly config-store-entry describe` JSON has no string `item_value` field; \
                         fastly CLI may have changed its output schema. (response: {})",
                        redact_describe_response(&stdout)
                    )
                })?;
            // Resolve chunk pointers: if `value` is a direct BlobEnvelope it
            // passes through unchanged; if it is a chunk pointer the chunks
            // are fetched from the same store and reconstructed.
            let resolved = resolve_fastly_config_value(key, value.to_owned(), |chunk_key| {
                fetch_remote_config_store_entry(&store_id, chunk_key)
            })?;
            return Ok(ReadConfigEntry::Present(resolved));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("not found") || lower.contains("does not exist") || lower.contains("404")
        {
            return Ok(ReadConfigEntry::MissingKey);
        }
        Err(format!(
            "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` exited with status {}\nstderr: {}",
            output.status,
            redact_stderr(&stderr)
        ))
    }

    fn read_config_entry_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        // Read from `[local_server.config_stores.<platform_name>.contents]`
        // in fastly.toml — the same section `push_config_entries_local` writes.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for config diff --local"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);
        let name = store.platform.as_str();
        let raw = match fs::read_to_string(&fastly_path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(ReadConfigEntry::MissingStore);
            }
            Err(err) => {
                return Err(format!("failed to read {}: {err}", fastly_path.display()));
            }
        };
        let doc: toml_edit::DocumentMut = raw
            .parse()
            .map_err(|err| format!("failed to parse {}: {err}", fastly_path.display()))?;
        // Probe `[local_server.config_stores.<name>]` — if absent, the store
        // has not been seeded locally yet.
        let Some(contents) = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(name))
            .and_then(|store_tbl| store_tbl.get("contents"))
        else {
            return Ok(ReadConfigEntry::MissingStore);
        };
        // The contents table is `key = "value"` pairs.
        match contents.get(key) {
            Some(item) => {
                let value = item.as_str().ok_or_else(|| {
                    format!(
                        "`[local_server.config_stores.{name}.contents].{key}` in {} is not a string",
                        fastly_path.display()
                    )
                })?;
                // Resolve chunk pointers using the same toml contents table.
                let resolved =
                    resolve_fastly_config_value(key, value.to_owned(), |chunk_key| match contents
                        .get(chunk_key)
                    {
                        Some(chunk_item) => {
                            let chunk_val = chunk_item.as_str().ok_or_else(|| {
                                format!(
                                    "chunk key `{chunk_key}` in {} is not a string",
                                    fastly_path.display()
                                )
                            })?;
                            Ok(Some(chunk_val.to_owned()))
                        }
                        None => Ok(None),
                    });
                match resolved {
                    Ok(body) => Ok(ReadConfigEntry::Present(body)),
                    // A corrupt/invalid prior value must NOT block a local push.
                    // The whole point of `config push --local` here is to
                    // OVERWRITE that broken state, and the local writer already
                    // fail-soft handles a suspicious prior pointer (overwrite,
                    // warn, prune nothing). Reporting `Unsupported` — "cannot
                    // diff against this" — lets the write proceed to that path
                    // instead of aborting the whole command on the diff read.
                    // (Local only: a single file we are about to replace. The
                    // cloud read keeps erroring, since we must not overwrite
                    // remote state we could not read.)
                    Err(_reason) => Ok(ReadConfigEntry::Unsupported(
                        "local prior value could not be resolved (corrupt or incomplete chunk \
                         state); it will be overwritten by this push",
                    )),
                }
            }
            None => Ok(ReadConfigEntry::MissingKey),
        }
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

/// Fetch a single entry value from a remote Fastly Config Store entry by
/// key, using `fastly config-store-entry describe --store-id=<id> --key=<k>
/// --json`. Used by the chunk-pointer resolver to fan out to chunk entries.
///
/// Returns:
/// - `Ok(Some(value))` when the entry exists.
/// - `Ok(None)` when the entry is absent (not-found / 404 / does not exist).
/// - `Err(...)` on adapter or parse errors.
///
/// # Errors
/// Returns an error if `fastly` isn't on `PATH`, spawning fails, the JSON
/// cannot be parsed, or the CLI exits with an unexpected non-zero status.
fn fetch_remote_config_store_entry(store_id: &str, key: &str) -> Result<Option<String>, String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "describe",
            store_arg.as_str(),
            key_arg.as_str(),
            "--json",
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|err| {
            format!(
                "failed to parse `fastly config-store-entry describe` JSON for key \
                 `{key}`: {err} (response: {})",
                redact_describe_response(&stdout)
            )
        })?;
        let value = parsed
            .get("item_value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry describe` JSON has no string `item_value` \
                     field for key `{key}`; fastly CLI may have changed its output schema. \
                     (response: {})",
                    redact_describe_response(&stdout)
                )
            })?;
        return Ok(Some(value.to_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not found") || lower.contains("does not exist") || lower.contains("404") {
        return Ok(None);
    }
    Err(format!(
        "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` \
         exited with status {}\nstderr: {}",
        output.status,
        redact_stderr(&stderr)
    ))
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

/// Write the local-server config-store entries to `fastly.toml`:
/// `[local_server.config_stores.<platform_name>]` becomes
/// `format = "inline-toml"`, and `[local_server.config_stores.<platform_name>.contents]`
/// gets the flat `key = "value"` pairs (overwriting any previous
/// values). Idempotent — re-running just rewrites `contents`. Other
/// blocks in `fastly.toml` (setup, scripts, the actual `[local_server]`
/// secret stores, etc.) are preserved via `toml_edit`.
fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
    gc_roots: &[(String, HashSet<String>)],
) -> Result<Vec<String>, String> {
    use toml_edit::{DocumentMut, Item, Table, Value, table};

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
    // Snapshot prior chunk keys per GC root BEFORE the upsert, using the
    // exact keep-set the caller computed for each root (no prefix scan).
    let mut plans: Vec<FastlyConfigGcPlan> = Vec::with_capacity(gc_roots.len());
    for (root_key, new_keys) in gc_roots {
        let prior_keys = contents_tbl
            .get(root_key)
            .and_then(toml_edit::Item::as_str)
            .map_or_else(|| Ok(Vec::new()), |value| prior_chunk_keys(root_key, value));
        plans.push(FastlyConfigGcPlan {
            new_keys: new_keys.clone(),
            prior_keys,
        });
    }

    // Upsert the new physical entries.
    for (key, value) in entries {
        contents_tbl.insert(key, Item::Value(Value::from(value.clone())));
    }

    // Prune orphans in the same in-memory rewrite; a suspicious prior
    // pointer (Err) warns and deletes nothing.
    let mut warnings = Vec::new();
    for plan in &plans {
        match orphan_chunk_keys(plan) {
            Ok(orphans) => {
                for key in orphans {
                    contents_tbl.remove(&key);
                }
            }
            Err(err) => warnings.push(format!("warning: {err}")),
        }
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(warnings)
}

// -------------------------------------------------------------------
// chunk GC helpers (Stage 7 re-push reclamation)
// -------------------------------------------------------------------

/// Expand ONE logical `(root_key, body)` into its physical entries, the
/// exact keep-set for that root, and the value written at the root key.
/// No cross-root prefix scanning (a free-form `--key` can't mislead it).
#[expect(
    clippy::type_complexity,
    reason = "one-off internal return; a named type would not aid readability"
)]
fn expand_root(
    root_key: &str,
    body: &str,
) -> Result<(Vec<(String, String)>, HashSet<String>, String), String> {
    let expanded = prepare_fastly_config_entries(root_key, body)?;
    let new_keys: HashSet<String> = expanded.iter().map(|(key, _)| key.clone()).collect();
    // prepare_* always emits the root entry LAST (root pointer or direct
    // value). Make the invariant explicit rather than silently defaulting.
    let new_root_value = expanded
        .last()
        .map(|(_, value)| value.clone())
        .ok_or_else(|| format!("internal: no physical entries produced for root `{root_key}`"))?;
    Ok((expanded, new_keys, new_root_value))
}

/// Orphans = prior chunk keys not in the new keep-set. Propagates a
/// suspicious-pointer `Err` so the caller can warn and skip GC.
fn orphan_chunk_keys(plan: &FastlyConfigGcPlan) -> Result<Vec<String>, String> {
    match &plan.prior_keys {
        Ok(prior) => Ok(prior
            .iter()
            .filter(|key| !plan.new_keys.contains(*key))
            .cloned()
            .collect()),
        Err(err) => Err(err.clone()),
    }
}

/// Reject logical keys that collide with the reserved chunk namespace.
/// `--key` is free-form, so this is enforced at the Fastly adapter
/// boundary: such a key would let a push write into another key's chunk
/// space, and could not be reclaimed correctly.
fn reject_reserved_root_keys(entries: &[(String, String)]) -> Result<(), String> {
    for (key, _) in entries {
        if key.contains(CHUNK_KEY_INFIX) {
            return Err(format!(
                "config key `{key}` contains the reserved infix `{CHUNK_KEY_INFIX}`, which collides with Fastly chunk storage; choose a different config key (or --key override)"
            ));
        }
    }
    Ok(())
}

/// Unix epoch seconds. Push-time only (the `cli` feature is native).
fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Reject a batch that names the same logical root key more than once.
///
/// The adapter trait takes an entry slice and does not enforce uniqueness,
/// but GC builds one plan per entry and snapshots every plan against the
/// SAME prior generation. With `[(root, A), (root, B)]` the last tuple wins
/// the upsert (root = B), yet A's plan would still reclaim `prior - A_keys`
/// — which includes B's freshly-written chunks — leaving the final pointer
/// referencing missing chunks. Rejecting is safer than silently coalescing:
/// a duplicated key is a caller bug, and picking a winner would hide it.
fn reject_duplicate_root_keys(entries: &[(String, String)]) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
    for (key, _) in entries {
        if !seen.insert(key.as_str()) {
            return Err(format!(
                "config key `{key}` appears more than once in a single push; each logical key must be pushed exactly once"
            ));
        }
    }
    Ok(())
}

/// Best-effort per-root orphan count for `config push --local --dry-run`.
/// Navigate to `[local_server.config_stores.<name>.contents]` for the
/// dry-run counter. `Ok(None)` when any level is absent (no prior state);
/// `Err` when a level is present but the wrong type — prior state the real
/// writer would reject, so the count must degrade to "unknown", not 0.
fn local_contents_table<'doc>(
    doc: &'doc toml_edit::DocumentMut,
    platform_name: &str,
) -> Result<Option<&'doc toml_edit::Table>, String> {
    let malformed = || "could not read prior state".to_owned();
    let Some(server_item) = doc.get("local_server") else {
        return Ok(None);
    };
    let Some(server) = server_item.as_table() else {
        return Err(malformed());
    };
    let Some(stores_item) = server.get("config_stores") else {
        return Ok(None);
    };
    let Some(stores) = stores_item.as_table() else {
        return Err(malformed());
    };
    let Some(store_item) = stores.get(platform_name) else {
        return Ok(None);
    };
    let Some(store) = store_item.as_table() else {
        return Err(malformed());
    };
    let Some(contents_item) = store.get("contents") else {
        return Ok(None);
    };
    contents_item
        .as_table()
        .map_or_else(|| Err(malformed()), |table| Ok(Some(table)))
}

/// Reads the current `fastly.toml` (offline) and, for each logical
/// `(root_key, body)`, counts `prior_chunk_keys(root, old) - new_keys`
/// where `new_keys` is the root's OWN expansion. Never fails the dry-run:
/// on a missing file / no prior pointer / direct prior value it reports
/// `Ok(0)`; on unreadable or malformed prior state it reports `Err(reason)`
/// which the caller renders as an "unknown" line.
fn local_orphan_counts_for_dry_run(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
) -> Vec<(String, Result<usize, String>)> {
    use toml_edit::DocumentMut;

    // Parse the current file once (best-effort). Absent file => no prior.
    let parsed: Result<Option<DocumentMut>, String> = match fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map(Some)
            .map_err(|_err| "could not read prior state".to_owned()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err("could not read prior state".to_owned()),
    };

    entries
        .iter()
        .map(|(root_key, body)| {
            let new_keys = match expand_root(root_key, body) {
                Ok((_, keys, _)) => keys,
                Err(err) => return (root_key.clone(), Err(err)),
            };
            let count = match &parsed {
                Err(reason) => Err(reason.clone()),
                Ok(None) => Ok(0),
                Ok(Some(doc)) => match local_contents_table(doc, platform_name) {
                    Err(reason) => Err(reason),
                    Ok(None) => Ok(0),
                    Ok(Some(contents)) => match contents.get(root_key) {
                        None => Ok(0), // no prior value for this root
                        Some(item) => match item.as_str() {
                            None => Err("could not read prior state".to_owned()),
                            Some(raw) => match prior_chunk_keys(root_key, raw) {
                                Ok(prior) => {
                                    Ok(prior.iter().filter(|key| !new_keys.contains(*key)).count())
                                }
                                Err(_) => Err("suspicious prior pointer".to_owned()),
                            },
                        },
                    },
                },
            };
            (root_key.clone(), count)
        })
        .collect()
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
/// `fastly config-store-entry list --store-id=<id> --json`, keeping only each
/// item's key and `created_at`. The item VALUE is discarded on purpose (it is
/// the stored config; see `redact_describe_response`).
fn list_config_store_entries(store_id: &str) -> Result<Vec<ConfigStoreItem>, String> {
    let store_arg = format!("--store-id={store_id}");
    let output = Command::new("fastly")
        .args(["config-store-entry", "list", store_arg.as_str(), "--json"])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`fastly config-store-entry list --store-id={store_id} --json` exited with status {}\nstderr: {}",
            output.status,
            redact_stderr(&stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|err| {
        format!(
            "failed to parse `fastly config-store-entry list` JSON: {err} (response: {})",
            redact_describe_response(&stdout)
        )
    })?;
    // A BARE ARRAY ONLY. The installed Fastly CLI returns the complete store as
    // a top-level array with no cursor/paging flags. Any other shape (e.g. an
    // `{"items":[...], ...}` envelope) may carry pagination metadata we do not
    // follow -- and a page that omitted a ROOT while listing its chunks would
    // make live chunks look orphaned. The completeness guard cannot see a root
    // that isn't there, so we refuse rather than reclaim from a partial view.
    let array = parsed.as_array().ok_or_else(|| {
        format!(
            "refusing to reclaim: `fastly config-store-entry list --json` did not return a bare \
             array (response: {}). This build only supports an unpaginated listing; a partial view \
             could hide a root and orphan its live chunks. Nothing was deleted.",
            redact_describe_response(&stdout)
        )
    })?;
    // FAIL CLOSED on any malformed entry. A missing/non-string field on a
    // reclamation input must NEVER be silently skipped or defaulted to empty:
    // skipping a root hides the chunks it references (they'd look orphaned and
    // get deleted while live), and an empty `item_value` makes a real root
    // parse as "references nothing" — same catastrophe. If we can't read the
    // listing exactly, we delete nothing.
    let mut items = Vec::with_capacity(array.len());
    for (idx, entry) in array.iter().enumerate() {
        let field = |name: &str| -> Result<String, String> {
            let raw = entry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "`fastly config-store-entry list` entry #{idx} is missing a string `{name}` \
                         field; refusing to reclaim on an unreadable listing (nothing deleted)"
                    )
                })?;
            // An EMPTY field is as dangerous as a missing one: an empty root
            // value would classify as "references nothing" and orphan its live
            // chunks. Reject it here rather than reason about it later.
            if raw.is_empty() {
                return Err(format!(
                    "`fastly config-store-entry list` entry #{idx} has an empty `{name}` field; \
                     refusing to reclaim on an unreadable listing (nothing deleted)"
                ));
            }
            Ok(raw.to_owned())
        };
        items.push(ConfigStoreItem {
            created_at: field("created_at")?,
            item_key: field("item_key")?,
            item_value: field("item_value")?,
        });
    }

    // DUPLICATE KEYS => fail closed. A key must appear once; a store cannot
    // really hold two entries under one key, so duplicate rows mean we are not
    // reading the store we think we are (a merged/paginated view, or a CLI
    // change). Left alone, the last row silently wins for BOTH the live-set
    // lookup and `created_at`, so conflicting rows could age a recent key into
    // eligibility and schedule the same key for two deletes.
    let mut seen: HashSet<&str> = HashSet::with_capacity(items.len());
    if let Some(duplicate) = items
        .iter()
        .find(|item| !seen.insert(item.item_key.as_str()))
    {
        return Err(format!(
            "refusing to reclaim: `fastly config-store-entry list` returned key `{}` more than \
             once. A key is unique in a config store, so this listing does not describe one \
             consistent view of it (nothing was deleted).",
            duplicate.item_key
        ));
    }

    Ok(items)
}

/// RFC 3339 (`2026-07-13T03:27:42Z`) -> unix seconds.
fn parse_rfc3339_secs(raw: &str) -> Option<u64> {
    let stamp = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    u64::try_from(stamp.timestamp()).ok()
}

/// `config gc` for Fastly: delete chunk entries that no LIVE root pointer
/// references and that are older than the operator's `older_than_secs`.
///
/// Why this is a separate, operator-invoked command rather than part of `config
/// push`: see `Adapter::gc_config_entries`. The operator's `--older-than` is the
/// safety assertion the platform cannot make. A dry-run prints exactly which
/// keys would go, with ages, so the assertion is reviewable.
///
/// Fails CLOSED: if the listing is unreadable, or a root's value cannot be
/// classified, nothing is deleted.
fn gc_fastly_config_store(
    store_name: &str,
    older_than_secs: u64,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    // THE destructive boundary enforces its own precondition. The CLI rejects a
    // zero window too, but `gc_config_entries` is a public trait method any
    // caller can reach directly -- a safety rule that lives only in the CLI is
    // not a safety rule. A zero window asserts nothing: it makes every orphan
    // eligible, including one superseded a second ago whose pointer POPs are
    // still serving. (A dry-run may preview at zero; it deletes nothing.)
    if !dry_run && older_than_secs == 0 {
        return Err(
            "refusing to reclaim: a destructive `config gc` requires a non-zero `--older-than` \
             window. Zero asserts nothing -- it would make every orphan eligible, including \
             chunks a pointer POPs are still serving. Nothing was deleted."
                .to_owned(),
        );
    }
    let resolved_id = resolve_remote_config_store_id(store_name)?;
    let items = list_config_store_entries(&resolved_id)?;
    let plan = plan_gc_reclamation(&items, unix_now_secs(), older_than_secs)?;
    let GcPlan {
        doomed,
        live_count,
        retained_recent,
        roots,
        unprovable,
    } = plan;

    let doomed_count: usize = doomed.iter().map(Vec::len).sum();
    let mut out = vec![format!(
        "fastly config-store `{store_name}` (id={resolved_id}): {} entries, {roots} root(s), {live_count} live chunk(s), {doomed_count} orphan(s) in {} generation(s) older than {older_than_secs}s, {retained_recent} orphan(s) too recent",
        items.len(),
        doomed.len(),
    )];
    if unprovable > 0 {
        // NEVER silent: these entries look like chunk keys but we could not
        // prove our writer produced them, so we left them alone. Say so, or the
        // summary reads as "everything reclaimable was reclaimed".
        out.push(format!(
            "  {unprovable} chunk-shaped entr(ies) left untouched: they are not byte-identical to what this writer would produce (wrong content-address, a split this writer would not choose, an incomplete generation, or a count it would never emit), so EdgeZero cannot claim them"
        ));
    }
    if doomed_count == 0 {
        out.push("nothing to reclaim".to_owned());
        return Ok(out);
    }
    for (key, age) in doomed.iter().flatten() {
        let verb = if dry_run { "would delete" } else { "deleting" };
        out.push(format!("  {verb} `{key}` (age {age}s)"));
    }
    if dry_run {
        out.push(format!(
            "dry-run: {doomed_count} orphan chunk(s) would be deleted; re-run with --yes to apply"
        ));
        return Ok(out);
    }

    let GcDeleteOutcome {
        deleted,
        failed,
        stranded,
        uncertain,
    } = execute_gc_deletes(&resolved_id, &doomed, &mut out);
    out.push(format!(
        "reclaimed {deleted} of {doomed_count} orphan chunk entries"
    ));
    if failed.is_empty() {
        return Ok(out);
    }
    // Partial/total failure must be a non-zero exit so automation can see it.
    let mut diagnostic = format!(
        "{}\nconfig gc: {} of {doomed_count} deletes FAILED ({})",
        out.join("\n"),
        failed.len(),
        failed.join(", ")
    );
    // A generation whose only failure was on an unconfirmed delete: the outcome
    // is UNKNOWN (Fastly may have committed it), so a re-run is worth trying but
    // may find a fragment.
    if !uncertain.is_empty() {
        write!(
            diagnostic,
            ".\nNOTE: a failed remote delete has an unknown outcome -- Fastly may have applied it \
             before returning an error. Re-run `config gc`: it reclaims each affected generation \
             if it is still whole, or reports it as an unprovable fragment (\"left untouched\") if \
             a delete did commit. If reported as a fragment, remove the survivors by hand:\n{}",
            recovery_commands(&resolved_id, &uncertain)
        )
        .map_err(|err| format!("failed to format the gc diagnostic: {err}"))?;
    }
    // A generation with a CONFIRMED prior delete: definitely a fragment now.
    if !stranded.is_empty() {
        write!(
            diagnostic,
            ".\nWARNING: {} entr(ies) are now an INCOMPLETE generation because a sibling was \
             already deleted before the failure: {}. `config gc` proves a generation by \
             reassembling it, so it can no longer prove these and will never reclaim them -- \
             re-running will NOT help. They are inert (no pointer references them). Remove them \
             by hand once you are satisfied they are unreferenced:\n{}",
            stranded.len(),
            stranded.join(", "),
            recovery_commands(&resolved_id, &stranded),
        )
        .map_err(|err| format!("failed to format the gc diagnostic: {err}"))?;
    }
    Err(diagnostic)
}

/// Render copy-pasteable `fastly config-store-entry delete` commands, one per
/// key, with EVERY interpolated value single-quoted for POSIX shells.
///
/// Root keys are free-form (`--key <override>`), and a chunk key preserves its
/// root, so a key can contain `$(...)`, spaces, or `;`. Pasting an unquoted
/// command could execute or misparse it, so this is not cosmetic.
fn recovery_commands(store_id: &str, keys: &[String]) -> String {
    keys.iter()
        .map(|key| {
            format!(
                "  fastly config-store-entry delete --store-id={} --key={} --auto-yes",
                shell_single_quote(store_id),
                shell_single_quote(key),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Single-quote a value for a POSIX shell: wrap in `'...'` and rewrite each
/// embedded `'` as `'\''`. Inside single quotes every other byte -- `$`, spaces,
/// `;`, `$(...)`, backticks -- is literal, so this neutralises any hostile key.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Delete each doomed generation, stopping a generation at its FIRST failure.
///
/// A generation is provable only as a whole (`prove_generation` reassembles it),
/// so a half-deleted one can never be proved again: the next run sees a fragment,
/// cannot verify it, and correctly refuses to touch it — forever. Ploughing on
/// after a failure is therefore the one thing that turns a possibly-recoverable
/// error into permanent, unreclaimable litter.
///
/// A failed remote delete has an UNKNOWN outcome — Fastly may commit it before
/// returning an error — so nothing here is promised as cleanly retryable. The
/// caller distinguishes two cases: a failure with a CONFIRMED prior sibling
/// delete strands the survivors for good (manual recovery), and a failure with
/// no confirmed prior delete leaves the generation in an UNCERTAIN state (a
/// re-run may reclaim it, or surface it as an unprovable fragment). Generations
/// are independent, so a failure in one does not stop the others.
fn execute_gc_deletes(
    resolved_id: &str,
    doomed: &[Vec<(String, u64)>],
    out: &mut Vec<String>,
) -> GcDeleteOutcome {
    let mut outcome = GcDeleteOutcome {
        deleted: 0,
        failed: Vec::new(),
        stranded: Vec::new(),
        uncertain: Vec::new(),
    };
    for generation in doomed {
        let mut deleted_here: Vec<&str> = Vec::new();
        for (key, _) in generation {
            match delete_config_store_entry(resolved_id, key) {
                Ok(()) => {
                    outcome.deleted = outcome.deleted.saturating_add(1);
                    deleted_here.push(key.as_str());
                }
                Err(err) => {
                    out.push(format!("  FAILED to delete `{key}` ({err})"));
                    outcome.failed.push(key.clone());
                    // Everything in this generation we have NOT confirmed deleted
                    // -- the failed key itself, plus the ones we never reached.
                    let unconfirmed: Vec<String> = generation
                        .iter()
                        .map(|(member, _)| member.clone())
                        .filter(|member| !deleted_here.contains(&member.as_str()))
                        .collect();
                    if deleted_here.is_empty() {
                        // No sibling is CONFIRMED gone. The failed delete's
                        // outcome is unknown: if it did not commit, the
                        // generation is whole and a re-run reclaims it; if it
                        // did, the re-run finds a fragment and reports it. Either
                        // way we must not claim clean retryability.
                        outcome.uncertain.extend(unconfirmed);
                    } else {
                        // A sibling is CONFIRMED gone, so this generation is
                        // definitely a fragment no future run can prove.
                        outcome.stranded.extend(unconfirmed);
                    }
                    break; // stop THIS generation; the others are independent
                }
            }
        }
    }
    outcome
}

/// Classify a store's entries: the live chunk set, the protected root keys, and
/// the root count.
///
/// Root-vs-chunk is decided by VALUE, not key shape. The runtime resolver reads
/// whatever value sits at a key, so ANY entry whose value is a valid direct
/// envelope or a chunk pointer is a runtime-readable root and must never be
/// deleted — even at a chunk-shaped key. Two ways that happens:
///
/// - a pointer parked at a chunk-shaped key makes its references LIVE;
/// - a value that is itself a valid direct envelope (e.g. a small envelope whose
///   first 7 000-byte chunk is the whole envelope plus trailing whitespace, and
///   so still parses and verifies) is a root in its own right.
///
/// Only a value that is NEITHER — a raw envelope fragment, which does not parse —
/// is a delete candidate. In normal operation a chunk payload is exactly such a
/// fragment, so this protects the pathological cases at no cost to real GC.
fn classify_store_entries(
    items: &[ConfigStoreItem],
    value_by_key: &HashMap<&str, &str>,
) -> Result<GcClassification, String> {
    let mut live: HashSet<String> = HashSet::new();
    let mut protected: HashSet<String> = HashSet::new();
    let mut roots = 0_usize;
    for item in items {
        let is_chunk_shaped = chunk_key_generation_any(&item.item_key).is_some();
        let classified = match gc_classify_root(&item.item_key, &item.item_value) {
            Ok(classified) => classified,
            // A chunk-shaped key whose value we cannot classify is a genuine
            // chunk fragment (a candidate) ONLY if that value is not itself
            // root-like. A pointer-kind value is always root-like — the runtime
            // reads it as a pointer — so an unclassifiable one (e.g. a
            // cross-root pointer that fails this root's scope check) must FAIL
            // CLOSED, never become a deletable candidate whose references we
            // would orphan.
            Err(_) if is_chunk_shaped && !value_is_pointer_kind(&item.item_value) => {
                continue; // a chunk payload: a delete candidate
            }
            Err(err) => {
                return Err(format!(
                    "refusing to reclaim: could not classify root `{}` ({err}); nothing was deleted",
                    item.item_key
                ));
            }
        };
        // A runtime-readable root, wherever it lives: never a delete candidate.
        roots = roots.saturating_add(1);
        protected.insert(item.item_key.clone());
        let GcRootValue::Chunked(pointer) = classified else {
            continue; // A direct envelope references no chunks.
        };
        // The pointer's METADATA is self-consistent by here. That is not proof
        // that it honestly describes its generation: a pointer can drop its last
        // chunk ref AND restate `envelope_len` as the remaining sum, and every
        // metadata check still passes while the dropped chunk silently leaves
        // the live set and becomes deletable. So reassemble what it references
        // and hold the bytes against its content-address.
        let assembled = assemble_pointer_chunks(&item.item_key, &pointer, value_by_key)?;
        gc_verify_generation(&pointer.envelope_sha256, &assembled).map_err(|err| {
            format!(
                "refusing to reclaim: root `{}` names a chunk set that does not reconstruct the \
                 envelope it claims ({err}). Its chunk list is therefore not a trustworthy live \
                 set, and treating it as one could delete a live chunk. Nothing was deleted.",
                item.item_key
            )
        })?;
        live.extend(pointer.chunks.into_iter().map(|chunk| chunk.key));
    }
    Ok(GcClassification {
        live,
        protected,
        roots,
    })
}

/// The reclamation plan for one store: which orphan chunk entries to delete, and
/// the counts for the summary line. Deriving it is where every safety guard
/// lives, so it is fail-closed throughout — any unreadable/incomplete state
/// returns `Err` and the caller deletes nothing.
///
/// The organising idea is that **content-addressing makes a chunk set
/// self-proving**: a chunk key embeds the SHA-256 of the whole envelope it
/// belongs to, so reassembling a generation either reproduces the
/// content-address its own keys name, or it does not. Every destructive decision
/// here rests on that hash — never on what the store's metadata claims about
/// itself, which is exactly what an inconsistent store gets wrong.
fn plan_gc_reclamation(
    items: &[ConfigStoreItem],
    now: u64,
    older_than_secs: u64,
) -> Result<GcPlan, String> {
    let mut value_by_key: HashMap<&str, &str> = HashMap::with_capacity(items.len());
    let mut created_by_key: HashMap<&str, u64> = HashMap::with_capacity(items.len());
    for item in items {
        let Some(created) = parse_rfc3339_secs(&item.created_at) else {
            // Unparseable timestamp anywhere in the listing -> fail closed. On a
            // DELETE path we will not guess an age.
            return Err(format!(
                "refusing to reclaim: entry `{}` has an unreadable `created_at`; nothing was deleted",
                item.item_key
            ));
        };
        created_by_key.insert(item.item_key.as_str(), created);
        value_by_key.insert(item.item_key.as_str(), item.item_value.as_str());
    }

    // ---- 1. Classify entries: live chunks, protected roots, root count ----
    let GcClassification {
        live,
        protected,
        roots,
    } = classify_store_entries(items, &value_by_key)?;

    // ---- 2. Per-root live-config age (best-effort; see the guard below) ----
    let root_live_since: HashMap<&str, u64> = live.iter().fold(HashMap::new(), |mut acc, key| {
        if let Some((root, _)) = key.split_once(CHUNK_KEY_INFIX) {
            let created = *created_by_key.get(key.as_str()).unwrap_or(&0);
            let slot = acc.entry(root).or_insert(0);
            *slot = (*slot).max(created);
        }
        acc
    });

    // ---- 3. Candidates, grouped by GENERATION and proven writer-produced ----
    // A per-key decision cannot be safe: an entry is only ours if the whole
    // generation it belongs to reassembles to the content-address its keys name.
    // So group first, prove second, and delete whole generations or none -- a
    // partial delete would leave a corrupt generation behind.
    let mut groups: BTreeMap<(&str, String), Vec<&ConfigStoreItem>> = BTreeMap::new();
    for item in items {
        if live.contains(&item.item_key) {
            continue;
        }
        // A key whose own value is a runtime-readable root is never a candidate,
        // even when its key is chunk-shaped (a valid direct envelope can sit at
        // one). Excluding it here also means any real chunk sharing that
        // generation drops to an incomplete group, which prove_generation then
        // leaves untouched — safe: we leak rather than delete a possible root.
        if protected.contains(&item.item_key) {
            continue;
        }
        let Some((root, _)) = item.item_key.split_once(CHUNK_KEY_INFIX) else {
            continue; // a root
        };
        let Some(generation) = chunk_key_generation(root, &item.item_key) else {
            continue; // chunk-shaped but NOT canonical => never a key we emit
        };
        groups.entry((root, generation)).or_default().push(item);
    }

    let mut doomed: Vec<Vec<(String, u64)>> = Vec::new();
    let mut retained_recent = 0_usize;
    let mut unprovable = 0_usize;
    for ((root, generation), group) in groups {
        if prove_generation(root, &generation, &group).is_err() {
            // We cannot prove we wrote this, so we do not touch it. It may be an
            // ordinary entry that merely LOOKS like a chunk key (a store can
            // predate this feature or be shared, and push-time reserved-key
            // rejection cannot protect what already exists), or a half-written
            // generation. Skipped rather than fatal: one foreign entry must not
            // block reclamation of the store forever. Reported in the summary.
            unprovable = unprovable.saturating_add(group.len());
            continue;
        }

        // Age the generation as a UNIT, by its youngest member: deleting a
        // generation is one decision, so its most restrictive age governs.
        let group_age = group
            .iter()
            .map(|item| {
                now.saturating_sub(*created_by_key.get(item.item_key.as_str()).unwrap_or(&0))
            })
            .min()
            .unwrap_or(0);
        // BOTH ages must clear the operator's window; neither substitutes for
        // the other, so take the more restrictive (the MINIMUM).
        //
        // - The chunks' OWN age is mandatory: a generation written seconds ago
        //   is inside the propagation window whatever its root looks like (e.g.
        //   a concurrent push wrote it and has not committed its pointer yet),
        //   so an old-looking root must never license deleting it.
        // - The root's live-config age (when known) is an EXTRA restriction: it
        //   catches an old generation superseded recently, which its own age
        //   cannot see.
        let effective_age = root_live_since.get(root).map_or(group_age, |live_since| {
            group_age.min(now.saturating_sub(*live_since))
        });
        if effective_age < older_than_secs {
            retained_recent = retained_recent.saturating_add(group.len());
            continue;
        }
        doomed.push(
            group
                .iter()
                .map(|item| {
                    let age = now
                        .saturating_sub(*created_by_key.get(item.item_key.as_str()).unwrap_or(&0));
                    (item.item_key.clone(), age)
                })
                .collect(),
        );
    }

    Ok(GcPlan {
        doomed,
        live_count: live.len(),
        retained_recent,
        roots,
        unprovable,
    })
}

/// Reassemble the chunks a live pointer references, in index order, checking each
/// against the pointer's own per-chunk `len`/`sha256` along the way.
///
/// Fails closed when a referenced key is absent from the listing. This subsumes
/// the old standalone completeness guard: an incomplete or paginated listing
/// cannot produce the bytes, so it can never reach a passing verification.
fn assemble_pointer_chunks(
    root_key: &str,
    pointer: &GcPointer,
    value_by_key: &HashMap<&str, &str>,
) -> Result<String, String> {
    // NOT `with_capacity(pointer.envelope_len)`: that length is untrusted stored
    // metadata. `validate_pointer_chunks` bounds it, but this is a destructive
    // path -- do not reserve from a number the store supplied when growing from
    // the bytes we actually read costs nothing.
    let mut assembled = String::new();
    for chunk in &pointer.chunks {
        let Some(value) = value_by_key.get(chunk.key.as_str()) else {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` references `{}`, which is absent from the \
                 store listing (the listing may be incomplete/paginated, or the store is already \
                 inconsistent); nothing was deleted",
                chunk.key
            ));
        };
        if value.len() != chunk.len {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` says `{}` is {} bytes but the store holds \
                 {}; nothing was deleted",
                chunk.key,
                chunk.len,
                value.len()
            ));
        }
        if sha256_hex(value.as_bytes()) != chunk.sha256 {
            return Err(format!(
                "refusing to reclaim: the stored value of `{}` does not match the SHA-256 that \
                 root `{root_key}` records for it; nothing was deleted",
                chunk.key
            ));
        }
        assembled.push_str(value);
    }
    if assembled.len() != pointer.envelope_len {
        return Err(format!(
            "refusing to reclaim: root `{root_key}` declares an envelope of {} bytes but its \
             chunks reassemble to {}; nothing was deleted",
            pointer.envelope_len,
            assembled.len()
        ));
    }
    Ok(assembled)
}

/// Is this candidate generation byte-identical to what THIS writer would have
/// produced for the bytes it contains?
///
/// The gate on every delete. `group` is every listed entry sharing one
/// `(root, generation)`.
///
/// **What this proves, precisely.** We reassemble the group in index order and
/// re-run `prepare_fastly_config_entries` over the result. If the writer, given
/// those exact bytes, would emit exactly these keys and these values, the entries
/// are indistinguishable from our own output: same direct-vs-chunked threshold,
/// same UTF-8-safe 7 000-byte boundaries, same content-addressed keys, same
/// count. A lone chunk fails automatically (an envelope small enough to store
/// directly round-trips to a single ROOT-keyed entry, and a large one to >= 2
/// chunks), as does any set split at boundaries we would not choose.
///
/// **What this does NOT prove: authorship.** Content-addressing is not a
/// signature. A foreign writer can pick envelope E, compute `H = sha256(E)`,
/// split E exactly as we would, and store the parts under our reserved
/// `.__edgezero_chunks.` namespace; that group is byte-identical to ours and we
/// will reclaim it. No preimage attack is needed, and no check over the stored
/// bytes alone can separate the two — telling them apart needs trusted
/// generation metadata or an authenticated marker, and the store offers neither
/// (any writer with store access could forge either).
///
/// We accept that residual: the namespace is reserved by convention, push-time
/// validation rejects logical keys inside it, and anything passing this gate is
/// a faithful reproduction of our format. The spec documents it as a limitation
/// rather than claiming a guarantee we cannot make.
fn prove_generation(
    root: &str,
    generation: &str,
    group: &[&ConfigStoreItem],
) -> Result<(), String> {
    let mut ordered: Vec<(usize, &str)> = Vec::with_capacity(group.len());
    for item in group {
        let index = item
            .item_key
            .rsplit_once('.')
            .and_then(|(_, index)| index.parse::<usize>().ok())
            .ok_or_else(|| format!("`{}` has no readable index", item.item_key))?;
        ordered.push((index, item.item_value.as_str()));
    }
    ordered.sort_by_key(|&(index, _)| index);
    for (position, &(index, _)) in ordered.iter().enumerate() {
        if index != position {
            return Err(format!(
                "indexes are not dense 0..n-1 (found {index} at position {position})"
            ));
        }
    }
    let assembled: String = ordered.iter().map(|&(_, value)| value).collect();

    // 1. The bytes must be the generation the keys name, and a real envelope.
    gc_verify_generation(generation, &assembled)?;

    // 2. ...and the writer, given those bytes, must produce EXACTLY these
    //    entries. This is what pins the split boundaries and the chunked-vs-
    //    direct threshold, so a set assembled by anything that does not
    //    reproduce our writer's output byte-for-byte is left alone.
    let expected = prepare_fastly_config_entries(root, &assembled)
        .map_err(|err| format!("this writer could not re-derive the generation ({err})"))?;
    let Some(expected_chunks) = expected.get(..expected.len().saturating_sub(1)) else {
        return Err("this writer produced no chunk entries for these bytes".to_owned());
    };
    if expected_chunks.is_empty() {
        // The envelope fits directly, so the writer would never have chunked it:
        // whatever these entries are, they are not ours.
        return Err(
            "these bytes fit the entry limit, so this writer would have stored them directly \
             rather than in chunks"
                .to_owned(),
        );
    }
    if expected_chunks.len() != ordered.len() {
        return Err(format!(
            "this writer would split these bytes into {} chunk(s), not {}",
            expected_chunks.len(),
            ordered.len()
        ));
    }
    for ((expected_key, expected_value), item) in
        expected_chunks.iter().zip(group_in_index_order(group))
    {
        if *expected_key != item.item_key {
            return Err(format!(
                "this writer would not have produced the key `{}`",
                item.item_key
            ));
        }
        if *expected_value != item.item_value {
            return Err(format!(
                "the stored value of `{}` is not the chunk this writer would have written at that \
                 index",
                item.item_key
            ));
        }
    }
    Ok(())
}

/// `group` sorted by chunk index, so it lines up with the writer's output order.
fn group_in_index_order<'item>(group: &[&'item ConfigStoreItem]) -> Vec<&'item ConfigStoreItem> {
    let mut ordered: Vec<&ConfigStoreItem> = group.to_vec();
    ordered.sort_by_key(|item| {
        item.item_key
            .rsplit_once('.')
            .and_then(|(_, index)| index.parse::<usize>().ok())
            .unwrap_or(usize::MAX)
    });
    ordered
}

/// Is this key a chunk key of ANY root? (`config gc` scans the whole store, so
/// it cannot scope to one root up front.) Validates the canonical shape.
fn chunk_key_generation_any(key: &str) -> Option<String> {
    let (root, _rest) = key.split_once(CHUNK_KEY_INFIX)?;
    chunk_key_generation(root, key)
}

/// Drive a sequential per-entry commit loop and produce the
/// partial-failure diagnostic when the committer fails mid-way.
/// Pure (no I/O) so the diagnostic shape is unit-testable without
/// the fastly CLI on PATH; production calls it with a closure that
/// shells out via `create_config_store_entry`. On success returns
/// the count of committed entries; on failure returns an error
/// string naming committed / failed / not-attempted keys so the
/// operator can resume from a known boundary.
fn push_entries_with_committer<F>(
    entries: &[(String, String)],
    mut committer: F,
) -> Result<usize, String>
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    let mut pushed: Vec<String> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if let Err(err) = committer(key, value) {
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
    Ok(pushed.len())
}

/// Shell `fastly config-store-entry update --upsert --stdin` with
/// the value piped through stdin instead of `--value=<value>` on
/// argv.
///
/// Two reasons for this exact invocation:
///
/// 1. `--upsert` (vs. the original `create` subcommand): the prior
///    `create` form errored on any key that already existed in the
///    config store, which made `config push` non-repeatable —
///    after the first push, every follow-up push triggered by a
///    config edit would fail at the first unchanged key.
///    `update --upsert` is documented as "insert or update", which
///    matches the convergent semantic the other config-push paths
///    already have (axum overwrites the JSON, cloudflare's
///    `wrangler kv bulk put` overwrites, spin's
///    `cloud key-value set` overwrites).
///
/// 2. `--stdin` (vs. `--value=<value>`): `--value=` exposed every
///    config entry's bytes in `ps`/`/proc/<pid>/cmdline` listings
///    AND was bounded by the host's `ARG_MAX` (4 KiB to 256 KiB
///    depending on platform — easy to trip with a JSON blob).
///    `--stdin` reads the value from stdin instead — keeps value
///    bytes out of argv and lifts the size cap to whatever the OS
///    pipe buffer + the CLI's read accept (megabytes in practice).
fn create_config_store_entry(store_id: &str, key: &str, value: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let mut child = Command::new("fastly")
        .args([
            "config-store-entry",
            "update",
            store_arg.as_str(),
            key_arg.as_str(),
            "--upsert",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    // Move stdin OUT of child via `take` so the ChildStdin drops at
    // end of scope — that closes the pipe and lets the CLI see EOF.
    // `child.wait_with_output()` then consumes child cleanly.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `fastly`".to_owned())?;
    stdin
        .write_all(value.as_bytes())
        .map_err(|err| format!("failed to write value to `fastly` stdin: {err}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `fastly`: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry update --store-id={store_id} --key={key} --upsert --stdin` exited with status {}\nstderr: {}",
        output.status,
        redact_stderr(&String::from_utf8_lossy(&output.stderr))
    ))
}

fn delete_config_store_entry(store_id: &str, key: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "delete",
            store_arg.as_str(),
            key_arg.as_str(),
            "--auto-yes",
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
    // EVERY non-zero delete is a failure -- no "already gone" special case.
    // Pattern-matching stderr for "not found"/"404" cannot reliably tell "this
    // key is already gone" from "the store does not exist", an auth failure, or
    // a 500: messages like `config store abc does not exist while deleting key
    // <key>` name the key AND say "does not exist". Reporting those as a
    // successful reclamation is strictly worse than a retry, and a retry is
    // free: `config gc` re-lists the store, so a key that really is gone simply
    // will not appear as a candidate next run.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry delete --store-id={store_id} --key={key} --auto-yes` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
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

/// Summarise a `fastly ... describe` response for diagnostics WITHOUT
/// leaking its contents.
///
/// The response body is the stored config value. App config may hold
/// credentials, internal endpoints, or security policy, and this adapter
/// performs no secret stripping — while CLI status lines are logged
/// verbatim and CI logs are commonly retained and shared. So a schema-drift
/// diagnostic must never echo the payload: report only its size and its
/// top-level *shape* (field names for an object, type otherwise), never a
/// value.
fn redact_describe_response(stdout: &str) -> String {
    let len = stdout.len();
    serde_json::from_str::<serde_json::Value>(stdout).map_or_else(
        |_err| format!("{len} bytes, not valid JSON"),
        |value| match value {
            serde_json::Value::Object(map) => {
                let mut names: Vec<&str> = map.keys().map(String::as_str).collect();
                names.sort_unstable();
                format!(
                    "{len} bytes, JSON object with fields [{}]",
                    names.join(", ")
                )
            }
            other @ (serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
            | serde_json::Value::Array(_)) => {
                format!("{len} bytes, JSON {}", shape_summary(&other))
            }
        },
    )
}

/// Summarise a failing `fastly` invocation's stderr WITHOUT echoing it.
///
/// The `describe` and `update --stdin` paths carry the stored config value, so
/// a Fastly error that quotes the payload back would put credentials straight
/// into CI logs — the same exposure as the stdout leak, via the failure branch.
/// Not-found *classification* still inspects stderr internally; only the
/// user-facing string is redacted.
fn redact_stderr(stderr: &str) -> String {
    let len = stderr.trim().len();
    format!(
        "{len} bytes suppressed (may echo the stored config value); re-run the `fastly` command directly to inspect it"
    )
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
    #[cfg(unix)]
    use edgezero_core::test_env::PathPrepend;
    use std::collections::HashSet;

    #[cfg(unix)]
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` per call site) keeps the
    // setup-vs-assertion pair in sync -- a typo in one place no
    // longer silently divorces from the other, because both reference
    // the same const. Also names the intent: these are the LOGICAL
    // store ids the fastly adapter operates on, not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

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

    // ---------- push_entries_with_committer ----------

    #[test]
    fn push_entries_with_committer_returns_count_when_all_succeed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let pushed = push_entries_with_committer(&entries, |_, _| Ok(())).expect("all succeed");
        assert_eq!(pushed, 3);
    }

    #[test]
    fn push_entries_with_committer_zero_entries_is_ok() {
        let pushed = push_entries_with_committer(&[], |_, _| Ok(())).expect("empty is fine");
        assert_eq!(pushed, 0);
    }

    #[test]
    fn push_entries_with_committer_failure_surfaces_committed_failed_not_attempted() {
        // Mock committer: succeed for first 2 keys, fail at third.
        let entries = vec![
            ("k1".to_owned(), "v1".to_owned()),
            ("k2".to_owned(), "v2".to_owned()),
            ("k3".to_owned(), "v3".to_owned()),
            ("k4".to_owned(), "v4".to_owned()),
            ("k5".to_owned(), "v5".to_owned()),
        ];
        let mut calls: usize = 0;
        let err = push_entries_with_committer(&entries, |key, _| {
            calls = calls.saturating_add(1);
            if key == "k3" {
                Err("simulated fastly stderr".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("middle failure must error");
        // Committer was invoked for k1, k2, k3 and stopped.
        assert_eq!(calls, 3_usize, "no retries beyond failure point");
        // Error names all three categories.
        assert!(err.contains("k1") && err.contains("k2"), "committed: {err}");
        assert!(
            err.contains("Failed: `k3`"),
            "failed entry named exactly: {err}"
        );
        assert!(
            err.contains("k4") && err.contains("k5"),
            "not-attempted: {err}"
        );
        assert!(err.contains("simulated fastly stderr"), "inner err: {err}");
        // Counts are sane.
        assert!(
            err.contains("committing 2 of 5 entries"),
            "committed/total count: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_first_entry_failure_reports_zero_committed() {
        let entries = vec![
            ("only".to_owned(), "val".to_owned()),
            ("never".to_owned(), "tried".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |_, _| Err("nope".to_owned()))
            .expect_err("first-entry failure");
        assert!(err.contains("committing 0 of 2"), "zero committed: {err}");
        assert!(
            err.contains("Failed: `only`"),
            "first-entry failure named: {err}"
        );
        assert!(
            err.contains("never"),
            "second entry as not-attempted: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_last_entry_failure_reports_n_minus_one_committed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |key, _| {
            if key == "c" {
                Err("late failure".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("last-entry failure");
        assert!(err.contains("committing 2 of 3"), "n-1 committed: {err}");
        assert!(
            err.contains("the remaining 0 entries"),
            "zero not-attempted when last fails: {err}"
        );
    }

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

    // ---------- write_fastly_local_config_store (config push --local) ----------

    #[test]
    fn write_fastly_local_config_store_creates_inline_block_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        write_fastly_local_config_store(&path, TEST_CONFIG_ID, &entries, &[]).expect("write");
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
            &[],
        )
        .expect("first write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "fresh".to_owned())],
            &[],
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
            &[],
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
            &[],
        )
        .expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains(&format!(
            "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
        )));
        assert!(after.contains("greeting = \"hi\""));
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
            .provision(dir.path(), Some("fastly.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        // 1 KV + 1 config + 1 secret + 1 runtime-env = 4 status lines.
        assert_eq!(out.len(), 4);
        assert!(out[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(out[1].contains("would run `fastly config-store create --name=app_config`"));
        assert!(out[2].contains("would run `fastly secret-store create --name=default`"));
        assert!(
            out[3].contains("would run `fastly config-store create --name=edgezero_runtime_env`"),
            "runtime-env store row: {out:?}",
        );
        // Manifest untouched.
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated fastly.toml");
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
            .provision(dir.path(), Some("fastly.toml"), None, &stores, false)
            .expect("skip path succeeds without invoking fastly");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("already declared"), "got: {out:?}");
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

    // ---------- find_config_store_id ----------

    #[test]
    fn find_config_store_id_matches_bare_array_by_name() {
        let stdout = format!(
            r#"[
                {{"id": "abc123", "name": "{TEST_CONFIG_ID}"}},
                {{"id": "def456", "name": "other_store"}}
            ]"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "abc123"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_tolerates_items_envelope() {
        let stdout = format!(
            r#"{{"items": [
                {{"id": "xyz789", "name": "{TEST_CONFIG_ID}"}}
            ]}}"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
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
        let stdout = format!(r#"[{{"title": "{TEST_CONFIG_ID}", "uid": "abc"}}]"#);
        let drift = find_config_store_id(&stdout, TEST_CONFIG_ID);
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
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        // First line names the resolve+publish flow; then one preview line per
        // key. A push no longer reclaims anything (see `config gc`), so there is
        // no GC-intent line.
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would resolve fastly config-store `app_config`")
                && out[0].contains("push entries"),
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
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "status line names the no-op: {out:?}"
        );
    }

    // ---------- read_config_entry_local ----------

    #[test]
    fn read_local_returns_missing_store_when_fastly_toml_absent() {
        let dir = tempdir().expect("tempdir");
        // No fastly.toml written — file missing.
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing file is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "absent fastly.toml => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_store_when_no_local_server_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // fastly.toml exists but has no [local_server.config_stores.*] block.
        fs::write(&path, "name = \"demo\"\n[setup.config_stores.app_config]\n").expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing local_server block is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "no local_server stanza => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_key_when_key_absent_from_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Write a local_server block with a different key so the store exists
        // but the requested key is absent.
        fs::write(
            &path,
            format!(
                "name = \"demo\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}]\n\
                 format = \"inline-toml\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}.contents]\n\
                 other_key = \"other_value\"\n"
            ),
        )
        .expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing key is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "key absent from contents => MissingKey"
        );
    }

    #[test]
    fn read_local_returns_present_when_key_exists_in_contents() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write initial toml");

        // Use a valid BlobEnvelope value — the resolver requires BlobEnvelope
        // or chunk-pointer JSON; raw strings are not accepted post-chunking.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), envelope_json.clone())],
            &[],
        )
        .expect("setup write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("key present");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, envelope_json, "value matches what was written");
    }

    #[test]
    fn read_local_roundtrips_with_push_local() {
        // Write via push_config_entries_local, then read via
        // read_config_entry_local — the two must agree on the value.
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");

        // push_config_entries_local passes the value through the chunk-pointer
        // helper which stores it verbatim when ≤ 8 000 chars. The reader then
        // resolves it through the same resolver that requires BlobEnvelope JSON.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "roundtrip"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entries = vec![("greeting".to_owned(), envelope_json.clone())];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("read succeeds");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present after push+read roundtrip");
        };
        assert_eq!(value, envelope_json, "roundtrip value matches");
    }

    #[test]
    fn read_local_requires_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let result = FastlyCliAdapter.read_config_entry_local(
            dir.path(),
            None, // adapter_manifest_path missing
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("[adapters.fastly.adapter].manifest"),
                "error names the missing field: {err}"
            ),
            Ok(_) => panic!("expected Err when adapter_manifest_path is None"),
        }
    }

    // ---------- read_config_entry (fake fastly, remote shell-out) ----------

    /// Build a tempdir containing a `fastly` shim script that:
    /// - Responds to `config-store list --json` with a store-list JSON containing
    ///   `TEST_CONFIG_ID` mapped to `store-abc123`.
    /// - Responds to `config-store-entry describe ...` with `stdout_body` on
    ///   stdout and `stderr_body` on stderr, exiting with `exit_code`.
    ///
    /// Payloads are written to separate sibling files so shell-active chars
    /// in the content don't get re-interpreted by the script.
    #[cfg(unix)]
    fn fake_fastly_returning(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        let stdout_file = dir.path().join("stdout_payload.txt");
        let stderr_file = dir.path().join("stderr_payload.txt");
        let list_file = dir.path().join("list_payload.txt");
        // Store-list JSON: bare array with one entry matching TEST_CONFIG_ID.
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        fs::write(&list_file, list_json).expect("write list payload");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\ncat '{}' >&2\nexit {exit_code}\n",
            list_file.display(),
            stdout_file.display(),
            stderr_file.display(),
        );
        fs::write(&script_path, script).expect("write fastly script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Build a fake `fastly` that logs each argv token (one per line) to
    /// `out_path`, handles the list call correctly, and exits 0 for both calls.
    #[cfg(unix)]
    fn fake_fastly_argv_log(out_path: &Path) -> tempfile::TempDir {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        let list_file = dir.path().join("list_payload.txt");
        let entry_file = dir.path().join("entry_payload.txt");
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        // item_value must be a valid BlobEnvelope JSON so the resolver accepts it.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"v": "logged"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entry_json = format!(
            r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
            serde_json::to_string(&envelope_json).expect("escape")
        );
        fs::write(&list_file, list_json).expect("write list payload");
        fs::write(&entry_file, &entry_json).expect("write entry payload");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\nexit 0\n",
            out_path.display(),
            list_file.display(),
            entry_file.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Process-wide mutex serialising PATH-mutating tests so parallel
    /// test threads don't race on the environment variable.
    #[cfg(unix)]
    fn path_mutation_guard() -> &'static Mutex<()> {
        use std::sync::OnceLock;
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_present_on_success() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Fake fastly: list succeeds with app_config → store-abc123;
        // describe returns valid JSON with item_value that is a BlobEnvelope.
        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entry_json = format!(
            r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
            serde_json::to_string(&envelope).expect("escape")
        );
        let fake = fake_fastly_returning(&entry_json, "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("fake fastly exit-0 must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, envelope);
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_not_found_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // describe exits non-zero with "not found" in stderr → MissingKey.
        let fake = fake_fastly_returning("", "Error: item not found", 1);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("not-found maps to MissingKey (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "not-found stderr => MissingKey"
        );
    }

    /// The Fastly impl distinguishes store-not-found from key-not-found via
    /// `resolve_remote_config_store_id`: when the list call exits non-zero and
    /// the error string contains "not found", `read_config_entry` returns
    /// `MissingStore` without ever calling the describe subcommand.
    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_on_appropriate_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Script that exits non-zero for the list call so resolve fails with
        // a "not found" error, causing read_config_entry to return MissingStore.
        let fake_dir = tempdir().expect("tempdir");
        let stderr_file = fake_dir.path().join("stderr_payload.txt");
        fs::write(&stderr_file, "Error: config store not found for service").expect("write stderr");
        let script_path = fake_dir.path().join("fastly");
        let script = format!(
            "#!/bin/sh\ncat '{stderr}' >&2\nexit 1\n",
            stderr = stderr_file.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        let _path = PathPrepend::new(fake_dir.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("list failure with not-found maps to MissingStore (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "list not-found => MissingStore"
        );
    }

    /// Verify that `read_config_entry` invokes
    /// `fastly config-store-entry describe --store-id=<id> --key=<key> --json`
    /// (after the resolve step that calls `fastly config-store list --json`).
    #[cfg(unix)]
    #[test]
    fn read_remote_invokes_correct_argv() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("argv-log fake must succeed");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "expected Present from argv-log fake"
        );
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // The describe call must include these args (resolve call args
        // are also captured but we only assert the describe shape here).
        assert!(
            captured.contains("config-store-entry"),
            "must invoke config-store-entry; got:\n{captured}"
        );
        assert!(
            captured.contains("describe"),
            "must pass describe subcommand; got:\n{captured}"
        );
        assert!(
            captured.contains("--store-id=store-abc123"),
            "must pass resolved store id; got:\n{captured}"
        );
        assert!(
            captured.contains("--key=greeting"),
            "must pass --key=<key>; got:\n{captured}"
        );
        assert!(
            captured.contains("--json"),
            "must pass --json flag; got:\n{captured}"
        );
    }

    // ---------- chunked push integration tests ----------

    /// Build a valid `BlobEnvelope` JSON string of approximately `target_len` bytes.
    #[cfg(unix)]
    fn make_test_envelope(target_len: usize) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let pad = "x".repeat(target_len.saturating_add(64));
        let data = json!({ "pad": pad });
        let raw =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
        if raw.len() >= target_len {
            let overhead = raw.len().saturating_sub(pad.len());
            let adjusted = "x".repeat(target_len.saturating_sub(overhead));
            let data2 = json!({ "pad": adjusted });
            serde_json::to_string(&BlobEnvelope::new(data2, "2026-06-22T00:00:00Z".into())).unwrap()
        } else {
            raw
        }
    }

    /// Build a fake `fastly` script whose describe response depends on
    /// the `--key=<k>` argument: `key_responses` maps key names to JSON
    /// item-value responses. Falls back to exit 1 "not found" for unknown keys.
    #[cfg(unix)]
    fn fake_fastly_with_key_dispatch(
        _dir: &Path,
        key_responses: &[(String, String)],
    ) -> tempfile::TempDir {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let fake_dir = tempdir().expect("tempdir");
        let list_file = fake_dir.path().join("list.json");
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        fs::write(&list_file, list_json).expect("write list");
        // Write each key response to a named file.
        let mut dispatch_lines = String::new();
        for (key, response) in key_responses {
            let resp_file = fake_dir.path().join(format!("resp_{key}.json"));
            fs::write(&resp_file, response).expect("write resp");
            // Use exact-match: iterate argv and compare each token literally
            // so that a root key like "app_config" does NOT match a chunk key
            // like "app_config.__edgezero_chunks.abc.0".
            writeln!(
                dispatch_lines,
                "  for arg in \"$@\"; do if [ \"$arg\" = \"--key={key}\" ]; then cat '{}'; exit 0; fi; done",
                resp_file.display()
            )
            .expect("write to String is infallible");
        }
        // Fallback outputs "not found" so fetch_remote_config_store_entry
        // maps it to Ok(None) rather than Err.
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\n{dispatch_lines}echo 'Error: item not found' >&2\nexit 1\n",
            list_file.display()
        );
        let script_path = fake_dir.path().join("fastly");
        fs::write(&script_path, &script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        fake_dir
    }

    /// Fake `fastly` for cloud chunk-GC tests. Logs each
    /// `config-store-entry` op ("describe <key>" / "update <key>" /
    /// "delete <key>", plus "delete-argv <full argv>") to `oplog`.
    ///
    /// `root_describe_seq` gives the successive raw `item_value`s returned when
    /// the ROOT key is described (call 1 = the pre-commit prior read, call 2 =
    /// the post-commit read-back). `entry_list` is served for
    /// `config-store-entry list` and is what reclamation derives generations
    /// and supersession times from. `fail_delete_key` makes that one delete
    /// exit non-zero. `describe_hard_error` makes the FIRST describe of each key
    /// fail hard (so the prior read errors while the read-back still works).
    #[cfg(unix)]
    fn fake_fastly_gc(
        root_key: &str,
        root_describe_seq: &[String],
        entry_list: &[(String, String, String)],
        fail_delete_key: Option<&str>,
        describe_hard_error: bool,
        oplog: &Path,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        // Rendered with handlebars. Triple-stache `{{{ }}}` disables HTML
        // escaping (paths are not markup); the shell's own `${var}` /
        // `$(( ))` use single braces so they are literal text to handlebars.
        const TEMPLATE: &str = r#"#!/bin/sh
if [ "$1" = "config-store" ]; then cat '{{{list}}}'; exit 0; fi
sub="$2"
key=""
for arg in "$@"; do case "$arg" in --key=*) key="${arg#--key=}";; esac; done
if [ "$sub" = "list" ]; then printf 'list\n' >> '{{{oplog}}}'; cat '{{{entries}}}'; exit 0; fi
if [ "$sub" = "update" ]; then cat >/dev/null; printf 'update %s\n' "$key" >> '{{{oplog}}}'; exit 0; fi
if [ "$sub" = "delete" ]; then printf 'delete %s\n' "$key" >> '{{{oplog}}}'; printf 'delete-argv %s\n' "$*" >> '{{{oplog}}}'; if [ "$key" = "{{{fail}}}" ]; then echo 'Error: boom' >&2; exit 1; fi; exit 0; fi
if [ "$sub" = "describe" ]; then
  printf 'describe %s\n' "$key" >> '{{{oplog}}}'
  cfile='{{{dir}}}/count_'"$key"
  n=0; [ -f "$cfile" ] && n=$(cat "$cfile"); n=$((n+1)); printf '%s' "$n" > "$cfile"
  {{#if hard_error}}if [ "$n" = "1" ]; then echo 'Error: internal server error' >&2; exit 1; fi{{/if}}
  rf='{{{dir}}}/resp_'"$key"'_'"$n"'.json'
  if [ -f "$rf" ]; then cat "$rf"; exit 0; fi
  echo 'Error: item not found' >&2; exit 1
fi
echo 'unexpected' >&2; exit 1
"#;
        let dir = tempdir().expect("tempdir");
        let list_file = dir.path().join("list.json");
        fs::write(
            &list_file,
            format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#),
        )
        .expect("list");
        let entries_file = dir.path().join("entries.json");
        fs::write(&entries_file, entry_list_json(entry_list)).expect("entries");
        for (index, value) in root_describe_seq.iter().enumerate() {
            let wrapped = format!(
                r#"{{"item_value":{}}}"#,
                serde_json::to_string(value).expect("escape")
            );
            let nth = index.saturating_add(1);
            fs::write(
                dir.path().join(format!("resp_{root_key}_{nth}.json")),
                wrapped,
            )
            .expect("resp");
        }
        let data = serde_json::json!({
            "list": list_file.display().to_string(),
            "entries": entries_file.display().to_string(),
            "oplog": oplog.display().to_string(),
            "dir": dir.path().display().to_string(),
            "fail": fail_delete_key.unwrap_or(""),
            "hard_error": describe_hard_error,
        });
        let script = handlebars::Handlebars::new()
            .render_template(TEMPLATE, &data)
            .expect("render fake fastly script");
        let script_path = dir.path().join("fastly");
        fs::write(&script_path, script).expect("script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        dir
    }

    /// Like `fake_fastly_gc`, but serves a VERBATIM `config-store-entry list`
    /// payload so a test can present a shape `entry_list_json` cannot build
    /// (e.g. a paginated envelope).
    #[cfg(unix)]
    fn fake_fastly_gc_raw_list(
        root_key: &str,
        raw_listing: &str,
        oplog: &Path,
    ) -> tempfile::TempDir {
        let dir = fake_fastly_gc(root_key, &[], &[], None, false, oplog);
        fs::write(dir.path().join("entries.json"), raw_listing).expect("raw entries");
        dir
    }

    /// A `config-store-entry list --json` payload. The item VALUE is a
    /// placeholder: reclamation must only ever use keys and timestamps.
    #[cfg(unix)]
    fn entry_list_json(items: &[(String, String, String)]) -> String {
        let entries: Vec<serde_json::Value> = items
            .iter()
            .map(|(key, created, value)| {
                serde_json::json!({
                    "item_key": key,
                    "created_at": created,
                    "item_value": value,
                })
            })
            .collect();
        serde_json::to_string(&entries).expect("entry list json")
    }

    /// An RFC-3339 stamp `secs` in the past (the shape Fastly returns).
    #[cfg(unix)]
    fn stamp_secs_ago(secs: u64) -> String {
        let delta = chrono::Duration::seconds(i64::try_from(secs).unwrap_or(0));
        let now = chrono::Utc::now();
        now.checked_sub_signed(delta)
            .unwrap_or(now)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// Every chunk of `envelope` as the listing would return it: REAL keys and
    /// REAL payload bytes.
    ///
    /// The values are not decorative. `config gc` proves a generation is ours by
    /// reassembling it and hashing the result against the content-address its
    /// keys name, so a placeholder value would (correctly) fail verification and
    /// never be reclaimed. Fixtures must be honest for these tests to mean
    /// anything.
    #[cfg(unix)]
    fn listed_generation(
        root_key: &str,
        envelope: &str,
        secs_ago: u64,
    ) -> Vec<(String, String, String)> {
        let (chunks, _) = chunked_parts(root_key, envelope);
        let stamp = stamp_secs_ago(secs_ago);
        chunks
            .into_iter()
            .map(|(key, value)| (key, stamp.clone(), value))
            .collect()
    }

    /// The ROOT entry as the listing would return it: its value is the pointer,
    /// which is how `config gc` learns which chunks are live.
    #[cfg(unix)]
    fn listed_root(root_key: &str, envelope: &str, secs_ago: u64) -> (String, String, String) {
        let (_, pointer) = chunked_parts(root_key, envelope);
        (root_key.to_owned(), stamp_secs_ago(secs_ago), pointer)
    }

    /// A chunked envelope with a distinct payload per tag, padded to `pad`
    /// characters so a caller can force a given number of chunks (7 000 bytes
    /// each).
    #[cfg(unix)]
    fn gen_envelope_padded(tag: &str, pad: usize) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let data = json!({ tag: "x".repeat(pad) });
        serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
            .expect("envelope")
    }

    /// A chunked envelope with a distinct payload per tag.
    #[cfg(unix)]
    fn gen_envelope(tag: &str) -> String {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
        serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
            .expect("envelope")
    }

    /// Split a chunked envelope into (chunk `(key, value)` pairs, root pointer).
    #[cfg(unix)]
    fn chunked_parts(root_key: &str, envelope: &str) -> (Vec<(String, String)>, String) {
        let entries = prepare_fastly_config_entries(root_key, envelope).expect("expand");
        let (_, pointer) = entries.last().expect("pointer").clone();
        let chunks = entries[..entries.len().saturating_sub(1)].to_vec();
        (chunks, pointer)
    }

    /// Just the chunk KEYS of a generation (for delete assertions).
    #[cfg(unix)]
    fn chunk_keys_of(root_key: &str, envelope: &str) -> Vec<String> {
        let (chunks, _) = chunked_parts(root_key, envelope);
        chunks.into_iter().map(|(key, _)| key).collect()
    }

    #[cfg(unix)]
    fn oplog_has(oplog: &Path, line: &str) -> bool {
        fs::read_to_string(oplog)
            .unwrap_or_default()
            .lines()
            .any(|entry| entry == line)
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_rejects_reserved_key() {
        let dir = tempdir().expect("tempdir");
        let bad_key = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");
        let err = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(bad_key.clone(), "{}".to_owned())],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("reserved key must be rejected");
        assert!(err.contains(&bad_key), "names the key: {err}");
    }

    /// Schema drift must never echo the config payload. App config can hold
    /// credentials; CLI status lines are logged verbatim and CI logs are
    /// retained/shared. Only a size + field-name shape may be reported.
    #[cfg(unix)]
    #[test]
    fn read_config_entry_schema_drift_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_abc123";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Valid JSON, but the value moved out of `item_value` (schema drift).
        let drift = format!(r#"{{"value_moved_here":"{SENTINEL}"}}"#);
        let fake = fake_fastly_returning(&drift, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("schema drift must error")
        };
        assert!(
            !err.contains(SENTINEL),
            "error must not leak the config payload: {err}"
        );
        assert!(
            err.contains("bytes"),
            "error should carry a redacted size/shape summary: {err}"
        );
    }

    /// The FAILURE branch leaks too: a Fastly error that quotes the stored
    /// value back in stderr must not reach the user-facing error.
    #[cfg(unix)]
    #[test]
    fn read_config_entry_stderr_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_stderr1";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Not a "not found" — a hard failure that echoes the value.
        let stderr = format!("Error: internal failure processing value {SENTINEL}");
        let fake = fake_fastly_returning("", &stderr, 1);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("hard stderr failure must error")
        };
        assert!(
            !err.contains(SENTINEL),
            "stderr must be redacted, not echoed: {err}"
        );
        assert!(
            err.contains("suppressed"),
            "error should say the stderr was suppressed: {err}"
        );
    }

    /// `config gc` reads `item_value` for every entry (to classify roots). A
    /// malformed listing whose values carry secrets must fail closed WITHOUT
    /// echoing any value. (Replaces the old push prior-read redaction tests,
    /// which are now vacuous: a cloud push performs no pre-commit read.)
    #[cfg(unix)]
    #[test]
    fn gc_list_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_gc_list";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let good = entry_list_json(&listing);
        // A valid entry whose VALUE contains the sentinel, plus a malformed
        // sibling (no created_at) to trip the fail-closed path.
        let mut array: serde_json::Value = serde_json::from_str(&good).unwrap();
        let arr = array.as_array_mut().unwrap();
        arr.push(serde_json::json!({
            "item_key": "some.__edgezero_chunks.deadbeef.0",
            "item_value": SENTINEL,
        }));
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            None,
            false,
            &dir.path().join("ops.log"),
        );
        fs::write(
            fake.path().join("entries.json"),
            serde_json::to_string(&array).unwrap(),
        )
        .expect("overwrite entries");
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            !err.contains(SENTINEL),
            "the fail-closed error must not echo a stored value: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_direct_entry_at_exactly_8000_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        assert_eq!(envelope.len(), FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        // One physical entry written (direct).
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "must write root key directly: {captured}"
        );
        assert!(
            out[0].contains("1 physical entries (1 logical)"),
            "summary reports 1 physical entry: {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_chunks_and_root_pointer_for_8001_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        assert!(envelope.len() > FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // At least one chunk key must appear before the root key.
        assert!(
            captured.contains(".__edgezero_chunks."),
            "chunk keys must be written: {captured}"
        );
        // Root pointer must also be written.
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "root pointer must be written: {captured}"
        );
        // Root key must be LAST in the log (chunk lines come before it).
        let root_pos = captured.rfind(&format!("--key={TEST_CONFIG_ID}")).unwrap();
        let chunk_pos = captured.find(".__edgezero_chunks.").unwrap();
        assert!(
            chunk_pos < root_pos,
            "chunk writes must precede root pointer write: chunk_pos={chunk_pos} root_pos={root_pos}"
        );
        assert!(out[0].contains("logical"), "summary line present: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_dry_run_reports_direct_vs_chunked() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");

        let direct_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let chunked_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));

        let entries = vec![
            ("cfg_direct".to_owned(), direct_envelope),
            ("cfg_chunked".to_owned(), chunked_envelope),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not error");

        // No shellout happens; output must describe intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would push `cfg_direct` as direct entry"),
            "must report direct: {combined}"
        );
        assert!(
            combined.contains("would push `cfg_chunked` as chunked"),
            "must report chunked: {combined}"
        );
    }

    /// Spec 12.7: pushing two blobs under different root keys
    /// (e.g. `app_config` + `app_config_staging`) must leave both
    /// keys readable from the local fastly.toml so the runtime
    /// `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY` override can
    /// switch between them. Prior to the upsert fix the second
    /// push wholesale-replaced the per-store contents table.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_preserves_sibling_keys() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");
        let store = ResolvedStoreId::from_logical(TEST_CONFIG_ID);
        let ctx = AdapterPushContext::new();

        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &store,
                &[("app_config".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &ctx,
                false,
            )
            .expect("first push");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &store,
                &[(
                    "app_config_staging".to_owned(),
                    "{\"envelope\":\"B\"}".to_owned(),
                )],
                &ctx,
                false,
            )
            .expect("second push (sibling key)");

        let raw = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = raw.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents after sibling push");
        let app_config = contents
            .get("app_config")
            .and_then(toml_edit::Item::as_str)
            .expect("default key must survive sibling push");
        assert_eq!(
            app_config, "{\"envelope\":\"A\"}",
            "default key value: {raw}"
        );
        let staging = contents
            .get("app_config_staging")
            .and_then(toml_edit::Item::as_str)
            .expect("staging key must be present");
        assert_eq!(staging, "{\"envelope\":\"B\"}", "staging key value: {raw}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_writes_literal_dotted_chunk_keys() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("local push must succeed");

        let after = fs::read_to_string(&fastly_toml).expect("read back");
        // Chunk keys contain '.' and must appear as quoted string keys,
        // not as TOML nested tables (which would look like [table.sub]).
        assert!(
            after.contains(".__edgezero_chunks."),
            "chunk keys written to fastly.toml: {after}"
        );
        // Parse with toml_edit and confirm chunk keys are string-keyed entries.
        let doc: toml_edit::DocumentMut = after.parse().expect("must parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .expect("contents table must exist");
        // At least one chunk key must be present as a string value (not a table).
        let has_chunk_string = contents.as_table().is_some_and(|tbl| {
            tbl.iter()
                .any(|(key, val)| key.contains(".__edgezero_chunks.") && val.as_value().is_some())
        });
        assert!(
            has_chunk_string,
            "chunk keys must be literal string-valued entries, not nested tables: {after}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_reports_chunking_and_does_not_edit_fastly_toml() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let original = "name = \"demo\"\n";
        fs::write(&fastly_toml, original).expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("local dry-run must not error");

        // File must be untouched.
        let after = fs::read_to_string(&fastly_toml).expect("read back");
        assert_eq!(after, original, "dry-run must not edit fastly.toml");

        // Output must describe chunking intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would set") && combined.contains("chunked"),
            "must report chunked intent: {combined}"
        );
    }

    // ---------- chunked read integration tests ----------

    #[cfg(unix)]
    #[test]
    fn read_config_entry_resolves_direct_value_unchanged() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = BlobEnvelope::new(json!({"hello": "world"}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&json_str).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Build a key→response map for every physical entry.
        let mut key_responses: Vec<(String, String)> = Vec::new();
        for (pk, pv) in &physical {
            let resp = format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap());
            key_responses.push((pk.clone(), resp));
        }
        // The root key should return the pointer.
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        key_responses.push((TEST_CONFIG_ID.to_owned(), ptr_resp));

        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_missing_chunk() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Only provide the root pointer; omit chunk responses so chunk fetch returns not-found.
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        let key_responses = vec![(TEST_CONFIG_ID.to_owned(), ptr_resp)];
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("missing chunk must error")
        };
        assert!(
            err.contains("missing chunk"),
            "error must mention missing chunk: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_corrupt_chunk_hash() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        let mut key_responses: Vec<(String, String)> = Vec::new();
        // Corrupt first chunk's content.
        let (first_chunk_key, first_chunk_val) = &physical[0];
        let corrupted: String = first_chunk_val.chars().map(|_| 'Z').collect();
        let corrupt_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&corrupted).unwrap()
        );
        key_responses.push((first_chunk_key.clone(), corrupt_resp));
        // Remaining chunks as normal.
        for (pk, pv) in physical
            .iter()
            .take(physical.len().saturating_sub(1))
            .skip(1)
        {
            key_responses.push((
                pk.clone(),
                format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap()),
            ));
        }
        key_responses.push((
            TEST_CONFIG_ID.to_owned(),
            format!(
                r#"{{"item_value":{}}}"#,
                serde_json::to_string(pointer_json).unwrap()
            ),
        ));
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("corrupt chunk must error")
        };
        assert!(
            err.contains("does not match the SHA-256"),
            "error must say what failed: {err}"
        );
        // identified by POSITION, and neither hash echoed -- the
        // expected one comes from the stored pointer, so it is value-controlled.
        assert!(
            err.contains("chunk 0"),
            "error must locate the failing chunk by position: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_malformed_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Root value is JSON but neither a BlobEnvelope nor a valid pointer.
        let bad_json = r#"{"some_field":"not a pointer or envelope"}"#;
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(bad_json).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("malformed pointer must error")
        };
        assert!(
            err.contains("neither a valid BlobEnvelope") || err.contains("chunk pointer"),
            "error must describe parse failure: {err}"
        );
    }

    // ---------- local read integration tests ----------

    #[test]
    fn read_config_entry_local_resolves_direct_value() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = BlobEnvelope::new(json!({"x": 1_i32}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        // Write directly as a single entry (not via push_config_entries_local so we
        // control the exact TOML content).
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), json_str.clone())],
            &[],
        )
        .expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("local read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[test]
    fn read_config_entry_local_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        // Write all physical entries (chunks + pointer) to the local store.
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &physical, &[])
            .expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    /// a corrupt/invalid prior value must NOT abort the
    /// local read, or the CLI push aborts on the diff read before the writer's
    /// fail-soft ("overwrite, warn, prune nothing") can repair the state.
    /// `config push --local` is how an operator recovers, so the read reports
    /// `Unsupported` ("cannot diff") and lets the write proceed.
    #[test]
    fn read_config_entry_local_degrades_corrupt_prior_to_unsupported() {
        use crate::chunked_config::{CHUNK_KEY_INFIX, POINTER_KIND};
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // A pointer-KIND value that is invalid (missing the chunks it needs).
        // The resolver would error on this; the local read must NOT propagate
        // that as `Err`.
        let broken_pointer = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"cfg{CHUNK_KEY_INFIX}{sha}.0","len":10,"sha256":"x"}}],"data_sha256":"","envelope_len":10,"envelope_sha256":"{sha}"}}"#,
            sha = "a".repeat(64),
        );
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), broken_pointer)],
            &[],
        )
        .expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("a corrupt local prior must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "a corrupt prior value must degrade to Unsupported so the push can overwrite it"
        );
    }

    /// Spec 12.3 + 9.3: a second oversized push must converge the
    /// runtime on the NEW envelope — chunk keys are content-addressed
    /// by the full-envelope SHA, so push B writes a new chunk-set and
    /// installs a new root pointer.
    ///
    /// The local fastly.toml writer upserts per-key (so a sibling
    /// `--key app_config_staging` push leaves `app_config` intact per
    /// spec 12.7). Within the SAME root key, GC on re-push prunes the
    /// prior generation: after envelope B's push, envelope A's chunks —
    /// now unreferenced by the `app_config` pointer — are removed from
    /// the contents table. A read after push B follows the active
    /// pointer and reconstructs envelope B, not A.
    #[cfg(unix)]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "linear test scenario: push A, inspect, push B, inspect, read; splitting would obscure the chunk-set comparison"
    )]
    fn second_oversized_push_converges_runtime_on_new_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // First push: envelope A. Records the chunk-key set so we can
        // confirm they are pruned by the second push's GC.
        let envelope_a = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_a.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push must succeed");

        let after_a = fs::read_to_string(&fastly_toml).expect("read");
        let doc_a: toml_edit::DocumentMut = after_a.parse().expect("parse");
        let contents_a = doc_a
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push A");
        let chunks_a: Vec<String> = contents_a
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_a.is_empty(),
            "push A must have produced chunk entries: {after_a}"
        );

        // Second push: a DIFFERENT oversized envelope B. The
        // content-addressed chunk keys must shift to B's sha; GC then
        // prunes the old A-chunks. Build envelope B with a distinct
        // payload key so its SHA differs from A's even at the same
        // total length.
        let envelope_b = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ "alt": "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:01Z".to_owned()))
                .expect("envelope B serialises")
        };
        assert_ne!(envelope_a, envelope_b, "test fixtures must differ");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("second push must succeed");

        let after_b = fs::read_to_string(&fastly_toml).expect("read");
        let doc_b: toml_edit::DocumentMut = after_b.parse().expect("parse");
        let contents_b = doc_b
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push B");
        let chunks_b: Vec<String> = contents_b
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_b.is_empty(),
            "push B must have produced chunk entries: {after_b}"
        );

        // Chunk keys are content-addressed by envelope SHA, so the B
        // push installs a fresh chunk-set whose keys are all distinct
        // from A's. GC on re-push prunes the now-unreferenced A-chunks.
        let new_b_chunks: Vec<&String> = chunks_b
            .iter()
            .filter(|key| !chunks_a.contains(*key))
            .collect();
        assert!(
            !new_b_chunks.is_empty(),
            "push B must have added at least one new content-addressed chunk: A-set={chunks_a:?} B-set={chunks_b:?}"
        );
        // Old A-chunks are pruned: GC deletes the prior generation the
        // old pointer referenced once B's pointer supersedes it.
        for chunk_key in &chunks_a {
            assert!(
                !chunks_b.contains(chunk_key),
                "old A-chunk `{chunk_key}` must be pruned from the local table after push B; B-set={chunks_b:?}"
            );
        }

        // Runtime-correctness property: a fresh read after push B
        // reconstructs envelope B (NOT envelope A).
        let read = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local read after push B");
        let ReadConfigEntry::Present(value) = read else {
            panic!("expected Present after push B");
        };
        assert_eq!(
            value, envelope_b,
            "read after second push must reconstruct envelope B, not A"
        );
        assert_ne!(
            value, envelope_a,
            "old envelope A's chunks must be inert -- read must NOT return A"
        );
    }

    // ---------- config gc (operator-invoked reclamation) ----------

    #[cfg(unix)]
    fn run_gc(dir: &Path, older_than_secs: u64, dry_run: bool) -> Result<Vec<String>, String> {
        FastlyCliAdapter.gc_config_entries(
            dir,
            None,
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            &AdapterPushContext::new(),
            older_than_secs,
            dry_run,
        )
    }

    /// gc never deletes a chunk the LIVE root pointer references, however old.
    #[cfg(unix)]
    #[test]
    fn gc_never_deletes_live_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        // The live generation is ANCIENT, but it is referenced by the root.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 1, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "live chunk `{key}` must never be reclaimed; log:\n{log}\nout: {out:?}"
            );
        }
    }

    /// gc reclaims unreferenced chunks older than the operator's threshold.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_unreferenced_chunks_older_than_threshold() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        // The live config has been stable for 2 days; the operator asserts a 1-day
        // window. So everything superseded (<= when live went live, i.e. >= 2
        // days ago) is safely reclaimable.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800)); // a week old

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "orphan `{key}` older than the threshold must be reclaimed; out: {out:?}"
            );
        }
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "live chunk `{key}` must survive"
            );
        }
    }

    /// The soundness test (design-3 counterexample): a root whose
    /// current config was deployed seconds ago must NOT have its prior generation
    /// reclaimed, even if that generation's chunks are ANCIENT. The clock is the
    /// live config's age, not the orphan chunk's own creation time.
    #[cfg(unix)]
    #[test]
    fn gc_protects_recently_superseded_generation_with_old_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let prior = gen_envelope("prior");
        let prior_chunks = chunk_keys_of(TEST_CONFIG_ID, &prior);

        // Live config went live 30s ago; the prior generation's chunks are a year
        // old but were superseded only 30s ago -> POPs may still serve them.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 30)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 30));
        listing.extend(listed_generation(TEST_CONFIG_ID, &prior, 31_536_000)); // ~1 year

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // Even a generous 1-day threshold must NOT delete the prior generation,
        // because the live config has only been stable for 30 seconds.
        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &prior_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a generation superseded 30s ago must be retained despite old chunks: `{key}`; log:\n{log}"
            );
        }
    }

    /// a live root whose pointer drops its
    /// last chunk ref AND restates `envelope_len` as the remaining sum passes
    /// every metadata check. The dropped chunk is then absent from the live set
    /// and looks like a deletable orphan -- while the config still needs it.
    ///
    /// Guards the PLANNER's content verification (a unit test on
    /// `gc_verify_generation` alone does not prove the planner calls it).
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_when_a_live_pointer_underreports_its_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Padded so the generation is >= 3 chunks: this case needs a ref to
        // drop that still leaves a plausible multi-chunk set behind.
        let live = gen_envelope_padded("live", 20_000);
        let (chunks, pointer_json) = chunked_parts(TEST_CONFIG_ID, &live);
        assert!(chunks.len() >= 3, "need >= 3 chunks for this case");

        // Doctor the pointer: drop the last ref, restate envelope_len to match
        // the survivors. Generation, indexes, per-chunk lens and the sum all
        // still agree -- only the CONTENT does not.
        let mut pointer: serde_json::Value = serde_json::from_str(&pointer_json).expect("parse");
        let refs = pointer
            .get_mut("chunks")
            .and_then(serde_json::Value::as_array_mut)
            .expect("chunks array");
        refs.pop().expect("drop the last chunk ref");
        let surviving_len: u64 = refs
            .iter()
            .filter_map(|chunk| chunk.get("len").and_then(serde_json::Value::as_u64))
            .sum();
        pointer["envelope_len"] = serde_json::json!(surviving_len);
        let doctored = serde_json::to_string(&pointer).expect("serialise");

        // The store still physically holds ALL the chunks, including the one the
        // doctored pointer no longer names.
        let orphaned_by_omission = chunks.last().expect("last chunk").0.clone();
        let stamp = stamp_secs_ago(999_999);
        let mut listing = vec![(TEST_CONFIG_ID.to_owned(), stamp.clone(), doctored)];
        for (key, value) in chunks {
            listing.push((key, stamp.clone(), value));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("does not reconstruct the envelope it claims"),
            "expected a content-address mismatch on the live pointer, got: {err}"
        );
        assert!(
            !oplog_has(&oplog, &format!("delete {orphaned_by_omission}")),
            "a chunk the live config still needs must never be deleted because its pointer \
             under-reported it: `{orphaned_by_omission}`"
        );
    }

    /// a LONE entry whose value hashes to the generation
    /// its own key names would otherwise "prove" itself and be deleted. But our
    /// writer never emits a one-chunk generation (an oversized envelope always
    /// splits into >= 2), so a group of one is never ours -- it is a root-like
    /// value sitting at a chunk-shaped key. This is the case a pure hash check
    /// cannot catch on its own.
    #[cfg(unix)]
    #[test]
    fn gc_never_reclaims_a_lone_self_consistent_chunk() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        // A complete envelope stored at a chunk-shaped key whose generation IS
        // that envelope's own SHA -- so it reassembles to its content-address.
        let squatter_value = gen_envelope("someones-real-config");
        let self_sha = sha256_hex(squatter_value.as_bytes());
        let squatter_key = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{self_sha}.0");
        listing.push((
            squatter_key.clone(),
            stamp_secs_ago(31_536_000),
            squatter_value,
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !oplog_has(&oplog, &format!("delete {squatter_key}")),
            "a one-chunk 'generation' is never something this writer emitted, so it must not be \
             reclaimed even though it hashes to its own key: `{squatter_key}`; log:\n{log}"
        );
    }

    /// a delete that fails on a generation's FIRST key has
    /// an UNKNOWN outcome -- Fastly may have committed it before returning an
    /// error.  called this "whole and retryable", which is unsound: if the
    /// failed delete did commit, a re-run finds a fragment. The honest report is
    /// a NOTE that the outcome is uncertain, NOT a clean-retry promise. We still
    /// stop the generation so a CONFIRMED partial delete cannot happen.
    #[cfg(unix)]
    #[test]
    fn gc_first_delete_failure_is_reported_as_uncertain_not_clean_retry() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // The FIRST chunk of the doomed generation fails to delete.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&dead_chunks[0]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        assert!(
            err.contains("unknown outcome"),
            "a failed delete's outcome is unknown and must be reported as such: {err}"
        );
        assert!(
            !err.contains("will retry them"),
            "the disproven clean-retry promise must be gone: {err}"
        );
        // The siblings must NOT have been ATTEMPTED -- stopping is what prevents a
        // CONFIRMED partial delete.
        for key in dead_chunks.iter().skip(1) {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "after the first failure the generation must be left alone: `{key}`"
            );
        }
    }

    /// the stateful case. A remote delete that COMMITS but
    /// still reports failure leaves a real fragment. On the SECOND run that
    /// missing key makes the generation unprovable, so it must be reported as
    /// left-untouched (surfaced), never silently dropped.
    #[cfg(unix)]
    #[test]
    fn gc_committed_but_failed_delete_surfaces_as_unprovable_next_run() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");

        // SECOND run's world: the first chunk's delete committed last time, so it
        // is gone. The generation is now a fragment.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let mut dead_gen = listed_generation(TEST_CONFIG_ID, &dead, 604_800);
        let survivor = dead_gen[1].0.clone();
        dead_gen.remove(0); // the committed-deleted chunk is absent now
        listing.extend(dead_gen);

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        assert!(
            !oplog_has(&oplog, &format!("delete {survivor}")),
            "an unprovable fragment survivor must not be deleted: `{survivor}`"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("not byte-identical to what this writer would produce")),
            "the surviving fragment must be SURFACED as left-untouched, not silently dropped: {out:?}"
        );
    }

    /// if a delete fails PART-WAY through a generation, the
    /// survivors are an incomplete generation that `prove_generation` can never
    /// verify again -- so `gc` will never reclaim them. Claiming "re-run to
    /// retry" there was false. Say plainly that recovery is manual.
    #[cfg(unix)]
    #[test]
    fn gc_reports_stranded_survivors_as_manual_recovery() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Padded to >= 3 chunks so a mid-generation failure leaves survivors.
        let live = gen_envelope("live");
        let dead = gen_envelope_padded("dead", 20_000);
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 3, "need >= 3 chunks");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // The SECOND chunk fails: the first is already gone by then.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&dead_chunks[1]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        assert!(
            err.contains("INCOMPLETE generation") && err.contains("re-running will NOT help"),
            "a stranded fragment must not be described as retryable: {err}"
        );
        // It must name the survivors and how to remove them by hand.
        for key in dead_chunks.iter().skip(2) {
            assert!(
                err.contains(key.as_str()),
                "the operator needs the exact surviving keys: `{key}` missing from: {err}"
            );
        }
        assert!(
            err.contains("fastly config-store-entry delete"),
            "give the operator the recovery command: {err}"
        );
        // And we stopped rather than deleting the rest.
        for key in dead_chunks.iter().skip(2) {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "deletion must stop at the first failure in a generation: `{key}`"
            );
        }
    }

    /// root keys are free-form, so a chunk key can hold
    /// shell metacharacters. Manual-recovery commands must render them so that
    /// pasting cannot execute or misparse -- single-quoted, with embedded quotes
    /// escaped.
    #[test]
    fn recovery_commands_are_shell_safe() {
        // A key crafted to run `id` and to break argument parsing if unquoted.
        let hostile = "app$(id).__edgezero_chunks.'; rm -rf /'.0".to_owned();
        let keys = [hostile.clone()];
        let rendered = recovery_commands("store-abc", &keys);

        // The dangerous substring is not sitting there unquoted.
        assert!(
            !rendered.contains("$(id)") || rendered.contains("'app$(id)"),
            "shell-active text must be inside single quotes: {rendered}"
        );
        // Every embedded single quote is closed-escaped-reopened, so no quote
        // context leaks.
        assert!(
            rendered.contains(r"'\''"),
            "embedded single quotes must be escaped as '\\'': {rendered}"
        );
        // Sanity: what a POSIX shell would parse back out of our --key argument
        // is EXACTLY the original key (round-trip through `sh`).
        let key_arg = rendered
            .split("--key=")
            .nth(1)
            .and_then(|rest| rest.split(" --auto-yes").next())
            .expect("a --key argument");
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {key_arg}"))
            .output()
            .expect("run sh");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            hostile,
            "the shell must parse the quoted argument back to the exact key"
        );
    }

    /// a valid DIRECT envelope at a chunk-shaped key is a
    /// runtime-readable root, but round 9 only protected POINTER values there.
    ///
    /// Construction: pad a small valid envelope with trailing JSON whitespace
    /// past the entry limit. The writer chunks it; chunk 0 (the first 7 000
    /// bytes) is the whole envelope plus trailing spaces, which STILL parses and
    /// verifies as that envelope. So chunk 0's key holds a valid direct envelope
    /// -- a root -- yet the generation round-trips through the writer and passes
    /// every proof, so GC deletes chunk 0.
    #[cfg(unix)]
    #[test]
    fn valid_envelope_at_chunk_shaped_key_is_a_protected_root() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A small valid envelope + trailing whitespace over the entry limit.
        let envelope = BlobEnvelope::new(json!({"k":"v"}), "2026-06-22T00:00:00Z".into());
        let mut padded = serde_json::to_string(&envelope).unwrap();
        padded.push_str(&" ".repeat(8_200));
        let entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &padded).expect("expand");
        assert!(entries.len() >= 3, "need >= 2 chunks + pointer");
        let holder_key = entries[0].0.clone();
        // Sanity: chunk 0's value IS a standalone valid envelope.
        let parsed: BlobEnvelope =
            serde_json::from_str(&entries[0].1).expect("chunk 0 must parse as an envelope");
        parsed.verify().expect("chunk 0 must verify as an envelope");

        // Seed the store with the chunk entries only -- NO live pointer refers
        // to them, so this generation looks orphaned. Aged old.
        let stamp = stamp_secs_ago(604_800);
        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        for (key, value) in &entries[..entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        drop(run_gc(dir.path(), 86_400, false));
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !oplog_has(&oplog, &format!("delete {holder_key}")),
            "an entry whose value is a valid direct envelope is a runtime-readable root and must \
             never be deleted, whatever its key looks like: `{holder_key}`; log:\n{log}"
        );
    }

    /// key shape is not authoritative for ROOTS either.
    ///
    /// A valid pointer stored at a chunk-SHAPED key (`shadow.__edgezero_chunks.
    /// <sha>.0`) is skipped by the live-set scan, which excludes chunk-shaped
    /// keys up front. The runtime resolver follows any pointer it is given, so
    /// that pointer's references ARE live -- but GC never sees them, calls the
    /// generation orphaned, and deletes it.
    #[cfg(unix)]
    #[test]
    fn pointer_at_chunk_shaped_key_keeps_its_references_live() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // `app_config`'s CURRENT config is small enough to store directly, so
        // its own root references no chunks at all.
        let live_direct = gen_envelope_padded("live-direct", 100);
        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            live_direct,
        )];

        // An older chunked generation of `app_config` still exists...
        let referenced = gen_envelope("still-referenced");
        let referenced_chunks = chunk_keys_of(TEST_CONFIG_ID, &referenced);
        listing.extend(listed_generation(TEST_CONFIG_ID, &referenced, 604_800));

        // ...and a pointer at a CHUNK-SHAPED key references it. The resolver
        // would happily follow this, so those chunks are LIVE.
        let (_, referenced_pointer) = chunked_parts(TEST_CONFIG_ID, &referenced);
        let shadow_key = format!("shadow{CHUNK_KEY_INFIX}{}.0", "d".repeat(64));
        listing.push((shadow_key, stamp_secs_ago(604_800), referenced_pointer));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // The RESULT does not matter here (it may Err after the fix if the
        // shadow pointer's own chunks are incomplete); the invariant is purely
        // that no LIVE-referenced chunk is deleted, which the oplog proves.
        drop(run_gc(dir.path(), 86_400, false));
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &referenced_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a chunk a live pointer references must never be deleted, whatever the KEY of \
                 the entry holding that pointer looks like: `{key}`; log:\n{log}"
            );
        }
    }

    /// a FOREIGN writer needs NO preimage to satisfy a
    /// content-address. Pick envelope E, compute H = sha256(E), split E however
    /// you like, store the parts as `<root>.__edgezero_chunks.H.0` / `.1`. Under
    /// hash-only checking that group "proved" itself and was deleted.
    ///
    /// The round-trip closes it: the writer, given those same bytes, must emit
    /// exactly these keys and values. A split at boundaries we would never
    /// choose is not our output, so it is left alone.
    #[cfg(unix)]
    #[test]
    fn gc_never_reclaims_a_foreign_content_addressed_group() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        // A foreign writer's data: a valid envelope, content-addressed under our
        // reserved namespace, but split at ITS OWN boundary (not our 7 000-byte
        // UTF-8-safe one). Everything hashes correctly -- no preimage needed.
        let foreign = gen_envelope_padded("foreign-tool", 20_000);
        let generation = sha256_hex(foreign.as_bytes());
        let (head, tail) = foreign.split_at(1_234);
        let foreign_keys = [
            format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{generation}.0"),
            format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{generation}.1"),
        ];
        listing.push((
            foreign_keys[0].clone(),
            stamp_secs_ago(31_536_000),
            head.to_owned(),
        ));
        listing.push((
            foreign_keys[1].clone(),
            stamp_secs_ago(31_536_000),
            tail.to_owned(),
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &foreign_keys {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a group this writer would never have produced must not be reclaimed, however \
                 well it hashes: `{key}`; log:\n{log}"
            );
        }
    }

    /// an entry can be chunk-SHAPED without being a chunk
    /// -- a store may predate this feature or be shared, and push-time
    /// reserved-key rejection cannot protect what already exists. Deleting one
    /// would destroy live config.
    ///
    /// proof is CONTENT, not shape. A candidate generation is ours only
    /// if it reassembles to the content-address its own keys name. Unprovable
    /// entries are left UNTOUCHED and reported -- not fatal, because one foreign
    /// entry must not block reclaiming the rest of the store forever.
    #[cfg(unix)]
    #[test]
    fn gc_leaves_unprovable_chunk_shaped_entries_untouched() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        // A real orphan generation: provable, old -> must still be reclaimed.
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // Pre-existing entries at chunk-shaped keys that we did NOT write: one
        // holding somebody's real config envelope, one holding plain text.
        // Both are old enough to look "eligible" on age alone.
        let envelope_squatter = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "b".repeat(64));
        let text_squatter = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "c".repeat(64));
        listing.push((
            envelope_squatter.clone(),
            stamp_secs_ago(31_536_000),
            gen_envelope("someones-real-config"),
        ));
        listing.push((
            text_squatter.clone(),
            stamp_secs_ago(31_536_000),
            "just some plain text".to_owned(),
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();

        for key in [&envelope_squatter, &text_squatter] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "an entry we cannot prove we wrote must never be deleted: `{key}`; log:\n{log}"
            );
        }
        // Left untouched must not mean silently ignored. The wording must not
        // over-claim either: these two entries fail for DIFFERENT reasons (a
        // wrong content-address vs a count this writer never emits), so the
        // summary says "not byte-identical to what this writer would produce"
        // rather than naming one specific check.
        assert!(
            out.iter()
                .any(|line| line.contains("not byte-identical to what this writer would produce")),
            "the summary must report what it declined to judge; out: {out:?}"
        );
        // ...and a genuine orphan generation is still reclaimed, so one foreign
        // entry does not block the store.
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "a provable orphan generation must still be reclaimed: `{key}`; log:\n{log}"
            );
        }
    }

    /// a key is unique in a config store, so duplicate rows
    /// mean the listing is not one consistent view. Left alone, last-row-wins on
    /// `created_at` could age a recent key into eligibility.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_duplicate_listing_keys() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let mut orphans = listed_generation(TEST_CONFIG_ID, &dead, 30);
        // The same key twice, with conflicting ages: young (real) then ancient.
        let (dup_key, _, dup_value) = orphans[0].clone();
        orphans.push((dup_key.clone(), stamp_secs_ago(31_536_000), dup_value));
        listing.extend(orphans);

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("more than once"),
            "expected a refusal naming the duplicate key, got: {err}"
        );
        assert!(
            !oplog_has(&oplog, &format!("delete {dup_key}")),
            "a duplicated row must not let a recent key be aged into eligibility"
        );
    }

    /// `gc_config_entries` is a public trait method, so the
    /// zero-window rule must live at the DESTRUCTIVE boundary, not only in the
    /// CLI that usually calls it. Rejected before any `fastly` invocation.
    #[cfg(unix)]
    #[test]
    fn gc_adapter_boundary_rejects_a_zero_window() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // Straight at the adapter, bypassing the CLI's own gate.
        let err = run_gc(dir.path(), 0, false).expect_err("a destructive zero window must fail");
        assert!(
            err.contains("non-zero `--older-than`"),
            "expected the boundary itself to reject zero, got: {err}"
        );
        assert!(
            !fs::read_to_string(&oplog)
                .unwrap_or_default()
                .contains("delete "),
            "nothing may be deleted under a zero window"
        );
        // A DRY-RUN at zero is still allowed: it previews and deletes nothing.
        run_gc(dir.path(), 0, true).expect("a dry-run may preview at zero");
    }

    /// a root whose value is TRUNCATED/unparseable must fail
    /// closed. It is pointer-shaped garbage -- we cannot tell what it references,
    /// so its (live!) chunks must not be judged orphaned. Regression guard: the
    /// push-path helper returns `Ok([])` for a non-pointer value, which on THIS
    /// path would read as "references nothing" and reclaim the whole store.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_truncated_root_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let (_, pointer) = chunked_parts(TEST_CONFIG_ID, &live);
        // A write that landed half-way: a valid PREFIX of the real pointer that
        // is no longer valid JSON. (Chars, not a byte slice -- never split a
        // codepoint.)
        let truncated: String = pointer.chars().take(40).collect();
        assert!(
            serde_json::from_str::<serde_json::Value>(&truncated).is_err(),
            "fixture must be unparseable to exercise the classifier: {truncated}"
        );

        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            truncated,
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("refusing to reclaim"),
            "expected a fail-closed refusal, got: {err}"
        );
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "nothing may be deleted when a root is unclassifiable: `{key}`"
            );
        }
    }

    /// an ENVELOPED listing (`{"items":[...]}`) may carry
    /// pagination we do not follow. A page that omitted a root would make that
    /// root's live chunks look orphaned -- and the completeness guard cannot see
    /// a root that isn't there. Refuse the shape outright.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_enveloped_listing() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        let enveloped = format!(
            r#"{{"items":{},"next_cursor":"abc"}}"#,
            entry_list_json(&listing)
        );

        let fake = fake_fastly_gc_raw_list(TEST_CONFIG_ID, &enveloped, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("bare array") && err.contains("Nothing was deleted"),
            "expected a refusal naming the unsupported listing shape, got: {err}"
        );
        assert!(
            !fs::read_to_string(&oplog)
                .unwrap_or_default()
                .contains("delete "),
            "an unsupported listing shape must delete nothing"
        );
    }

    /// a root with an EMPTY value is as dangerous as a
    /// missing one -- it would classify as "references nothing" and orphan its
    /// live chunks. The listing parser rejects it before any reasoning.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_empty_root_value() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            String::new(),
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("empty `item_value`"),
            "expected a refusal naming the empty field, got: {err}"
        );
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "nothing may be deleted on an unreadable listing: `{key}`"
            );
        }
    }

    /// the orphan's OWN age is mandatory -- an old root does
    /// not license deleting a chunk written seconds ago (e.g. by a concurrent
    /// push that has not committed its pointer yet). Both ages must clear the
    /// window; the more restrictive wins.
    #[cfg(unix)]
    #[test]
    fn gc_retains_young_orphan_under_long_stable_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let fresh = gen_envelope("fresh");
        let fresh_chunks = chunk_keys_of(TEST_CONFIG_ID, &fresh);

        // The root's live config has been stable for a year -- so the live-config
        // clock alone would happily reclaim. But these chunks were written 10s
        // ago and no pointer names them yet.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 31_536_000)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 31_536_000));
        listing.extend(listed_generation(TEST_CONFIG_ID, &fresh, 10));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &fresh_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a chunk written 10s ago must be retained under a 1-day window regardless of \
                 how stable its root is: `{key}`; log:\n{log}"
            );
        }
    }

    /// A dry-run lists exactly what it would delete, and deletes nothing.
    #[cfg(unix)]
    #[test]
    fn gc_dry_run_lists_but_deletes_nothing() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, true).expect("dry-run succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "a dry-run must not delete; log:\n{log}"
        );
        let rendered = out.join("\n");
        assert!(
            rendered.contains("would delete"),
            "lists intent: {rendered}"
        );
        for key in &dead_chunks {
            assert!(rendered.contains(key.as_str()), "names `{key}`: {rendered}");
        }
    }

    /// An unreadable `created_at` on a DELETE path fails CLOSED.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_unreadable_timestamp() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 3_600)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 3_600));
        // An orphan whose timestamp is garbage.
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        for key in dead_chunks {
            listing.push((key, "not-a-timestamp".to_owned(), "X".to_owned()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("unreadable") && err.contains("nothing was deleted"),
            "must refuse to reclaim: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when the state is unreadable; log:\n{log}"
        );
    }

    /// A root whose pointer cannot be classified fails CLOSED — we cannot know
    /// what it references, so nothing may be deleted.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_unclassifiable_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let dead = gen_envelope("dead");
        // Root value is pointer-kind but invalid.
        let bad = r#"{"edgezero_kind":"fastly_config_chunks","version":2}"#.to_owned();
        let mut listing = vec![(TEST_CONFIG_ID.to_owned(), stamp_secs_ago(3_600), bad)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("could not classify root") && err.contains("nothing was deleted"),
            "must refuse to reclaim: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when a root is unclassifiable; log:\n{log}"
        );
    }

    /// A listing entry missing a required field fails CLOSED — a defaulted/empty
    /// field could make a real root look like it references nothing, deleting
    /// live chunks.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_malformed_listing_entry() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let good = entry_list_json(&listing);
        // Inject an entry with NO item_value (drop that field entirely).
        let mut array: serde_json::Value = serde_json::from_str(&good).unwrap();
        array.as_array_mut().unwrap().push(serde_json::json!({
            "item_key": "some.__edgezero_chunks.deadbeef.0",
            "created_at": stamp_secs_ago(1000),
        }));
        // Serve that hand-built listing.
        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        fs::write(
            fake.path().join("entries.json"),
            serde_json::to_string(&array).unwrap(),
        )
        .expect("overwrite entries");
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("missing a string") && err.contains("item_value"),
            "must name the missing field: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted on a malformed listing; log:\n{log}"
        );
    }

    /// A failed delete is a non-zero exit that names the failed key(s), so
    /// automation can detect partial failure.
    #[cfg(unix)]
    #[test]
    fn gc_delete_failure_is_non_zero_exit() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let fail_key = dead_chunks.first().expect("a chunk").clone();

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&fail_key),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete must be non-zero");
        assert!(
            err.contains("deletes FAILED") && err.contains(&fail_key),
            "error names the failed key: {err}"
        );
    }

    /// Every reclamation delete passes `--key` + `--auto-yes` and NEVER `--all`.
    #[cfg(unix)]
    #[test]
    fn gc_delete_uses_key_and_auto_yes_never_all() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        let argv_lines: Vec<&str> = log
            .lines()
            .filter(|line| line.starts_with("delete-argv "))
            .collect();
        assert!(!argv_lines.is_empty(), "a delete happened: {log}");
        for line in argv_lines {
            assert!(
                line.contains("--auto-yes"),
                "delete passes --auto-yes: {line}"
            );
            assert!(line.contains("--key="), "delete targets a --key: {line}");
            assert!(
                !line.contains("--all"),
                "delete must NEVER pass --all: {line}"
            );
        }
    }

    /// A non-canonical chunk-like key (short/uppercase SHA, leading-zero index)
    /// is NOT a delete candidate — the destructive validator is canonical-only.
    #[cfg(unix)]
    #[test]
    fn gc_never_deletes_non_canonical_keys() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // Foreign-shaped keys under the reserved infix but not canonical.
        let noncanonical = [
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.abc123.0"), // short sha
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.{}.00", "a".repeat(64)), // leading-zero idx
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.{}.0", "A".repeat(64)), // uppercase
        ];
        for key in &noncanonical {
            listing.push((key.clone(), stamp_secs_ago(604_800), "X".to_owned()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // A key that is NOT canonical is not one we wrote, so it is not a
        // reclamation candidate. It sits in our reserved namespace, though, so
        // it is also not an ordinary root: we cannot say what it is. Since the
        // GC classifier fails closed on any root it cannot classify, the run
        // aborts and names it -- which satisfies this test's invariant (a
        // non-canonical key is never deleted) the strict way.
        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("refusing to reclaim"),
            "expected a fail-closed refusal, got: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &noncanonical {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a non-canonical key must never be deleted: `{key}`; log:\n{log}"
            );
        }
        assert!(
            !log.contains("delete "),
            "a fail-closed run deletes nothing at all; log:\n{log}"
        );
    }

    // ---------- local chunk GC ----------

    /// Config shrinks from chunked back under the 8 000-char limit: the
    /// new value is a direct envelope, so GC prunes every prior chunk.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_prunes_prior_chunks_when_value_shrinks_to_direct() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), chunked)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("second push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");

        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "root holds the direct envelope"
        );
        assert!(
            !contents
                .iter()
                .any(|(key, _)| key.contains(CHUNK_KEY_INFIX)),
            "prior chunks must be pruned: {after}"
        );
    }

    /// A logical key containing the reserved chunk infix is rejected
    /// before any file I/O (it would collide with the chunk namespace).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_rejects_reserved_key() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let bad_key = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");

        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(bad_key.clone(), "{}".to_owned())],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("reserved key must be rejected");
        assert!(err.contains(&bad_key), "error names the key: {err}");
        assert!(
            !fastly_toml.exists(),
            "rejection must happen before any write"
        );
    }

    /// A suspicious prior pointer (pointer-kind but invalid) makes GC
    /// warn and delete nothing — pre-seeded chunk keys must survive.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_warns_on_suspicious_prior_pointer_and_keeps_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // Seed the root with a pointer-kind-but-invalid value AND a real
        // chunk-like key so "no deletes" is non-vacuous.
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":2}\"\n",
            "\"app_config.__edgezero_chunks.deadbeef.0\" = \"seeded-chunk-payload\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must still succeed");

        let combined = out.join("\n");
        assert!(
            combined.contains("skipping chunk GC"),
            "must warn about the suspicious prior pointer: {combined}"
        );

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");
        assert!(
            contents
                .get("app_config.__edgezero_chunks.deadbeef.0")
                .is_some(),
            "pre-seeded chunk key must survive a suspicious-pointer skip: {after}"
        );
        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "new value still written"
        );
    }

    /// Dry-run reports the orphan count and writes nothing.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_reports_orphan_count() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        let envelope_a = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_a)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("seed push");
        let before = fs::read_to_string(&fastly_toml).expect("read");

        let envelope_b = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ "alt": "y".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:02Z".to_owned()))
                .expect("envelope B")
        };
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b)],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run");

        let combined = out.join("\n");
        assert!(
            combined.contains("would delete") && combined.contains("orphan chunks"),
            "dry-run must report orphan count: {combined}"
        );
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            before,
            "dry-run must not edit fastly.toml"
        );
    }

    /// Dry-run of an identical re-push reports zero orphans (new keys
    /// equal prior keys — regression for expanding `new_keys`).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_identical_repush_counts_zero() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("seed push");

        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope)],
                &AdapterPushContext::new(),
                true, // dry_run, same bytes
            )
            .expect("dry-run");

        assert!(
            out.join("\n").contains("would delete 0 orphan chunks"),
            "identical re-push must count 0 orphans: {out:?}"
        );
    }

    /// Dry-run over a suspicious prior pointer reports an unknown count
    /// and does not fail.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_suspicious_prior_pointer_unknown() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":2}\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not fail on suspicious pointer");

        assert!(
            out.join("\n").contains("unknown: suspicious prior pointer"),
            "dry-run must degrade to unknown: {out:?}"
        );
    }

    /// A present-but-malformed `contents` (non-table) is prior state the
    /// real writer would reject — the dry-run count must degrade to
    /// `unknown: could not read prior state`, not silently report 0.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_non_table_contents_unknown() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n",
            "contents = \"bad\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope)],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not fail on malformed contents");

        assert!(
            out.join("\n")
                .contains("unknown: could not read prior state"),
            "non-table contents must degrade to unknown: {out:?}"
        );
    }

    /// A duplicate root key in one batch is rejected before any I/O.
    /// Otherwise the earlier tuple's GC plan would reclaim the chunks the
    /// LAST tuple just installed, leaving the final pointer dangling.
    /// Regression: prior B, batch `[(root, A), (root, B)]` — the root must
    /// still resolve afterwards.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_rejects_duplicate_root_keys() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        let make = |tag: &str| {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
                .expect("envelope")
        };
        let envelope_a = make("aaa");
        let envelope_b = make("bbb");

        // Prior generation B is live.
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("seed push");
        let before = fs::read_to_string(&fastly_toml).expect("read");

        // Duplicate-root batch must be rejected outright.
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[
                    (TEST_CONFIG_ID.to_owned(), envelope_a),
                    (TEST_CONFIG_ID.to_owned(), envelope_b.clone()),
                ],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("duplicate root keys must be rejected");
        assert!(
            err.contains("more than once"),
            "error explains the duplicate: {err}"
        );
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            before,
            "rejection must happen before any write"
        );

        // The live root still resolves to B (nothing was reclaimed).
        let read = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("root must still resolve");
        let ReadConfigEntry::Present(value) = read else {
            panic!("expected Present");
        };
        assert_eq!(value, envelope_b, "root still reconstructs envelope B");
    }

    /// GC of a chunked root must not touch a chunked SIBLING's chunks —
    /// the prefix `app_config.__edgezero_chunks.` must not match
    /// `app_config_staging.__edgezero_chunks.` (shared string prefix).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_gc_preserves_sibling_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        let make = |tag: &str| {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
                .expect("envelope")
        };
        let push = |key: &str, body: String| {
            FastlyCliAdapter
                .push_config_entries_local(
                    dir.path(),
                    Some("fastly.toml"),
                    None,
                    &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                    &[(key.to_owned(), body)],
                    &AdapterPushContext::new(),
                    false,
                )
                .expect("push");
        };

        // app_config gen X, then a chunked sibling, then app_config gen Z.
        push("app_config", make("x1"));
        push("app_config_staging", make("staging"));
        let staging_chunks = chunk_keys_of("app_config_staging", &make("staging"));
        push("app_config", make("z2")); // GCs app_config's gen-X chunks

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");
        for key in &staging_chunks {
            assert!(
                contents.get(key).is_some(),
                "sibling chunk `{key}` must survive app_config GC: {after}"
            );
        }
    }

    // ---- chunk GC helpers ----

    #[test]
    fn reject_reserved_root_keys_accepts_clean_keys() {
        let entries = vec![
            ("app_config".to_owned(), "{}".to_owned()),
            ("app_config_staging".to_owned(), "{}".to_owned()),
        ];
        reject_reserved_root_keys(&entries).expect("clean keys accepted");
    }

    #[test]
    fn reject_reserved_root_keys_rejects_infix_key() {
        let bad = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");
        let entries = vec![(bad.clone(), "{}".to_owned())];
        let err = reject_reserved_root_keys(&entries).expect_err("reserved infix must reject");
        assert!(err.contains(&bad), "error names the key: {err}");
        assert!(err.contains("reserved"), "error explains why: {err}");
    }

    #[test]
    fn orphan_chunk_keys_subtracts_new_keys() {
        let mut new_keys = HashSet::new();
        new_keys.insert("keep".to_owned());
        let plan = FastlyConfigGcPlan {
            new_keys,
            prior_keys: Ok(vec![
                "gone1".to_owned(),
                "keep".to_owned(),
                "gone2".to_owned(),
            ]),
        };
        let orphans = orphan_chunk_keys(&plan).expect("ok");
        assert_eq!(orphans, vec!["gone1".to_owned(), "gone2".to_owned()]);
    }

    #[test]
    fn orphan_chunk_keys_propagates_prior_err() {
        let plan = FastlyConfigGcPlan {
            new_keys: HashSet::new(),
            prior_keys: Err("suspicious".to_owned()),
        };
        orphan_chunk_keys(&plan).unwrap_err();
    }

    #[test]
    fn expand_root_direct_value_has_single_entry() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let (expanded, new_keys, new_root_value) = expand_root(TEST_CONFIG_ID, &envelope).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(new_root_value, envelope);
        assert!(new_keys.contains(TEST_CONFIG_ID));
        assert_eq!(new_keys.len(), 1);
    }

    #[test]
    fn expand_root_chunked_value_carries_pointer_as_root_value() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let (expanded, new_keys, new_root_value) = expand_root(TEST_CONFIG_ID, &envelope).unwrap();
        assert!(expanded.len() >= 2, "chunks + pointer");
        let (last_key, last_value) = expanded.last().unwrap();
        assert_eq!(last_key, TEST_CONFIG_ID);
        assert_eq!(&new_root_value, last_value);
        assert!(new_keys.contains(TEST_CONFIG_ID));
        assert_eq!(new_keys.len(), expanded.len());
    }
}
