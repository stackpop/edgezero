use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::process::id as process_id;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::RUNTIME_ENV_STORE_NAME;
use crate::chunked_config::{
    CHUNK_KEY_INFIX, GcPointer, GcRootValue, ResolveFailure, chunk_key_generation, chunk_key_index,
    chunk_lengths, gc_classify_root, gc_verify_generation, prepare_fastly_config_entries,
    prior_chunk_keys, resolve_fastly_config_value_typed, sha256_hex, value_announces_our_kind,
    value_is_future_format, value_is_inert_foreign, verify_writer_split_layout,
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

/// Base name of the staging twin of [`RUNTIME_ENV_STORE_NAME`]. The actual store is
/// named PER SERVICE — [`staging_selector_store_name`] appends the service id —
/// because Fastly config stores are account-wide, versionless resources: a
/// single shared twin would let a staged deploy of service B destructively
/// overwrite the selectors a staged version of service A is reading.
///
/// A staged deploy clones the active version, and a clone inherits its resource
/// links — so without a second store the staged version reads production's
/// selector, and therefore production's config. Fastly resource links are
/// per-version and carry an overridable NAME, so the staged draft links THIS
/// store under the name `edgezero_runtime_env`. The runtime opens that name and
/// gets staged config; the active version is untouched.
const RUNTIME_ENV_STAGING_STORE_PREFIX: &str = "edgezero_runtime_env_staging";

/// Env var carrying the Fastly API token (read by the Fastly CLI and
/// forwarded to the Fastly API via the `Fastly-Key` header). Part of
/// the Fastly staging lifecycle.
const FASTLY_API_TOKEN_ENV: &str = "FASTLY_API_TOKEN";
/// Env var carrying the default Fastly service id, used when
/// `--service-id` is not passed explicitly.
const FASTLY_SERVICE_ID_ENV: &str = "FASTLY_SERVICE_ID";

/// Bound every Fastly API call so an outage or a stalled connection cannot hang the
/// job — potentially until the surrounding workflow timeout, hours later — during a
/// time-sensitive operation like a rollback. `curl` exits 28 when either limit is
/// hit, which `curl_config_capture` turns into an explicit timeout error.
const FASTLY_API_CONNECT_TIMEOUT_SECS: u64 = 10;
const FASTLY_API_MAX_TIME_SECS: u64 = 30;
/// curl's exit code for an operation that exceeded `--connect-timeout`/`--max-time`.
const CURL_EXIT_TIMEOUT: i32 = 28;

/// Flags `fastly compute update` accepts that take a VALUE (either
/// `--flag value` or `--flag=value`). Verified against
/// `fastly compute update --help` (Fastly CLI v15): the command's
/// `--service-id`/`-s`, `--service-name`, `--package`/`-p`, `--version`,
/// plus the global `--token`/`-t`.
const COMPUTE_UPDATE_VALUE_FLAGS: &[&str] = &[
    "--service-id",
    "-s",
    "--service-name",
    "--package",
    "-p",
    "--version",
    "--token",
    "-t",
];

/// Boolean flags `fastly compute update` accepts: the command's
/// `--autoclone` plus the Fastly CLI globals. NOTE the absence of
/// `--comment` -- `compute update` does NOT support it (unlike
/// `compute deploy`), which is why an operator `--comment` is routed to
/// `service-version update` instead (see `deploy_staged`).
const COMPUTE_UPDATE_BOOL_FLAGS: &[&str] = &[
    "--autoclone",
    "--accept-defaults",
    "-d",
    "--auto-yes",
    "-y",
    "--debug-mode",
    "--non-interactive",
    "-i",
    "--quiet",
    "-q",
    "--verbose",
    "-v",
];

/// Hard-error message for a value written by a NEWER format this v1 CLI must not
/// overwrite. Shared by the read path so the wording stays consistent.
const FUTURE_FORMAT_READ_ERROR: &str = "the remote value uses a config format this CLI version does not recognise (a newer \
     `edgezero_kind` or envelope/pointer version); UPGRADE the CLI to push to this store rather \
     than overwrite a newer format.";

struct FastlyCliAdapter;

/// An operator passthrough arg list split for a staged deploy (see
/// `split_staged_passthrough`).
struct StagedPassthrough {
    /// The `--comment` value, applied to the version separately via
    /// `fastly service-version update --comment` (`compute update` has
    /// no `--comment` flag).
    comment: Option<String>,
    /// Args `compute update` does not support; dropped with a warning
    /// rather than forwarded (forwarding them makes the CLI exit
    /// non-zero and fails the whole staged deploy).
    dropped: Vec<String>,
    /// Args that `fastly compute update` actually supports.
    forwarded: Vec<String>,
}

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
    /// The root keys retained as live/protected — the config entries GC will NOT
    /// delete, sorted. Surfaced so a run shows what it is KEEPING, not only what
    /// it would delete, making the sweep reviewable.
    kept_roots: Vec<String>,
    live_count: usize,
    retained_recent: usize,
    roots: usize,
    /// Chunk-shaped entries we could NOT prove our writer produced, so left
    /// untouched. Surfaced so an operator can see we declined to judge them.
    unprovable: usize,
    /// Non-fatal problems to print — see `GcClassification::warnings`.
    warnings: Vec<String>,
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
    /// Non-fatal problems the operator should see — currently roots that are
    /// not runtime-readable and so can never be reclaimed automatically.
    warnings: Vec<String>,
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

/// An exclusive, cross-process advisory lock covering a local `fastly.toml`
/// rewrite. Serialises concurrent pushes so their read-modify-write cycles
/// cannot interleave and lose each other's edits.
///
/// The lock is a persistent sidecar file next to the manifest. It is never
/// unlinked — deleting it would reintroduce a create/lock race between two
/// processes each making their own lock file. Dropping the guard releases the
/// OS lock (closing the file descriptor). `File::lock` is advisory, so it only
/// coordinates other lockers, which is exactly the pushes we control.
struct ManifestLock {
    _file: fs::File,
    /// The REAL file the lock guards, resolved through any symlink. Callers read
    /// and replace THIS path, so every alias operates on one target.
    target: PathBuf,
}

/// Removes a staging temp file on drop unless disarmed — so every early return
/// (permission failure, write failure, rename failure) cleans up after itself.
struct TempFileGuard {
    path: Option<PathBuf>,
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
            // Fastly staging lifecycle.
            AdapterAction::DeployStaged => deploy_staged(args),
            AdapterAction::EmitVersion => emit_active_version(args),
            AdapterAction::Healthcheck => healthcheck(args),
            AdapterAction::Rollback => rollback(args),
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

    fn preflight_config_write(&self, key: &str, body: &str) -> Result<(), String> {
        // Reject an infeasible push here, BEFORE the CLI's remote read, so it
        // fails offline rather than after a list/describe. The write path
        // re-checks, so this is a strict early gate, not the only one.
        //
        // An empty key is writer-valid but resolver-invalid (canonical chunk
        // parsing rejects an empty root); reject it before any I/O.
        if key.is_empty() {
            return Err(
                "config key is empty; provide a store id or a non-empty `--key`".to_owned(),
            );
        }
        let entry = [(key.to_owned(), String::new())];
        reject_reserved_root_keys(&entry)?;
        // Run the full chunk expansion OFFLINE (no I/O): exactly what the write
        // path does, so every body-dependent feasibility failure — the root key
        // over the store limit, a DERIVED chunk key over it once the value
        // chunks, or a pointer that would not fit the entry limit — is caught
        // here, before the remote read, instead of after it.
        prepare_fastly_config_entries(key, body)?;
        Ok(())
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
        // the runtime opens by name (see `runtime_env_config` in
        // lib.rs). Provision owns the store creation alongside the
        // operator's declared stores so the runtime override path is
        // wired correctly out of the box; if the store already appears
        // in `[setup.config_stores.edgezero_runtime_env]`, skip.
        let runtime_env_kind = "config";
        let runtime_env_name = RUNTIME_ENV_STORE_NAME;
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
            // NB: this store is what the ACTIVE (production) service reads. The
            // example must never point it at a staging key — following that would
            // make production serve staged config. Staged versions get their own
            // selector via `edgezero_runtime_env_staging`, wired automatically by
            // a staged deploy; nothing here should be edited to stage config.
            let mut line = format!(
                "created fastly {runtime_env_kind}-store `{runtime_env_name}` (EdgeZero runtime override store, read by the ACTIVE version); appended setup tables to {}\n  Provision writes non-default store-name mappings below. Config stores still select their logical id as the default key.\n  To point PRODUCTION at a different config key, and only then:\n    fastly config-store-entry update --store-id=<STORE-ID> --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY --value=<production-key> --upsert\n  Do NOT set a `_staging` key here: staged config is isolated by a per-service `{RUNTIME_ENV_STAGING_STORE_PREFIX}_<service-id>` store, which a staged deploy creates and links automatically.",
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

        out.extend(persist_runtime_env_store_name_entries(stores, dry_run)?);

        // The STAGING twin of the runtime-override store is created and
        // populated entirely by a staged deploy (see
        // `relink_runtime_env_for_staging` → `mirror_production_to_staging`), so
        // it always mirrors production's CURRENT overrides. Provision does not
        // touch it: a twin populated here would drift the moment an operator
        // edited a production override.

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
        // Expand each logical root into its physical entries (chunks + pointer, or
        // a single direct entry). Collecting them all first surfaces a
        // pointer-too-large error before touching the remote store. A cloud push
        // does NOT reclaim, so — unlike the local path — it keeps no per-root
        // keep-set / root-value GC bookkeeping.
        let mut physical_entries: Vec<(String, String)> = Vec::new();
        for (key, body) in entries {
            let (expanded, ..) = expand_root(key, body)?;
            physical_entries.extend(expanded);
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
        let resolved_id =
            resolve_remote_config_store_id(name)?.ok_or_else(|| no_matching_store_error(name))?;
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
        // Preflight: refuse if a generated chunk key would clobber an existing
        // root-like sibling in the remote store. Uses a completeness-strict key
        // listing (value-tolerant) and describes only the rare colliding keys.
        let remote_keys = list_config_store_keys(&resolved_id)?;
        reject_generated_key_collisions(&physical_entries, &remote_keys, |chunk_key| {
            fetch_remote_config_store_entry(&resolved_id, chunk_key).map(Some)
        })?;
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
        // A TYPED absence: `Ok(None)` (list succeeded, no store matched) is the
        // only path to MissingStore. Any operational failure stays `Err` and fails
        // closed -- an incomplete read must never read as absence and authorise an
        // overwrite of healthy remote state.
        let Some(store_id) = resolve_remote_config_store_id(name)? else {
            return Ok(ReadConfigEntry::MissingStore);
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
            let stdout = strict_stdout(output.stdout, "config-store-entry describe --json")?;
            // Parse the JSON and extract the `item_value` field.
            let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
                format!(
                    "failed to parse `fastly config-store-entry describe` JSON (parse error \
                     redacted; response: {})",
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
            //
            // A chunk describe that fails could not be FULLY read. Confirm whether
            // the chunk is genuinely ABSENT against the complete store listing
            // (authoritative), never the describe 404:
            //   - CONFIRMED absent → resolve to a repairable `Corrupt`. The blob
            //     spec makes persistent chunk loss repairable by re-pushing, so a
            //     push can overwrite to fix it.
            //   - present-but-unreadable, or the listing itself failed →
            //     `fetch_failed`: an incomplete read that must be a HARD error,
            //     never an overwritable value.
            let store_keys: RefCell<Option<Result<HashSet<String>, String>>> = RefCell::new(None);
            let fetch_failed: Cell<bool> = Cell::new(false);
            let resolved = resolve_fastly_config_value_typed(key, value.to_owned(), |chunk_key| {
                match fetch_remote_config_store_entry(&store_id, chunk_key) {
                    Ok(found) => Ok(Some(found)),
                    Err(_describe_err) => {
                        match confirm_key_absent_cached(&store_keys, &store_id, chunk_key) {
                            Ok(true) => Ok(None), // genuinely gone → repairable Corrupt
                            Ok(false) => {
                                fetch_failed.set(true);
                                Err("a referenced chunk is present in the store but its value \
                                     could not be read (incomplete read)"
                                    .to_owned())
                            }
                            Err(list_err) => {
                                fetch_failed.set(true);
                                Err(list_err)
                            }
                        }
                    }
                }
            });
            return classify_resolved_read(resolved, value, fetch_failed.get());
        }
        // The describe failed. Absence is CONFIRMED only by a complete listing
        // that omits the key -- never by a describe 404, which a proxy/endpoint or
        // auth failure produces just the same. A present key (or a listing that
        // itself fails) is a hard error, so two such incomplete reads can never
        // pass the pre-write recheck and authorise an overwrite.
        if confirm_entry_absent(&store_id, key)? {
            return Ok(ReadConfigEntry::MissingKey);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` exited \
             with status {} but the key IS present in the store listing (an operational failure, \
             not absence); nothing was changed.\nstderr: {}",
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
        // A prior-state read failure must never BLOCK the command: the diff just
        // cannot be computed, so it degrades to `Unsupported` ("cannot diff").
        // Downstream, a dry-run then reaches the writer's orphan-count
        // degradation (spec 12.x) and a real push reaches the writer, which
        // fails fatally on malformed TOML or overwrites otherwise. Erroring here
        // would newly fail a dry-run that reads nothing today.
        let raw = match fs::read_to_string(&fastly_path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(ReadConfigEntry::MissingStore);
            }
            Err(_err) => {
                return Ok(ReadConfigEntry::Unsupported(
                    "local fastly.toml could not be read; cannot diff the prior value",
                ));
            }
        };
        let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
            return Ok(ReadConfigEntry::Unsupported(
                "local fastly.toml is not valid TOML; cannot diff the prior value",
            ));
        };
        // Descend `[local_server.config_stores.<name>.contents]` level by level.
        // At each level an ABSENT key means the store isn't seeded yet
        // (MissingStore), but a key that is PRESENT yet not a table is malformed
        // store state — distinct outcomes. Collapsing the malformed case into
        // MissingStore (as a plain `.get().and_then()` chain does) would render an
        // inaccurate "all values added" diff, so it degrades to "cannot diff".
        //
        // `descend` returns Ok(None) for absent (-> MissingStore) and
        // Err(Unsupported) for present-but-not-a-table.
        let descend = |parent: &'_ toml_edit::Item,
                       child: &str|
         -> Result<Option<toml_edit::Item>, ReadConfigEntry> {
            match parent.get(child) {
                None => Ok(None),
                Some(item) if item.is_table_like() => Ok(Some(item.clone())),
                Some(_) => Err(ReadConfigEntry::Unsupported(
                    "a local config-store parent table is not a table; cannot diff the prior value",
                )),
            }
        };
        let root_item = toml_edit::Item::Table(doc.as_table().clone());
        let contents_item = (|| {
            let Some(local_server) = descend(&root_item, "local_server")? else {
                return Ok(None);
            };
            let Some(config_stores) = descend(&local_server, "config_stores")? else {
                return Ok(None);
            };
            let Some(store_tbl) = descend(&config_stores, name)? else {
                return Ok(None);
            };
            descend(&store_tbl, "contents")
        })();
        let contents = match contents_item {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(ReadConfigEntry::MissingStore),
            Err(unsupported) => return Ok(unsupported),
        };
        // `contents` MUST be a table of `key = "value"` pairs. (Guaranteed by
        // `descend` above, but re-borrow as a table to index it.)
        let Some(contents_tbl) = contents.as_table_like() else {
            return Ok(ReadConfigEntry::Unsupported(
                "local config-store `contents` is not a table; cannot diff the prior value",
            ));
        };
        // The contents table is `key = "value"` pairs.
        match contents_tbl.get(key) {
            Some(item) => {
                let Some(value) = item.as_str() else {
                    return Ok(ReadConfigEntry::Unsupported(
                        "the local prior value is not a string; cannot diff the prior value",
                    ));
                };
                // Resolve chunk pointers using the same toml contents table.
                let resolved =
                    resolve_fastly_config_value_typed(key, value.to_owned(), |chunk_key| {
                        match contents_tbl.get(chunk_key) {
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
                        }
                    });
                // Same taxonomy as the cloud read, so recovery is uniform across
                // targets: a valid envelope is `Present`; a non-envelope or
                // corrupt/incomplete value is `Corrupt` (the local writer's
                // fail-soft then overwrites it); an unknown/future kind is a hard
                // error (do not clobber a newer format). There is no
                // infrastructure fetch here -- the chunks are read from the local
                // TOML table -- so `fetch_failed` is always false.
                classify_resolved_read(resolved, value, false)
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

impl ManifestLock {
    fn acquire(manifest_path: &Path) -> Result<Self, String> {
        // Key the lock on the REAL target, so a symlinked manifest and a direct
        // path to the same file acquire the SAME lock rather than two different
        // sidecars. Every manifest writer (config push AND provision) takes this
        // lock, so their read-modify-writes serialise instead of clobbering.
        let target = canonical_manifest_target(manifest_path)?;
        // A hard-linked manifest cannot be safely replaced: two hard links share
        // one inode but have distinct pathnames, so they key DIFFERENT sidecar
        // locks (no mutual exclusion), and the atomic rename swaps in a NEW inode,
        // breaking the link. We cannot detect the other names, so fail closed
        // rather than silently diverge or break the link.
        reject_hard_linked_manifest(&target)?;
        let dir = target.parent().unwrap_or_else(|| Path::new("."));
        let file_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("fastly.toml");
        let lock_path = dir.join(format!(".{file_name}.edgezero-lock"));
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| format!("failed to open lock file {}: {err}", lock_path.display()))?;
        // Blocks until any other writer holding the lock releases it.
        file.lock()
            .map_err(|err| format!("failed to lock {}: {err}", lock_path.display()))?;
        // Re-check AFTER the (possibly long) lock wait: a hard link created while
        // we blocked would not have been visible to the pre-lock check above. The
        // replacement path re-checks once more immediately before the rename.
        reject_hard_linked_manifest(&target)?;
        Ok(Self {
            _file: file,
            target,
        })
    }

    /// The real file this lock guards. Callers read and replace THIS path.
    fn target(&self) -> &Path {
        &self.target
    }
}

impl TempFileGuard {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _cleanup = fs::remove_file(path);
        }
    }
}

/// Resolve a manifest path to the REAL file every alias shares, so a symlink and
/// a direct path lock and replace the SAME target. An existing file (or symlink)
/// canonicalizes directly; a not-yet-created file canonicalizes via its parent
/// so a fresh `fastly.toml` still keys on a stable location.
///
/// FAILS CLOSED on an ambiguous chain: a symlink whose target cannot be read, or
/// a chain too deep / cyclic, returns `Err` rather than falling back to a writable
/// path that could replace an intermediate link.
fn canonical_manifest_target(path: &Path) -> Result<PathBuf, String> {
    // Follow the WHOLE symlink chain to the final target -- each hop may itself be
    // a dangling symlink (fastly.toml -> middle.toml -> missing.toml). We write at
    // the final target, preserving every intermediate link, and a direct writer to
    // that same target keys on the same lock.
    let mut current = path.to_owned();
    // Bounded to avoid spinning on a symlink cycle (canonicalize would ELOOP).
    for _ in 0..40_u32 {
        // Fully resolvable => the real existing file.
        if let Ok(real) = fs::canonicalize(&current) {
            return Ok(real);
        }
        // Otherwise, if this hop is a symlink, follow one link and continue.
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(&current) {
                Ok(link) => {
                    current = if link.is_absolute() {
                        link
                    } else {
                        // A relative link resolves against the DIRECTORY holding it.
                        current
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(link)
                    };
                }
                // A symlink we cannot read: refuse rather than guess a target.
                Err(err) => {
                    return Err(format!(
                        "could not read the manifest symlink `{}` ({err}); refusing to write",
                        current.display()
                    ));
                }
            },
            // Not a symlink -- a plain not-yet-created file, or the final dangling
            // target: this is where the write should land.
            _ => return Ok(canonicalize_parent_join(&current)),
        }
    }
    // Exhausted the hop budget: a cyclic or absurdly deep chain. Fail closed.
    Err(format!(
        "the manifest symlink chain starting at `{}` is too deep or cyclic; refusing to write",
        path.display()
    ))
}

/// Canonicalize `path`'s PARENT (which should exist) and rejoin the file name,
/// so a not-yet-created file still resolves to a stable absolute location.
fn canonicalize_parent_join(path: &Path) -> PathBuf {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path.file_name().unwrap_or(path.as_os_str());
    match fs::canonicalize(parent) {
        Ok(real_parent) => real_parent.join(file_name),
        Err(_) => path.to_owned(),
    }
}

/// Refuse to operate on a manifest that has MORE THAN ONE hard link. Such a file
/// cannot be replaced safely: the atomic rename installs a new inode (breaking
/// the link), and the path-based lock cannot serialise writers arriving via the
/// other names. Fail closed with a fix. A not-yet-created file, or a filesystem
/// that does not report a link count, is left alone.
///
/// The link count is read via the platform `MetadataExt` -- `nlink()` on Unix,
/// `number_of_links()` on Windows (both stable, no extra deps) -- so Windows
/// hard-link aliases are caught too, not just Unix ones. On any other target the
/// count is unknown and the file is left alone.
fn reject_hard_linked_manifest(target: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let link_count: Option<u64> = {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(target).ok().map(|meta| meta.nlink())
    };
    #[cfg(windows)]
    let link_count: Option<u64> = {
        use std::os::windows::fs::MetadataExt as _;
        fs::metadata(target)
            .ok()
            .and_then(|meta| meta.number_of_links())
            .map(u64::from)
    };
    #[cfg(not(any(unix, windows)))]
    let link_count: Option<u64> = None;

    if let Some(count) = link_count
        && count > 1
    {
        return Err(format!(
            "{} has multiple hard links (link count {count}); refusing to replace it -- an atomic \
             rename would break the link and concurrent writers via the other names could \
             diverge. Remove the extra hard link(s), or use a symlink instead.",
            target.display(),
        ));
    }
    Ok(())
}

/// Fetch a single entry value from a remote Fastly Config Store entry by
/// key, using `fastly config-store-entry describe --store-id=<id> --key=<k>
/// --json`. Used by the chunk-pointer resolver to fan out to chunk entries.
///
/// `Ok(value)` when the entry exists; `Err` on ANY failure, INCLUDING a
/// not-found. Absence is NOT decided here (a describe 404 is not proof) -- the
/// caller confirms it against the complete store listing.
///
/// # Errors
/// Returns an error if `fastly` isn't on `PATH`, spawning fails, the JSON
/// cannot be parsed, or the CLI exits with a non-zero status (not-found included).
fn fetch_remote_config_store_entry(store_id: &str, key: &str) -> Result<String, String> {
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
        let stdout = strict_stdout(output.stdout, "config-store-entry describe --json")?;
        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
            format!(
                "failed to parse `fastly config-store-entry describe` JSON for key \
                 `{key}` (parse error redacted; response: {})",
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
        return Ok(value.to_owned());
    }
    // `Err` on ANY non-success, INCLUDING a not-found. A describe 404 alone is not
    // proof of absence -- a proxy/endpoint 404, an auth 404, or a gateway error
    // all look the same -- so the caller CONFIRMS a genuine absence against the
    // complete store listing rather than trusting this stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` \
         exited with status {}\nstderr: {}",
        output.status,
        redact_stderr(&stderr)
    ))
}

/// The COMPLETE set of item keys in a store, via `config-store-entry list`.
///
/// Absence is CONFIRMED against this, never against a describe 404: the listing
/// is completeness-strict (fails closed on a paginated / non-bare-array view and
/// on a duplicate key), so a key's absence from it is authoritative. Tolerant of
/// empty item VALUES -- only keys are needed to confirm presence.
fn list_config_store_keys(store_id: &str) -> Result<HashSet<String>, String> {
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
    let stdout = strict_stdout(output.stdout, "config-store-entry list --json")?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
        format!(
            "failed to parse `fastly config-store-entry list` JSON (parse error redacted; \
             response: {})",
            redact_describe_response(&stdout)
        )
    })?;
    let array = parsed.as_array().ok_or_else(|| {
        format!(
            "refusing to confirm absence: `fastly config-store-entry list --json` did not return a \
             bare array (response: {}). A paginated or partial view could hide a present key and \
             turn it into a false absence that authorises an overwrite.",
            redact_describe_response(&stdout)
        )
    })?;
    let mut keys = HashSet::with_capacity(array.len());
    for (idx, entry) in array.iter().enumerate() {
        let key = entry
            .get("item_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry list` entry #{idx} is missing a string `item_key`; \
                     refusing to confirm absence on an unreadable listing"
                )
            })?;
        if key.is_empty() {
            return Err(format!(
                "`fastly config-store-entry list` entry #{idx} has an empty `item_key`; refusing \
                 to confirm absence on an unreadable listing"
            ));
        }
        if !keys.insert(key.to_owned()) {
            return Err(format!(
                "`fastly config-store-entry list` returned duplicate key `{key}`; refusing to \
                 confirm absence on an ambiguous listing"
            ));
        }
    }
    Ok(keys)
}

/// Confirm `key` is ABSENT from the store via a complete listing (authoritative).
/// `Ok(true)` = the listing succeeded and omits the key. `Ok(false)` = the key IS
/// present (so a describe failure on it was operational, not absence). `Err` = the
/// listing itself failed. All three fail closed for the caller: only `Ok(true)`
/// is a genuine absence.
fn confirm_entry_absent(store_id: &str, key: &str) -> Result<bool, String> {
    Ok(!list_config_store_keys(store_id)?.contains(key))
}

/// Cached form of [`confirm_entry_absent`] for chunk fetches: lists the store at
/// most ONCE per read (a whole lost generation would otherwise list per chunk).
fn confirm_key_absent_cached(
    cache: &RefCell<Option<Result<HashSet<String>, String>>>,
    store_id: &str,
    key: &str,
) -> Result<bool, String> {
    let mut slot = cache.borrow_mut();
    if slot.is_none() {
        *slot = Some(list_config_store_keys(store_id));
    }
    match slot.as_ref() {
        Some(Ok(keys)) => Ok(!keys.contains(key)),
        Some(Err(err)) => Err(err.clone()),
        // Unreachable: populated just above. Fail closed rather than unwrap.
        None => Err("internal error: store listing cache was not populated".to_owned()),
    }
}

/// Convert `fastly` stdout to a `String`, FAILING CLOSED on invalid UTF-8 rather
/// than substituting U+FFFD. A lossy replacement inside a JSON string could
/// mutate a stored root value or chunk and yield parseable-but-WRONG data on a
/// path that drives an overwrite or a deletion, violating the exact-read
/// invariant. Diagnostics only ever see redacted output, so stderr stays lossy.
fn strict_stdout(stdout: Vec<u8>, command: &str) -> Result<String, String> {
    String::from_utf8(stdout).map_err(|_err| {
        format!(
            "`fastly {command}` returned non-UTF-8 output; refusing to act on it -- a lossy \
             conversion could mutate a stored value. Nothing was changed."
        )
    })
}

/// Does `body` parse AND integrity-verify as a `BlobEnvelope`?
///
/// The typed-config key must hold a valid envelope. A resolved chunk pointer
/// already reconstructs and verifies one; a DIRECT or foreign value is checked
/// here. A value that is not a verifying envelope (invalid JSON, missing fields,
/// or a SHA mismatch) is corrupt FOR THE PUSH -- something to overwrite, not to
/// diff against.
fn body_is_valid_envelope(body: &str) -> bool {
    use edgezero_core::blob_envelope::BlobEnvelope;
    serde_json::from_str::<BlobEnvelope>(body).is_ok_and(|envelope| envelope.verify().is_ok())
}

/// Map a `resolve_fastly_config_value` result to a read outcome, distinguishing
/// the cases that must NOT be treated as overwritable corruption:
///
/// - a FUTURE format (unknown/newer `edgezero_kind`, or a bumped envelope/pointer
///   `version`) → a hard error: overwriting a newer format with this v1 CLI would
///   lose it. Checked FIRST. Detected two ways: on the raw stored value (a direct
///   future envelope, or a future pointer version), AND via a typed
///   [`ResolveFailure::FutureFormat`] from the resolver -- the ONLY signal for a
///   newer INNER envelope reassembled from v1 chunks, which the raw value alone
///   cannot reveal.
/// - a resolve error where a chunk FETCH failed for infrastructure reasons
///   (`fetch_failed`) → a hard error: the read was incomplete, so a push must not
///   overwrite healthy remote state.
/// - `Ok(body)` that verifies as an envelope → `Present`.
/// - `Ok(body)` that is NOT a valid envelope (a malformed direct value, a SHA
///   mismatch, a foreign non-envelope) → `Corrupt` (repairable by overwrite).
/// - any other resolve error (bad/missing chunk, malformed pointer) → `Corrupt`.
fn classify_resolved_read(
    resolved: Result<String, ResolveFailure>,
    raw_value: &str,
    fetch_failed: bool,
) -> Result<ReadConfigEntry, String> {
    // A newer format is refused BEFORE anything else: on the raw value (direct
    // future envelope or future pointer version) OR when the resolver typed the
    // failure as a newer format (a future inner envelope only knowable after the
    // chunks are reassembled). Overwriting a newer format with this v1 CLI would
    // lose it.
    if value_is_future_format(raw_value)
        || resolved
            .as_ref()
            .err()
            .is_some_and(ResolveFailure::is_future_format)
    {
        return Err(FUTURE_FORMAT_READ_ERROR.to_owned());
    }
    match resolved {
        // An INFRASTRUCTURE fetch failure: the read was incomplete, so a push must
        // not overwrite. The resolver's message is already redacted (it names only
        // a chunk POSITION, never a value), so surface it for diagnostics.
        Err(err) if fetch_failed => Err(format!(
            "a chunk fetch failed while reading the remote value ({}); the remote was not fully \
             read, so nothing was changed. Fix connectivity/auth and retry.",
            err.into_message()
        )),
        Ok(body) if body_is_valid_envelope(&body) => Ok(ReadConfigEntry::Present(body)),
        Ok(_) => Ok(ReadConfigEntry::Corrupt(
            "remote value is not a valid config envelope; a push will overwrite it",
        )),
        // A confirmed-absent chunk, a hash mismatch, or a malformed pointer: the
        // value was fully read and is provably unusable, so a push repairs it.
        Err(_) => Ok(ReadConfigEntry::Corrupt(
            "remote prior value could not be resolved (corrupt or incomplete chunk state); a push \
             will overwrite it",
        )),
    }
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
    let doc: toml_edit::DocumentMut = raw.parse().map_err(|_err| {
        format!(
            "failed to parse {} as TOML (details redacted: the error can quote a stored value)",
            path.display()
        )
    })?;
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
    let doc: toml_edit::DocumentMut = raw.parse().map_err(|_err| {
        format!(
            "failed to parse {} as TOML (details redacted: the error can quote a stored value)",
            path.display()
        )
    })?;
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

    // Provision writes the SAME manifest as `config push --local`; take the same
    // lock so a concurrent provision and push serialise instead of clobbering
    // each other's edit, and operate on the real target the lock resolved.
    let lock = ManifestLock::acquire(path)?;
    let target = lock.target();

    let raw = match fs::read_to_string(target) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", target.display())),
    };
    let mut doc: DocumentMut = raw.parse().map_err(|_err| {
        format!(
            "failed to parse {} as TOML (details redacted: the error can quote a stored value)",
            target.display()
        )
    })?;

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

    atomically_replace_file(target, &raw, &doc.to_string())?;
    Ok(())
}

/// Write the local-server config-store entries to `fastly.toml`:
/// `[local_server.config_stores.<platform_name>]` becomes
/// `format = "inline-toml"`, and `[local_server.config_stores.<platform_name>.contents]`
/// gets the flat `key = "value"` pairs (overwriting any previous
/// values). Idempotent — re-running just rewrites `contents`. Other
/// blocks in `fastly.toml` (setup, scripts, the actual `[local_server]`
/// secret stores, etc.) are preserved via `toml_edit`.
/// Refuse before writing if any GENERATED chunk key would clobber an existing
/// value that is itself ROOT-LIKE (announces our `edgezero_kind`, is a newer
/// format, or classifies as a valid root) or that has a NESTED generation beneath
/// it. Chunk keys are content-addressed, so such a collision is pathological, but
/// overwriting one would destroy live or foreign config -- so fail closed.
///
/// Logical ROOT keys are excluded here; overwriting a root is governed by the
/// downgrade/future guards. `sibling_keys` is the complete set of existing store
/// keys (for the nested-generation check); `existing_value_at` fetches the value
/// at a colliding key (only called for keys already present).
fn reject_generated_key_collisions(
    entries: &[(String, String)],
    sibling_keys: &HashSet<String>,
    mut existing_value_at: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<(), String> {
    for (key, _) in entries {
        if !key.contains(CHUNK_KEY_INFIX) {
            continue; // a logical root; the root-overwrite guards cover it
        }
        let has_nested_generation = sibling_keys
            .iter()
            .any(|other| other != key && chunk_key_generation(key, other).is_some());
        let clobbers_root_like = sibling_keys.contains(key)
            && existing_value_at(key)?.is_some_and(|value| {
                value_announces_our_kind(&value)
                    || value_is_future_format(&value)
                    || gc_classify_root(key, &value).is_ok()
            });
        if has_nested_generation || clobbers_root_like {
            return Err(format!(
                "refusing to push: the generated chunk key `{key}` already holds a value that is \
                 itself a root (or has a nested generation beneath it); overwriting it could \
                 destroy live or foreign config. Nothing was changed."
            ));
        }
    }
    Ok(())
}

/// [`reject_generated_key_collisions`] against a local `contents` table.
fn reject_local_generated_key_collisions(
    contents_tbl: &toml_edit::Table,
    entries: &[(String, String)],
) -> Result<(), String> {
    let sibling_keys: HashSet<String> = contents_tbl
        .iter()
        .map(|(existing_key, _)| existing_key.to_owned())
        .collect();
    reject_generated_key_collisions(entries, &sibling_keys, |chunk_key| {
        Ok(contents_tbl
            .get(chunk_key)
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned))
    })
}

/// Ensure a local config-store entry is `format = "inline-toml"` -- the only
/// format compatible with the inline `contents` this writer emits.
///
/// REFUSES an existing non-inline store rather than converting it. A
/// `format = "json"` / `"file"` store points at an EXTERNAL file that this writer
/// cannot safely rewrite: leaving `file` in place produces a manifest Viceroy
/// rejects ("unrecognized key 'file'"), and removing it would silently discard
/// the sibling entries that file holds (this writer only inserts the pushed
/// root). Migration is the operator's explicit choice, not a silent side effect.
fn ensure_inline_toml_format(
    store_tbl: &mut toml_edit::Table,
    platform_name: &str,
) -> Result<(), String> {
    let existing = store_tbl.get("format").and_then(toml_edit::Item::as_str);
    match existing {
        Some("inline-toml") => Ok(()),
        Some(other) => Err(format!(
            "refusing to push: `local_server.config_stores.{platform_name}` uses `format = \
             \"{other}\"` (an external-file store), which is incompatible with the inline \
             `contents` this command writes. Converting it here would either produce a manifest \
             the local server rejects or silently discard the sibling entries the external file \
             holds. Migrate the store to `format = \"inline-toml\"` (or a fresh store id) yourself, \
             then re-run. Nothing was changed."
        )),
        None => {
            // A brand-new or format-less entry: this writer owns it, so stamp the
            // inline format it is about to fill.
            store_tbl.insert("format", toml_edit::value("inline-toml"));
            Ok(())
        }
    }
}

/// TOCTOU guard for the LOCAL writer: refuse to overwrite a root that now holds a
/// NEWER format, classified HERE under the write lock. The generic push's
/// pre-push future-format check ran BEFORE the lock, so a newer writer could have
/// installed a v2 value in between; without this the old writer would clobber it.
///
/// The raw value alone does not reveal a future INNER envelope hidden behind a
/// valid v1 pointer -- that is only knowable after reconstruction. So each root
/// that is one of our pointers is RESOLVED against the locked `contents` table
/// (its chunks live there too); a typed `FutureFormat` from the resolver is
/// refused just like a raw future value.
fn reject_future_local_roots(
    contents_tbl: &toml_edit::Table,
    gc_roots: &[(String, HashSet<String>)],
) -> Result<(), String> {
    for (root_key, _) in gc_roots {
        let Some(existing) = contents_tbl.get(root_key).and_then(toml_edit::Item::as_str) else {
            continue;
        };
        // Raw check: a direct future envelope, a future pointer version, or an
        // unknown `edgezero_kind`.
        let mut is_future = value_is_future_format(existing);
        if !is_future {
            // Resolve against the locked contents to catch a future INNER envelope
            // behind a valid v1 pointer. Only `FutureFormat` blocks the write; a
            // corrupt/incomplete v1 prior stays overwritable.
            let resolved = resolve_fastly_config_value_typed(root_key, existing.to_owned(), |ck| {
                Ok(contents_tbl
                    .get(ck)
                    .and_then(toml_edit::Item::as_str)
                    .map(str::to_owned))
            });
            is_future = matches!(resolved, Err(err) if err.is_future_format());
        }
        if is_future {
            return Err(format!(
                "refusing to overwrite `{root_key}`: the local store now holds a value in a newer \
                 format this CLI does not recognise (installed since the pre-push check). Upgrade \
                 the CLI rather than clobber a newer format. Nothing was changed."
            ));
        }
    }
    Ok(())
}

fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
    gc_roots: &[(String, HashSet<String>)],
) -> Result<Vec<String>, String> {
    use toml_edit::{DocumentMut, Item, Table, Value, table};

    // Hold a cross-process advisory lock for the WHOLE read-modify-write. Two
    // concurrent local pushes would otherwise both read the file, each apply
    // their own edit, and the later rename would discard the earlier push's
    // change. Serialising here makes each push read what the previous one wrote
    // and build on it, so both edits survive. Released when `_lock` drops.
    let lock = ManifestLock::acquire(path)?;
    // Read and replace the REAL target the lock guards, so a symlinked manifest
    // and a direct path never diverge between the read, the compare, and the
    // rename.
    let target = lock.target();

    let raw = match fs::read_to_string(target) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", target.display())),
    };
    // Redacted: `toml_edit`'s parse error quotes the offending source LINE, which
    // in a config-store `contents` table is a stored (possibly secret-bearing)
    // value. The diff read redacts the same failure; the writer must too.
    let mut doc: DocumentMut = raw.parse().map_err(|_err| {
        format!(
            "failed to parse {} as TOML (details redacted: the error can quote a stored value)",
            target.display()
        )
    })?;

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
    // previously-pushed `app_config` blob. The
    // default + staging keys must coexist so the runtime
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
    ensure_inline_toml_format(store_tbl, platform_name)?;
    let contents_entry = store_tbl
        .entry("contents")
        .or_insert_with(|| Item::Table(Table::new()));
    let contents_tbl = contents_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores.{platform_name}.contents` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    reject_future_local_roots(contents_tbl, gc_roots)?;
    reject_local_generated_key_collisions(contents_tbl, entries)?;
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
                    // Never remove an orphan that is itself protected -- a
                    // runtime-readable root, a value claiming our `edgezero_kind`
                    // namespace or written by a NEWER format, or a nested root with
                    // canonical chunks beneath it (deleting which would orphan that
                    // nested generation). Only a raw leaf PAYLOAD prunes. Shared
                    // with the dry-run count via `is_prunable_leaf`, so the preview
                    // can never disagree with what is removed here.
                    if !is_prunable_leaf(contents_tbl, &key) {
                        warnings.push(format!(
                            "warning: kept `{key}` -- it is a runtime-readable root, claims the \
                             `edgezero_kind` namespace, or is a nested root with chunks beneath it; \
                             not a prunable chunk payload"
                        ));
                        continue;
                    }
                    contents_tbl.remove(&key);
                }
            }
            Err(err) => warnings.push(format!("warning: {err}")),
        }
    }

    atomically_replace_file(target, &raw, &doc.to_string())?;
    Ok(warnings)
}

/// Replace an already-canonical `target`'s contents ATOMICALLY. Callers pass
/// [`ManifestLock::target`] and hold the lock across the surrounding
/// read-modify-write, so this is not racing another writer; the re-read + compare
/// is a defence-in-depth corruption check, not the concurrency guard.
///
/// In order:
///
/// 1. Re-read `target` and require it to still hold the bytes this rewrite
///    started from (`expected_before`). A mismatch means something OUTSIDE our
///    writers mutated it, so fail rather than overwrite.
/// 2. Create a FRESH temp file in the target's directory with `create_new`
///    (`O_EXCL`): this never follows a file or symlink someone pre-planted at the
///    temp path, and successive names avoid collisions. The rename stays within
///    one directory so it cannot cross a filesystem boundary.
/// 3. Copy the target's existing permissions onto the temp BEFORE writing, so the
///    config bytes are never briefly readable under wider permissions than the
///    manifest allows, then write, then `rename` over the target. `rename` is
///    atomic on POSIX, so a concurrent reader sees either the old file or the new.
///
/// A [`TempFileGuard`] removes the temp on any failure after it is created.
fn atomically_replace_file(
    target: &Path,
    expected_before: &str,
    contents: &str,
) -> Result<(), String> {
    let current = match fs::read_to_string(target) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to re-read {}: {err}", target.display())),
    };
    if current != expected_before {
        return Err(format!(
            "{} changed on disk while this write was preparing its rewrite; nothing was written. \
             Re-run to pick up the other change.",
            target.display()
        ));
    }

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fastly.toml");
    // Create a staging file that CANNOT be an attacker's pre-planted symlink:
    // `create_new` fails if the path already exists (regular file or symlink), so
    // we retry successive names until we own a fresh inode.
    let mut attempt = 0_u32;
    let (tmp_path, mut tmp_file) = loop {
        let candidate = dir.join(format!(
            ".{file_name}.edgezero-{}-{attempt}.tmp",
            process_id()
        ));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                attempt = attempt.saturating_add(1);
                if attempt > 1_024 {
                    return Err(format!(
                        "could not create a staging temp file next to {}",
                        target.display()
                    ));
                }
            }
            Err(err) => return Err(format!("failed to create staging temp file: {err}")),
        }
    };
    let mut guard = TempFileGuard {
        path: Some(tmp_path.clone()),
    };

    // Match the target's permissions BEFORE writing any bytes, so config content
    // never lands under wider permissions than the manifest already had. A brand
    // NEW manifest (NotFound) keeps the create default -- nothing to preserve --
    // but any OTHER metadata error means the target EXISTS yet we cannot read its
    // mode, so we must NOT silently widen: fail rather than guess.
    match fs::metadata(target) {
        Ok(meta) => tmp_file
            .set_permissions(meta.permissions())
            .map_err(|err| format!("failed to set permissions on the staging temp file: {err}"))?,
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to read the permissions of {} (refusing to widen access): {err}",
                target.display()
            ));
        }
    }
    tmp_file
        .write_all(contents.as_bytes())
        .map_err(|err| format!("failed to write the staging temp file: {err}"))?;
    // Flush to disk BEFORE the rename. A writeback error (ENOSPC/EIO) must surface
    // HERE, while the known-good manifest is still untouched -- NOT be swallowed
    // so the command "succeeds" after installing content that never reached disk.
    // The guard removes the temp on this error.
    tmp_file
        .sync_all()
        .map_err(|err| format!("failed to flush the staging temp file to disk: {err}"))?;
    drop(tmp_file);

    // Re-check the hard-link count IMMEDIATELY before the rename. The lock-acquire
    // check ran before this write blocked on the lock, and a hard link created
    // during that wait (or since) would survive the byte comparison above only for
    // the rename to break the alias. This is the last moment we can fail closed.
    reject_hard_linked_manifest(target)?;

    fs::rename(&tmp_path, target)
        .map_err(|err| format!("failed to replace {}: {err}", target.display()))?;
    guard.disarm();
    // Sync the containing directory so the rename entry itself survives a crash.
    // Best-effort: opening a directory as a file is not portable (Windows), and
    // the critical durability -- the file's contents -- is already flushed above.
    if let Ok(dir_handle) = fs::File::open(dir) {
        let _dir_sync = dir_handle.sync_all();
    }
    Ok(())
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
/// Is `key` a plain, prunable chunk PAYLOAD in `contents`? `false` for a value
/// that must be KEPT: a runtime-readable root, a value claiming our
/// `edgezero_kind` namespace or written by a newer format, or a NESTED root (a
/// key with a canonical chunk beneath it). Only a raw leaf payload prunes.
///
/// The single source of truth shared by the real prune (`write_fastly_local_
/// config_store`) and the dry-run count, so the previewed number can never drift
/// from what `--yes` actually removes. (The dry-run reads the PRE-upsert table and
/// the prune the POST-upsert one, but a generated key with a nested generation is
/// already refused by `reject_generated_key_collisions`, so that asymmetry cannot
/// change the verdict.)
fn is_prunable_leaf(contents: &toml_edit::Table, key: &str) -> bool {
    let value_protected = contents
        .get(key)
        .and_then(toml_edit::Item::as_str)
        .is_some_and(|text| {
            value_announces_our_kind(text)
                || value_is_future_format(text)
                || gc_classify_root(key, text).is_ok()
        });
    let has_nested = contents
        .iter()
        .any(|(other, _)| other != key && chunk_key_generation(key, other).is_some());
    !(value_protected || has_nested)
}

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
                                Ok(prior) => Ok(prior
                                    .iter()
                                    .filter(|key| !new_keys.contains(*key))
                                    // Count only what the real prune would remove:
                                    // it must still be PRESENT (an absent key is a
                                    // no-op remove, not a deletion) AND a prunable
                                    // leaf by the SAME predicate the prune uses.
                                    .filter(|key| {
                                        contents.get(key.as_str()).is_some()
                                            && is_prunable_leaf(contents, key)
                                    })
                                    .count()),
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

/// Run `fastly config-store-entry list --store-id=<id> --json` and return each
/// item's `item_key`, `item_value`, and `created_at`.
///
/// The item VALUE is KEPT (not discarded): `config gc` classifies each root by
/// its value (`gc_classify_root`) and reconstructs live generations from the
/// chunk values, so all three fields are required. The value is used internally
/// only and is NEVER echoed into a diagnostic — parse failures redact it via
/// `redact_describe_response`.
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
    let stdout = strict_stdout(output.stdout, "config-store-entry list --json")?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
        format!(
            "failed to parse `fastly config-store-entry list` JSON (parse error redacted; \
             response: {})",
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
        // Name the offending KEY, not just the index: `item_key` is readable even
        // when another field is empty, so the operator can see WHICH entry to fix.
        let key_hint = entry
            .get("item_key")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .map_or_else(|| format!("#{idx}"), |key| format!("`{key}`"));
        let field = |name: &str| -> Result<String, String> {
            let raw = entry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "`fastly config-store-entry list` entry {key_hint} is missing a string \
                         `{name}` field; refusing to reclaim (nothing deleted)"
                    )
                })?;
            // An EMPTY field is as dangerous as a missing one: an empty root value
            // would classify as "references nothing" and orphan its live chunks.
            // Reject it here rather than reason about it later -- but say what to
            // look at, since a legitimate empty-valued sibling is otherwise a
            // whole-store block with no obvious cause.
            if raw.is_empty() {
                return Err(format!(
                    "`fastly config-store-entry list` entry {key_hint} has an empty `{name}` field; \
                     refusing to reclaim (nothing deleted). If this is a legitimate empty-valued \
                     entry, remove it or give it a value before running `config gc`."
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

/// RFC 3339 (`2026-07-13T03:27:42Z`) -> unix seconds, rounded UP on any fraction.
///
/// `timestamp()` FLOORS the sub-second part, and the current time the age gate
/// compares against is also floored. A creation floored DOWN makes a key look
/// OLDER: a true age of 59.002s (created `...:42.998Z`) would compute as 60s and
/// pass a 60s `--older-than` almost a full second early. Rounding creation UP
/// keeps the computed age conservative -- a key never ages into deletion early.
fn parse_rfc3339_secs(raw: &str) -> Option<u64> {
    let stamp = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let secs = stamp.timestamp();
    let rounded_up = if stamp.timestamp_subsec_nanos() > 0 {
        secs.checked_add(1)?
    } else {
        secs
    };
    u64::try_from(rounded_up).ok()
}

/// Report what a sweep is KEEPING, not only what it would delete, so the run is
/// reviewable: each RETAINED root by key, plus the referenced-chunk total those
/// roots hold (already summarised). A root listed here is never a delete
/// candidate.
///
/// "Retained"/"referenced", not "live": the set also includes a root that is
/// PROTECTED but not runtime-readable (e.g. one that fails the writer split check
/// and is warned about separately). Its chunks are conservatively protected, not
/// runtime-live, so `live_count` here is a count of REFERENCED chunks.
fn append_kept_roots_report(out: &mut Vec<String>, kept_roots: &[String], live_count: usize) {
    if kept_roots.is_empty() {
        out.push("keeping 0 retained root(s)".to_owned());
        return;
    }
    out.push(format!(
        "keeping {} retained root(s) ({live_count} referenced chunk(s) held by them):",
        kept_roots.len()
    ));
    for key in kept_roots {
        out.push(format!("  keeping `{key}`"));
    }
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
    let resolved_id = resolve_remote_config_store_id(store_name)?
        .ok_or_else(|| no_matching_store_error(store_name))?;
    let items = list_config_store_entries(&resolved_id)?;
    let plan = plan_gc_reclamation(&items, unix_now_secs(), older_than_secs)?;
    let GcPlan {
        doomed,
        kept_roots,
        live_count,
        retained_recent,
        roots,
        unprovable,
        warnings,
    } = plan;

    let doomed_count: usize = doomed.iter().map(Vec::len).sum();
    let mut out = vec![format!(
        "fastly config-store `{store_name}` (id={resolved_id}): {} entries, {roots} root(s), {live_count} referenced chunk(s), {doomed_count} orphan(s) in {} generation(s) older than {older_than_secs}s, {retained_recent} orphan(s) too recent",
        items.len(),
        doomed.len(),
    )];
    out.extend(warnings);
    append_kept_roots_report(&mut out, &kept_roots, live_count);
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
    if dry_run {
        // A dry-run only PLANS: list every candidate and stop. Nothing is
        // attempted, so there is no confirmed/failed/skipped distinction yet.
        for (key, age) in doomed.iter().flatten() {
            out.push(format!("  would delete `{key}` (age {age}s)"));
        }
        // `--yes` ALWAYS requires an explicit non-zero `--older-than` (a
        // destructive run must not guess the window), so the apply instruction
        // names both -- "re-run with --yes" alone would be rejected.
        out.push(format!(
            "dry-run: {doomed_count} orphan chunk(s) planned for deletion; re-run with \
             `--yes --older-than <dur>` (a non-zero window is required) to apply"
        ));
        return Ok(out);
    }
    // Real run: `doomed_count` is the PLANNED count. Do NOT pre-print each key as
    // "deleting" -- execution stops at a generation's first failure, so some
    // planned keys are never attempted. `execute_gc_deletes` reports the real
    // per-key outcome (deleted / FAILED / skipped) as it happens.
    out.push(format!(
        "reclaiming {doomed_count} planned orphan chunk(s) across {} generation(s)",
        doomed.len()
    ));

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
///
/// The escaping is POSIX/bash (Linux/macOS). A leading note makes that explicit,
/// because Windows `cmd` and PowerShell quote differently — an operator on those
/// shells must adapt the quoting rather than paste verbatim.
fn recovery_commands(store_id: &str, keys: &[String]) -> String {
    let commands = keys
        .iter()
        .map(|key| {
            format!(
                "  fastly config-store-entry delete --store-id={} --key={} --auto-yes",
                shell_single_quote(store_id),
                shell_single_quote(key),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "  # POSIX/bash (Linux/macOS). On Windows cmd/PowerShell the quoting \
         differs -- adapt it for your shell.\n{commands}"
    )
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
                    // CONFIRMED gone, per key, as it happens.
                    out.push(format!("  deleted `{key}`"));
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
                    // Distinguish the ones we NEVER ATTEMPTED (after the stop)
                    // from the failed key itself, so the report is not read as
                    // "all of these were tried and failed".
                    for skipped in unconfirmed.iter().filter(|member| *member != key) {
                        out.push(format!(
                            "  skipped `{skipped}` (not attempted: this generation's delete stopped at the failure above)"
                        ));
                    }
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
    let mut warnings: Vec<String> = Vec::new();
    for item in items {
        let is_chunk_shaped = chunk_key_generation_any(&item.item_key).is_some();
        let classified = match gc_classify_root(&item.item_key, &item.item_value) {
            Ok(classified) => classified,
            // A chunk-shaped key whose value we cannot classify is a genuine
            // chunk fragment (a candidate) ONLY if BOTH hold:
            //   - the value ANNOUNCES no kind. A real chunk payload is a raw
            //     envelope fragment (no `edgezero_kind`); anything that DOES claim
            //     our namespace -- a parked pointer, an unknown/future kind -- is
            //     root-like or suspicious and must fail closed below.
            //   - NOTHING is nested beneath this key. A truncated/corrupt pointer
            //     at a chunk-shaped key is ALSO an unparseable fragment, but if it
            //     is a nested ROOT with its own generation, those nested chunks are
            //     proven independently and would be deleted while their (unreadable)
            //     root can no longer name them -- silent loss of a whole nested
            //     generation. If any canonical chunk of THIS key exists, treat the
            //     key as an unreadable nested root and FAIL CLOSED. A real leaf
            //     payload never has nested chunks, so normal GC is unaffected.
            Err(_)
                if is_chunk_shaped
                    && !value_announces_our_kind(&item.item_value)
                    && !value_is_future_format(&item.item_value)
                    && !items.iter().any(|other| {
                        other.item_key != item.item_key
                            && chunk_key_generation(&item.item_key, &other.item_key).is_some()
                    }) =>
            {
                continue; // a leaf chunk payload: a delete candidate
            }
            // A definitively FOREIGN entry at an ORDINARY key — a plain string
            // like `greeting = "hello"`, a scalar, or a complete JSON object
            // without our discriminator. The runtime returns it verbatim and it
            // references no chunks, so protect it as a zero-reference root.
            // Aborting here would let one ordinary sibling block reclamation of
            // every generation in the store.
            //
            // Three guards keep this from ever masking corruption:
            //   - the value must be provably inert (NOT a malformed object that
            //     could be a truncated/corrupt pointer, NOT a value claiming our
            //     namespace) -- otherwise we might orphan chunks a broken root
            //     still references;
            //   - it must NOT be a future format. A direct envelope from a newer
            //     writer classifies as `Foreign` (no `edgezero_kind`), so without
            //     this guard it would be waved through as a zero-reference root --
            //     yet a newer format may reference chunks under a scheme this build
            //     cannot read, and GC would plan them for deletion. Fail closed;
            //   - the KEY must be outside our reserved `.__edgezero_chunks.`
            //     namespace. A non-canonical key that still lives in that
            //     namespace is not an ordinary sibling; we cannot say what it is,
            //     so it fails closed below rather than being waved through.
            Err(_)
                if value_is_inert_foreign(&item.item_value)
                    && !value_is_future_format(&item.item_value)
                    && !item.item_key.contains(CHUNK_KEY_INFIX) =>
            {
                roots = roots.saturating_add(1);
                protected.insert(item.item_key.clone());
                continue;
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
        // The reassembled value may be a NEWER inner format (a bumped envelope
        // version, or an unknown `edgezero_kind`) that `BlobEnvelope` deserialize
        // silently ignores. Such a format can reference ADDITIONAL generations this
        // build cannot see, so trusting only the outer pointer's chunks as the live
        // set would let GC delete those as orphans. The runtime resolver rejects
        // this case; GC must too. Fail closed.
        if value_is_future_format(&assembled) {
            return Err(format!(
                "refusing to reclaim: root `{}` reconstructs to a value in a newer format this \
                 build does not recognise. It may reference generations this build cannot see, so \
                 treating its outer chunks as the whole live set could delete live data. Nothing \
                 was deleted.",
                item.item_key
            ));
        }
        gc_verify_generation(&pointer.envelope_sha256, &assembled).map_err(|err| {
            format!(
                "refusing to reclaim: root `{}` names a chunk set that does not reconstruct the \
                 envelope it claims ({err}). Its chunk list is therefore not a trustworthy live \
                 set, and treating it as one could delete a live chunk. Nothing was deleted.",
                item.item_key
            )
        })?;
        // Same exact-split predicate the RUNTIME resolver applies. The content
        // checks above only prove the bytes; a pointer whose boundaries are not
        // the ones this writer emits reassembles correctly here but is REJECTED
        // at runtime -- so GC would otherwise call it a healthy live root while
        // the guest 500s on it, and its generation can never satisfy
        // `prove_generation` either, making it permanently unreclaimable.
        //
        // We still protect it (fail-closed: never delete on a judgement we are
        // unsure of), but we no longer call it healthy silently -- the operator
        // gets told it is unreadable and will not be reclaimed automatically.
        if let Err(err) =
            verify_writer_split_layout(&item.item_key, &assembled, &chunk_lengths(&pointer.chunks))
        {
            warnings.push(format!(
                "warning: root `{}` is NOT runtime-readable ({err}). Its chunks are kept, but this \
                 generation can never be proven writer-produced, so `config gc` will never reclaim \
                 it. Re-run `config push` for this key to rewrite it, then re-run `config gc`.",
                item.item_key
            ));
        }
        live.extend(pointer.chunks.into_iter().map(|chunk| chunk.key));
    }
    Ok(GcClassification {
        live,
        protected,
        roots,
        warnings,
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
        warnings,
    } = classify_store_entries(items, &value_by_key)?;

    // ---- 2. Per-root live-config age (best-effort; see the guard below) ----
    // rsplit_once (the LAST infix): a chunk of a chunk-shaped root nests the infix
    // twice, and its root is everything before the LAST one. Splitting on the
    // first would attribute a nested chunk's age to the wrong (outer) root.
    let root_live_since: HashMap<&str, u64> = live.iter().fold(HashMap::new(), |mut acc, key| {
        if let Some((root, _)) = key.rsplit_once(CHUNK_KEY_INFIX) {
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
        // rsplit_once (the LAST infix): the same nested-chunk correctness the
        // live-set scan and classification use — a chunk of a chunk-shaped root
        // is grouped under THAT root, not the outer one, so nested orphans are
        // grouped (and thus reclaimed or reported), not silently dropped.
        let Some((root, _)) = item.item_key.rsplit_once(CHUNK_KEY_INFIX) else {
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
    for ((root, generation), mut group) in groups {
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
        // Delete in canonical chunk-INDEX order (`.0`, `.1`, ...), NOT the remote
        // listing order. Deletion stops at a generation's first failure, so a
        // reordered listing would otherwise change the preview order and which
        // siblings get stranded; sorting makes both deterministic. Every member is
        // a canonical chunk of `root` (it passed the grouping filter), so
        // `chunk_key_index` is `Some`; `None` sorts last defensively.
        group.sort_by_key(|item| chunk_key_index(root, &item.item_key).unwrap_or(usize::MAX));
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

    let mut kept_roots: Vec<String> = protected.into_iter().collect();
    kept_roots.sort();

    Ok(GcPlan {
        doomed,
        kept_roots,
        live_count: live.len(),
        retained_recent,
        roots,
        unprovable,
        warnings,
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
    // The chunk KEY is pointer-controlled (a malformed pointer can carry any
    // string there), so diagnostics name a POSITION, not the key. `root_key` is
    // the operator's own logical entry key and is named for context, as the rest
    // of the GC diagnostics do.
    for (position, chunk) in pointer.chunks.iter().enumerate() {
        let Some(value) = value_by_key.get(chunk.key.as_str()) else {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` references chunk {position}, which is \
                 absent from the store listing (the listing may be incomplete/paginated, or the \
                 store is already inconsistent); nothing was deleted"
            ));
        };
        if value.len() != chunk.len {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` says chunk {position} is {} bytes but the \
                 store holds {}; nothing was deleted",
                chunk.len,
                value.len()
            ));
        }
        if sha256_hex(value.as_bytes()) != chunk.sha256 {
            return Err(format!(
                "refusing to reclaim: the stored value of chunk {position} does not match the \
                 SHA-256 that root `{root_key}` records for it; nothing was deleted"
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
    // Split on the LAST infix, not the first: a chunk of a root that ITSELF
    // contains the infix (a pointer parked at a chunk-shaped key with self-scoped
    // chunks) has the infix twice, and its chunk suffix is after the LAST one.
    // Splitting on the first would misread the doubly-nested chunk as a
    // non-chunk, get it classified as an unclassifiable root, and abort the whole
    // store's GC. For an ordinary single-infix key the root has no infix, so the
    // last infix IS the first — this only changes the nested case.
    let (root, _rest) = key.rsplit_once(CHUNK_KEY_INFIX)?;
    chunk_key_generation(root, key)
}

/// Drive a sequential per-entry commit loop and produce the
/// partial-failure diagnostic when the committer fails mid-way.
/// Pure (no I/O) so the diagnostic shape is unit-testable without
/// the fastly CLI on PATH; production calls it with a closure that
/// shells out via `create_config_store_entry`. On success returns
/// the count of committed entries; on failure returns an error
/// string. The FAILED entry's outcome is UNKNOWN — Fastly may have
/// committed it before returning the error — so the message does not
/// claim a clean boundary; it directs the operator to re-run the whole
/// idempotent push rather than hand-resume from a supposed cut point.
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
                "fastly push failed at entry `{key}` while committing {committed} of {total} entries.\n  \
                 The failed entry's outcome is UNKNOWN: Fastly may have committed it before the error \
                 (a timeout can arrive after the write lands), including when it is the root pointer.\n  \
                 Recovery: re-run the SAME `config push`. It is idempotent -- chunk keys are content-addressed \
                 and writes use `--upsert` -- so entries already written are rewritten harmlessly and any \
                 missing ones are filled. Do NOT hand-delete the failed key.\n  \
                 Already written (a retry rewrites them): {pushed:?}\n  \
                 Failed: `{key}` (outcome unknown) -- {err}\n  \
                 Not attempted: {remaining:?}",
                committed = pushed.len(),
                total = entries.len(),
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
    // Take stdin OUT of the child and hand it to a helper that writes the value
    // and drops the handle on return — closing the pipe so the CLI sees EOF.
    // Dropping on scope-exit rather than via an explicit `drop()` keeps this
    // valid on targets where `ChildStdin` is a non-Drop stub.
    // `child.wait_with_output()` then consumes child cleanly.
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `fastly`".to_owned())?;
    write_value_to_fastly_stdin(stdin, value)?;
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

/// Write `value` to the child's stdin, then drop the handle as it falls out of
/// scope on return — closing the pipe so the `fastly` CLI sees EOF. Taking
/// `stdin` by value gives a natural scope-end drop rather than an explicit
/// `drop()`, which also keeps this valid on targets where `ChildStdin` is a
/// non-Drop stub.
fn write_value_to_fastly_stdin(mut stdin: ChildStdin, value: &str) -> Result<(), String> {
    stdin
        .write_all(value.as_bytes())
        .map_err(|err| format!("failed to write value to `fastly` stdin: {err}"))
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
    // Redact stderr: a Fastly error can quote the entry value back, which on the
    // delete path would put a stored config value into CI logs.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry delete --store-id={store_id} --key={key} --auto-yes` exited with status {}\n{}",
        output.status,
        redact_stderr(&stderr)
    ))
}

/// Read every `(key, value)` in config store `store_id` via
/// `fastly config-store-entry list --store-id=<id> --json`.
///
/// Accepts a bare array or an `{"items": [...]}` envelope, and reads each
/// entry's key/value from `item_key`/`item_value` (the field names
/// `config-store-entry describe` uses), falling back to `key`/`value`. A parse
/// failure is an error, NOT an empty list: a staged deploy mirrors this store,
/// and treating an unreadable listing as "no entries" would silently drop
/// production's overrides from the staged version.
fn read_config_store_entries(store_id: &str, cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let stdout = run_fastly_capture(
        &[
            "config-store-entry".to_owned(),
            "list".to_owned(),
            format!("--store-id={store_id}"),
            "--json".to_owned(),
        ],
        cwd,
    )?;
    parse_config_store_entries(&stdout)
}

/// Parse the `config-store-entry list --json` payload into `(key, value)` pairs.
///
/// Split out from the CLI call so it is unit-testable — and, critically, so every
/// error path REDACTS the payload. The listing carries every entry's `item_value`,
/// which may be production config or secrets, and CLI status lines are logged
/// verbatim into commonly-retained CI logs. So a schema-drift / parse error must
/// summarise the response (size + top-level shape via `redact_describe_response`),
/// never echo the raw stdout.
fn parse_config_store_entries(stdout: &str) -> Result<Vec<(String, String)>, String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).map_err(|err| {
        format!(
            "failed to parse `fastly config-store-entry list --json` JSON: {err} ({})",
            redact_describe_response(stdout)
        )
    })?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
        .ok_or_else(|| {
            format!(
                "`fastly config-store-entry list --json` output is neither a bare array nor an `items` envelope ({}); fastly CLI may have changed its schema",
                redact_describe_response(stdout)
            )
        })?;
    let mut entries = Vec::with_capacity(array.len());
    for entry in array {
        let key = entry
            .get("item_key")
            .or_else(|| entry.get("key"))
            .and_then(serde_json::Value::as_str);
        let value = entry
            .get("item_value")
            .or_else(|| entry.get("value"))
            .and_then(serde_json::Value::as_str);
        match (key, value) {
            (Some(found_key), Some(found_value)) => {
                entries.push((found_key.to_owned(), found_value.to_owned()));
            }
            _ => {
                return Err(format!(
                    "a `fastly config-store-entry list --json` entry has no string `item_key`/`item_value` fields ({}); fastly CLI may have changed its schema",
                    redact_describe_response(stdout)
                ));
            }
        }
    }
    Ok(entries)
}

/// `fastly config-store-entry delete --store-id=<id> --key=<k>`, run in the
/// app manifest directory. Distinct from the `config gc` `delete_config_store_entry`
/// (which runs in the process cwd with redacted diagnostics); staging reconciliation
/// must run `fastly` in `cwd` so it resolves the right service context.
fn delete_staging_config_store_entry(store_id: &str, key: &str, cwd: &Path) -> Result<(), String> {
    run_fastly_status(
        &[
            "config-store-entry".to_owned(),
            "delete".to_owned(),
            format!("--store-id={store_id}"),
            format!("--key={key}"),
        ],
        cwd,
    )
}

/// Compute the staging selector store's entries from production's, given the
/// declared config-store logical ids.
///
/// The twin is a faithful MIRROR of production's runtime overrides — adapter
/// host, logging level, `__NAME` redirects — with exactly one transform: every
/// declared config store's selector key (`EDGEZERO__STORES__CONFIG__<ID>__KEY`)
/// points at `<logical>_staging`, the key `config push --staging` writes. A
/// declared store gets that selector even when production has no explicit entry
/// for it (production relies on the runtime's default = the logical id; staging
/// must NOT inherit that default, or it would read production's key).
///
/// Pure so the transform is unit-testable without the fastly CLI.
fn staging_entries_from_production(
    production: &[(String, String)],
    config_logical_ids: &[String],
) -> Vec<(String, String)> {
    // selector key -> staging value, one per declared config store.
    let selectors: Vec<(String, String)> = config_logical_ids
        .iter()
        .map(|id| (runtime_env_key_for(id), format!("{id}_staging")))
        .collect();
    let is_selector = |key: &str| selectors.iter().any(|(sel, _)| sel == key);

    // Copy every non-selector production override verbatim; selectors are
    // supplied from `selectors` below (whether or not production carried one).
    let mut out: Vec<(String, String)> = production
        .iter()
        .filter(|(key, _)| !is_selector(key))
        .cloned()
        .collect();
    out.extend(selectors);
    out
}

fn is_runtime_store_name_key(key: &str) -> bool {
    let mut segments = key.split("__");
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (
            Some("EDGEZERO"),
            Some("STORES"),
            Some("CONFIG" | "KV" | "SECRETS"),
            Some(id),
            Some("NAME"),
            None,
        ) if !id.is_empty()
    )
}

fn runtime_store_name_entries_from_vars(
    vars: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    for (key, value) in vars {
        if !is_runtime_store_name_key(&key) {
            continue;
        }
        if value.is_empty() || value.trim() != value {
            return Err(format!(
                "runtime store-name override `{key}` must be non-empty and contain no surrounding whitespace"
            ));
        }
        entries.push((key, value));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn overlay_runtime_store_name_entries(
    base: &[(String, String)],
    overrides: &[(String, String)],
) -> Vec<(String, String)> {
    let mut entries = base.to_vec();
    for (key, value) in overrides {
        if let Some((_, current)) = entries.iter_mut().find(|(candidate, _)| candidate == key) {
            current.clone_from(value);
        } else {
            entries.push((key.clone(), value.clone()));
        }
    }
    entries
}

/// Resolve the staging twin store, creating it on demand. A staged deploy owns
/// this store end to end (it is never linked on the ACTIVE version), so it does
/// not depend on `provision` having created it first. Fails closed on a lookup
/// FAILURE rather than blindly creating a duplicate.
/// The per-service staging twin store name — the base prefix plus the service
/// id, so concurrent staged deploys of different services on one account never
/// clobber each other's selectors.
fn staging_selector_store_name(service_id: &str) -> String {
    format!("{RUNTIME_ENV_STAGING_STORE_PREFIX}_{service_id}")
}

fn ensure_staging_selector_store(store_name: &str) -> Result<String, String> {
    match classify_remote_config_store(store_name)? {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => {
            create_fastly_store("config", store_name)?;
            // resolve_remote_config_store_id now yields a typed absence; we just
            // created the store, so a None here is fail-closed (the listing did
            // not reflect our own create), not a genuine absence.
            resolve_remote_config_store_id(store_name)
                .map_err(|err| {
                    format!(
                        "created fastly config-store `{store_name}` but could not resolve its id: {err}"
                    )
                })?
                .ok_or_else(|| {
                    format!(
                        "created fastly config-store `{store_name}` but it did not appear in `config-store list`"
                    )
                })
        }
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` while resolving `{store_name}`: {detail}.\n  Refusing to stage. Pin a known-compatible fastly CLI version and retry."
        )),
    }
}

/// Reconcile the staging twin so it MIRRORS production's runtime overrides
/// (`production`) with only the config selectors redirected to
/// `<logical>_staging`.
///
/// Upserts the full desired set FIRST, then deletes twin entries production no
/// longer has (so a removed override does not linger and diverge staging from
/// production). Runs while the staged draft is still editable, before the relink.
/// When production has NO override store, `production` is empty and the twin holds
/// only the derived staging selectors — staging is still isolated.
///
/// Order matters: this per-service twin can still be LINKED by a previously-staged
/// version of the same service, which reads it live. Upserting every desired entry
/// before deleting any stale one means that reader never observes a required
/// selector transiently absent (which would fall it back to PRODUCTION config), and
/// a mid-reconciliation failure leaves the twin a superset — never a store missing a
/// selector. `--upsert` (see `create_config_store_entry`) makes the writes
/// idempotent, so re-running is safe.
///
/// Residual limitation: two *concurrent* staged deploys of the SAME service still
/// race on this one twin. Serialize them with a per-service concurrency group in
/// the calling workflow (see the deploy guide's reconcile section); a shared store
/// cannot make that race safe on its own.
fn mirror_production_to_staging(
    production: &[(String, String)],
    staging_id: &str,
    config_logical_ids: &[String],
    cwd: &Path,
) -> Result<(), String> {
    let process_overrides = runtime_store_name_entries_from_vars(env::vars())?;
    let effective_production = overlay_runtime_store_name_entries(production, &process_overrides);
    let desired = staging_entries_from_production(&effective_production, config_logical_ids);

    for (key, value) in &desired {
        create_config_store_entry(staging_id, key, value)?;
    }
    let current = read_config_store_entries(staging_id, cwd)?;
    for (key, _) in &current {
        if !desired.iter().any(|(dk, _)| dk == key) {
            delete_staging_config_store_entry(staging_id, key, cwd)?;
        }
    }
    Ok(())
}

/// Return the runtime entries required when logical store ids map to different
/// Fastly resource names.
fn runtime_env_store_name_entries(stores: &ProvisionStores<'_>) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for (kind, ids) in [
        ("CONFIG", stores.config),
        ("KV", stores.kv),
        ("SECRETS", stores.secrets),
    ] {
        for store in ids {
            if store.logical == store.platform {
                continue;
            }
            entries.push((
                format!(
                    "EDGEZERO__STORES__{kind}__{}__NAME",
                    store.logical.to_ascii_uppercase()
                ),
                store.platform.clone(),
            ));
        }
    }
    entries
}

fn persist_runtime_env_store_name_entries(
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let entries = runtime_env_store_name_entries(stores);
    if dry_run {
        return Ok(entries
            .iter()
            .map(|(key, value)| {
                format!(
                    "would upsert `{key}={value}` into fastly config-store `{RUNTIME_ENV_STORE}`"
                )
            })
            .collect());
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let runtime_env_store_id = resolve_remote_config_store_id(RUNTIME_ENV_STORE)?
        .ok_or_else(|| no_matching_store_error(RUNTIME_ENV_STORE))?;
    push_entries_with_committer(&entries, |key, value| {
        create_config_store_entry(&runtime_env_store_id, key, value)
    })?;
    Ok(vec![format!(
        "persisted {} non-default store-name mapping(s) in fastly config-store `{RUNTIME_ENV_STORE}`",
        entries.len()
    )])
}

/// The runtime-override entry naming the config-store KEY for logical store
/// `id` — `EDGEZERO__STORES__CONFIG__<ID>__KEY`.
///
/// Must match what the runtime reads: `EnvConfig::from_vars` strips the
/// `EDGEZERO__` prefix, splits on `__`, and lowercases each segment, and
/// `store_key("config", id)` looks up `["stores", "config", id, "key"]`. So the
/// entry name is the id uppercased. A near-miss here is silent — the runtime
/// would just fall back to the id and read production config.
fn runtime_env_key_for(logical_id: &str) -> String {
    format!(
        "EDGEZERO__STORES__CONFIG__{}__KEY",
        logical_id.to_ascii_uppercase()
    )
}

/// Find the id of the resource link published under `link_name` in
/// `fastly resource-link list --json` output.
///
/// The link's own `name` is an alias that defaults to the linked resource's
/// name, so match on it rather than the resource name — the whole point of the
/// staging relink is that a store named `edgezero_runtime_env_staging` is linked
/// under the name `edgezero_runtime_env`.
///
/// Returns `None` when the version has no such link (nothing to delete).
fn find_resource_link_id(stdout: &str, link_name: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))?;
    array.iter().find_map(|entry| {
        let name = entry.get("name").and_then(serde_json::Value::as_str)?;
        if name != link_name {
            return None;
        }
        entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
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
    // FAIL CLOSED on any malformed or duplicate row: a `NotFound` here becomes a
    // MissingStore that AUTHORISES an overwrite, so a listing we cannot read
    // exactly must never look like a definite absence. A malformed row could BE
    // the requested store (its unreadable `name` might have matched), and a
    // duplicate name means we are not reading the store we think we are. Every row
    // must carry a non-empty string `name` and `id`, and names must be unique.
    let mut seen_names = HashSet::with_capacity(array.len());
    let mut found: Option<String> = None;
    for (idx, entry) in array.iter().enumerate() {
        let name_field = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty());
        let id_field = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty());
        let (Some(entry_name), Some(entry_id)) = (name_field, id_field) else {
            return ConfigStoreLookup::SchemaDrift(format!(
                "store-list entry #{idx} is missing a non-empty string `name` or `id`; refusing to \
                 treat a store as absent on a listing this build cannot read exactly"
            ));
        };
        if !seen_names.insert(entry_name.to_owned()) {
            return ConfigStoreLookup::SchemaDrift(format!(
                "store-list has a duplicate `name` (`{entry_name}`); refusing to resolve a store id \
                 on an ambiguous listing"
            ));
        }
        if entry_name == name {
            found = Some(entry_id.to_owned());
        }
    }
    found.map_or(ConfigStoreLookup::NotFound, ConfigStoreLookup::Found)
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
                // Object KEYS are stored/provider-controlled data (a wrong-shape
                // response could be `{"<secret>": ...}`), so only the COUNT is
                // reported, never the key names.
                format!("{len} bytes, JSON object with {} field(s)", map.len())
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
///
/// Returns a TYPED absence: `Ok(None)` ONLY when the list call SUCCEEDS and no
/// store matches (a genuine absence). An operational failure (missing binary,
/// spawn/list failure, schema drift) stays `Err` -- callers that read for a diff
/// must not treat an operational failure as "store absent" and overwrite.
fn resolve_remote_config_store_id(name: &str) -> Result<Option<String>, String> {
    match classify_remote_config_store(name)? {
        ConfigStoreLookup::Found(id) => Ok(Some(id)),
        ConfigStoreLookup::NotFound => Ok(None),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// Look a config store up by name and return the raw [`ConfigStoreLookup`], so
/// callers can tell "the account has no such store" (`NotFound`) apart from "the
/// lookup itself failed" (`Err` — CLI missing / non-zero exit — or
/// `SchemaDrift`). A staged deploy relies on that distinction to decide whether
/// to skip config isolation (genuinely no store) or fail closed (couldn't tell).
///
/// `Err` is only for a failure to OBTAIN an answer; a successful listing that
/// simply doesn't contain `name` is `Ok(ConfigStoreLookup::NotFound)`.
fn classify_remote_config_store(name: &str) -> Result<ConfigStoreLookup, String> {
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
    // Adopt main's strict UTF-8 gate (fail closed on undecodable stdout) but
    // return the raw lookup so staging callers keep the 3-way verdict; the
    // SchemaDrift -> Err mapping lives in resolve_remote_config_store_id.
    let stdout = strict_stdout(output.stdout, "config-store list --json")?;
    Ok(find_config_store_id(&stdout, name))
}

/// Message for a genuinely-absent store, for the write/GC callers that treat
/// absence as a hard error (they cannot operate on a store that does not exist).
fn no_matching_store_error(name: &str) -> String {
    format!(
        "no fastly config-store matches `{name}` (did you run `edgezero provision --adapter fastly`?)"
    )
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

/// Whether `args` already carries the Fastly CLI's non-interactive
/// switch, in either its long (`--non-interactive`) or short (`-i`)
/// form. Used to avoid passing the flag twice when a caller already
/// supplied it via `deploy-args` passthrough.
fn has_non_interactive(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--non-interactive" || arg == "-i")
}

/// Build the argv for `fastly compute deploy`, appending
/// `--non-interactive` (a Fastly CLI *global* flag, supported by
/// `compute deploy`) unless the caller already passed it. Without it a
/// production deploy can block on an interactive prompt in CI.
fn build_compute_deploy_args(extra_args: &[String]) -> Vec<String> {
    let mut argv = vec!["compute".to_owned(), "deploy".to_owned()];
    argv.extend_from_slice(extra_args);
    if !has_non_interactive(extra_args) {
        argv.push("--non-interactive".to_owned());
    }
    argv
}

/// # Errors
/// Returns an error if the Fastly CLI deploy command fails.
///
/// Honours a CLI-threaded `--manifest-path <abs fastly.toml>` (see
/// [`resolve_manifest_dir`]) so a monorepo with several Fastly apps
/// deploys the one the operator's `edgezero.toml` selected, rather than
/// whichever `fastly.toml` a bare working-directory search finds first.
/// The flag is EdgeZero-internal — `fastly compute deploy` has no such
/// flag — so it is stripped from the forwarded argv.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest_dir = resolve_manifest_dir(extra_args)?;
    let forwarded = args_without_flag_value(extra_args, "--manifest-path");

    let status = Command::new("fastly")
        .args(build_compute_deploy_args(&forwarded))
        .current_dir(&manifest_dir)
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

// ===================================================================
// Fastly staging lifecycle
// ===================================================================
//
// These entry points back the `deploy --staging`, `healthcheck`, and
// `rollback` app-CLI subcommands. They mirror the Fastly semantics of
// `stackpop/trusted-server-actions`:
//
//   * staged deploy  → build + `compute update --autoclone` (no
//     activation) + `service-version stage`; emits the staged version.
//   * production      → `fastly compute deploy` runs via the manifest
//     command; `emit_active_version` resolves the activated version.
//   * healthcheck     → curl the domain (production) or the version's
//     resolved staging IP (`--staging`); non-zero exit when unhealthy.
//   * rollback        → activate the explicit `--rollback-to` version
//     (production) or deactivate `<v>` (staging) via the Fastly API.
//
// **Version-output contract:** deploy/stage print a
// single `version=<N>` line to stdout (via `log::info!`, which the CLI
// logger emits verbatim). The `deploy-fastly` action greps that line
// to surface `fastly-version`. Rollback prints `rolled-back-to=<N>`.
//
// Provider HTTP calls shell out to `curl` (matching
// trusted-server-actions and avoiding a WASM-incompatible HTTP client
// in the adapter). The `FASTLY_API_TOKEN` is passed to `curl` via a
// `--config -` stdin file rather than on argv, so it never appears in
// `ps` / `/proc/<pid>/cmdline` (same discipline as
// `create_config_store_entry`'s `--stdin`).

/// Value that follows `flag` in a `--flag value` arg slice, if present.
fn arg_value<'args>(args: &'args [String], flag: &str) -> Option<&'args str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| idx.checked_add(1))
        .and_then(|idx| args.get(idx))
        .map(String::as_str)
}

/// Whether a boolean `flag` (e.g. `--staging`) is present in `args`.
fn arg_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

/// Copy of `args` with `--flag value` removed (both tokens). Used to
/// forward operator passthrough (e.g. `--comment`) to `fastly compute
/// update` without re-passing `--service-id`, which is threaded
/// explicitly.
fn args_without_flag_value(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == flag {
            skip = true;
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Split an arg on a leading `--flag=value`, returning `(flag, value)`.
fn split_inline_value(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with('-') => (flag, Some(value)),
        Some(_) | None => (arg, None),
    }
}

/// Partition operator passthrough args for a staged deploy: forward only
/// what `fastly compute update` supports, lift `--comment` out (it is a
/// `compute deploy` / `service-version update` flag, NOT a
/// `compute update` one), and drop the rest.
///
/// Both `--comment value` and `--comment=value` are recognised.
fn split_staged_passthrough(args: &[String]) -> StagedPassthrough {
    let mut split = StagedPassthrough {
        forwarded: Vec::with_capacity(args.len()),
        comment: None,
        dropped: Vec::new(),
    };
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = split_inline_value(arg);
        if flag == "--comment" {
            split.comment = match inline {
                Some(value) => Some(value.to_owned()),
                None => iter.next().cloned(),
            };
        } else if COMPUTE_UPDATE_VALUE_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
            if inline.is_none()
                && let Some(value) = iter.next()
            {
                split.forwarded.push(value.clone());
            }
        } else if COMPUTE_UPDATE_BOOL_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
        } else {
            // Unsupported by `compute update`. Consume a detached value
            // too, so a stray `stage` from `--env stage` is not left
            // behind as a bogus positional.
            split.dropped.push(flag.to_owned());
            if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                iter.next();
            }
        }
    }
    split
}

/// Resolve the target service id from `--service-id` or, failing that,
/// `FASTLY_SERVICE_ID`.
fn resolve_service_id(args: &[String]) -> Result<String, String> {
    if let Some(value) = arg_value(args, "--service-id") {
        return Ok(value.to_owned());
    }
    env::var(FASTLY_SERVICE_ID_ENV).map_err(|_err| {
        format!("no service id: pass `--service-id <id>` or set {FASTLY_SERVICE_ID_ENV}")
    })
}

/// Read the required Fastly API token from the environment.
fn require_token() -> Result<String, String> {
    env::var(FASTLY_API_TOKEN_ENV)
        .map_err(|_err| format!("{FASTLY_API_TOKEN_ENV} must be set in the environment"))
}

/// Whether an HTTP status counts as healthy (2xx only).
///
/// A passing probe gates against an automatic rollback, so a 3xx is deliberately
/// NOT healthy: a staged version answering `301` to an error page (the probe does
/// not follow redirects) would otherwise mask a bad deploy as healthy.
fn is_healthy_status(code: u16) -> bool {
    (200..300).contains(&code)
}

/// Digits immediately following `marker` in `lower` (a lowercased
/// haystack), for the LAST occurrence of `marker`. The number must be
/// terminated by `terminator` — so a partial/confusable match (e.g. a
/// semver `15.2.0`) yields `None` rather than a bogus version.
fn last_version_after(lower: &str, marker: &str, terminator: char) -> Option<u64> {
    let mut result = None;
    for (idx, _) in lower.match_indices(marker) {
        let after = idx.saturating_add(marker.len());
        let Some(rest) = lower.get(after..) else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || rest.chars().nth(digits.len()) != Some(terminator) {
            continue;
        }
        if let Ok(parsed) = digits.parse::<u64>() {
            result = Some(parsed);
        }
    }
    result
}

/// Parse a Fastly service version out of Fastly CLI output, accepting
/// ONLY the shapes the CLI actually emits, in precedence order:
///
///   1. Our canonical `version=<N>` contract line.
///   2. The CLI's success line, whose Go format string is
///      `"Updated package (service %s, version %v)"` (and
///      `"Deployed package (...)"` for `compute deploy`) — matched as
///      `, version <N>)`. This names the version the package landed on,
///      so it wins over (3).
///   3. The `--autoclone` notice, `"... Now operating on version %d."` —
///      the freshly-cloned draft, used when the success line is absent.
///
/// Everything else yields `None` and the caller FAILS CLOSED.
///
/// Deliberately strict. The previous implementation took ANY digits
/// appearing after the word "version" and let the last match win, so:
///   * `Uploaded package to service 12345, version unchanged` parsed as
///     version 12345, and
///   * the autoclone notice's *pre-clone* version
///     (`Service version 3 is not editable...`) could beat the real one,
///     since stdout and stderr are concatenated and their relative order
///     is not guaranteed.
///
/// A misparse here stages, comments, or rolls back the WRONG service
/// version, so ambiguity must be an error, not a guess.
fn parse_fastly_version(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    parse_canonical_version_line(&lower)
        .or_else(|| last_version_after(&lower, ", version ", ')'))
        .or_else(|| last_version_after(&lower, "now operating on version ", '.'))
}

/// Last standalone `version=<N>` line (the whole trimmed line must be
/// exactly that, so a `--version=active` flag echoed in a command line
/// cannot masquerade as one).
fn parse_canonical_version_line(lower: &str) -> Option<u64> {
    lower.lines().rev().find_map(|line| {
        let digits = line.trim().strip_prefix("version=")?;
        (!digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// Parse `fastly service-version list --json` (or the Fastly API
/// `/service/<id>/version` array) for the `number` of the `active`
/// version.
/// Resolve the active version from a Fastly version-list JSON.
///
/// `Ok(Some(n))` — exactly one version is active. `Ok(None)` — the list parsed
/// but NO version is active (a first-ever deploy; the caller records an empty
/// rollback target and proceeds). `Err(_)` — the payload could not be parsed as
/// a version list, OR it is MALFORMED (a non-boolean `active` on ANY entry, an
/// `active: true` entry whose `number` is missing or not an unsigned integer, or
/// MORE THAN ONE active version). All are OPERATIONAL failures the caller must
/// NOT silently treat as "no active version" — otherwise a garbled or ambiguous
/// response would fail open and let a production deploy proceed with no rollback
/// target.
///
/// The ENTIRE list is scanned (not short-circuited at the first active entry) so
/// that a malformed `active` field or a second active version anywhere in the
/// response is caught rather than ignored.
fn resolve_active_version(json: &str) -> Result<Option<u64>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("failed to parse the Fastly version list as JSON: {err}"))?;
    let array = value.as_array().ok_or_else(|| {
        "the Fastly version list was not a JSON array; the API may have changed its schema"
            .to_owned()
    })?;
    // A real Fastly service always has at least an initial (inactive) version, so
    // an EMPTY list is an invalid response — fail closed rather than read it as a
    // legitimate "no active version yet" (first deploy).
    if array.is_empty() {
        return Err(
            "the Fastly version list is empty; a service always has at least an initial version, so this response cannot be trusted".to_owned()
        );
    }
    let mut active_version: Option<u64> = None;
    for entry in array {
        // EVERY entry must be a well-formed version object with an unsigned
        // integer `number` — Fastly includes it on every version. A `null`, a
        // non-object, or a missing/non-integer `number` means the response
        // cannot be trusted; treating such an entry as merely "not active" would
        // let a garbled payload read as "no active version" (fail open).
        let Some(object) = entry.as_object() else {
            return Err(format!(
                "a Fastly version list element is not an object; the API may have changed its schema. Element: {entry}"
            ));
        };
        let number = object.get("number").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            format!(
                "a Fastly version entry has no unsigned-integer `number`; the API may have changed its schema. Entry: {entry}"
            )
        })?;
        // `active` is optional (an omitted field means not active), but a PRESENT
        // non-boolean is schema drift.
        let active = match object.get("active") {
            None => false,
            Some(active_field) => active_field.as_bool().ok_or_else(|| {
                format!(
                    "a Fastly version entry has a non-boolean `active` field; the API may have changed its schema. Entry: {entry}"
                )
            })?,
        };
        if active {
            if active_version.is_some() {
                return Err(format!(
                    "the Fastly version list reports more than one active version ({} and {number}); the response is ambiguous, refusing to pick one",
                    active_version.unwrap_or_default()
                ));
            }
            active_version = Some(number);
        }
    }
    Ok(active_version)
}

/// Best-effort staleness guard for a production rollback: the version being
/// rolled back FROM (`from_version`, the caller's `--version`) must still be the
/// ACTIVE version. A rollback can run long after its deploy; if a newer version
/// was activated since, activating the old target would clobber it — so refuse.
///
/// This narrows but does NOT close the race: the caller reads the active version
/// and activates in two separate requests, and Fastly's activate endpoint has no
/// precondition, so a deploy landing between them can still be clobbered.
/// Service-scoped serialization is required to eliminate it.
fn ensure_rollback_from_is_active(
    active: Option<u64>,
    from_version: u64,
    service_id: &str,
) -> Result<(), String> {
    match active {
        Some(active_version) if active_version == from_version => Ok(()),
        Some(active_version) => Err(format!(
            "refusing to roll back service {service_id}: the active version is now {active_version}, not the {from_version} being rolled back from -- a newer deploy is live and rolling back would clobber it"
        )),
        None => Err(format!(
            "refusing to roll back service {service_id}: it has no active version"
        )),
    }
}

/// First staging IP found in a Fastly
/// `GET /service/<id>/version/<n>/domain?include=staging_ips` response.
///
/// The response is an ARRAY of domain objects, and the staging address
/// is a SINGULAR, nullable STRING field named `staging_ip` on each
/// domain (`staging_ips` is only the `include=` query-param value, never
/// a field name). Verified against the go-fastly `Domain` model, whose
/// field is `StagingIP` with the mapstructure tag `staging_ip`, and its
/// recorded API fixture `fixtures/domains/list_with_staging_ips.yaml`,
/// plus Fastly's "working with staging" guide. The field is absent from
/// the published Domain data model, so it is treated as optional.
///
/// We also tolerate a plural `staging_ips` array, in case a Fastly
/// response (or a future API version) carries that shape.
fn parse_staging_ip(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    find_staging_ip(&value)
}

fn find_staging_ip(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            // The documented shape: a singular `staging_ip` string.
            if let Some(ip) = map.get("staging_ip").and_then(serde_json::Value::as_str) {
                return Some(ip.to_owned());
            }
            // Tolerated: a plural `staging_ips` array of strings.
            if let Some(ip) = map
                .get("staging_ips")
                .and_then(serde_json::Value::as_array)
                .and_then(|arr| arr.iter().find_map(serde_json::Value::as_str))
            {
                return Some(ip.to_owned());
            }
            map.values().find_map(find_staging_ip)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_staging_ip),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => None,
    }
}

/// Build the `curl` argv for a health probe. Production probes the
/// domain directly; staging reroutes the TLS connection to the
/// resolved staging IP via `--connect-to ::<ip>:443`. `path` is the
/// URL path (always begins with '/'), applied identically to both.
fn build_curl_probe_args(
    domain: &str,
    path: &str,
    staging_ip: Option<&str>,
    timeout_secs: u64,
) -> Vec<String> {
    let mut args = vec![
        // `-q` first so curl never merges `~/.curlrc` into a probe (a planted
        // `proxy`/`output` there could otherwise redirect or corrupt the check).
        "-q".to_owned(),
        "-sS".to_owned(),
        // Disable curl's URL globbing: a valid probe path may contain `[` `]` `{`
        // `}` (e.g. `/health?ids[0]=1`), which curl would otherwise treat as a
        // glob — failing with exit 3 or firing multiple requests, and so
        // mis-reporting a healthy deployment as unhealthy.
        "--globoff".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-w".to_owned(),
        "%{http_code}".to_owned(),
        "--max-time".to_owned(),
        timeout_secs.to_string(),
    ];
    if let Some(ip) = staging_ip {
        // `--connect-to ::HOST:PORT` reroutes the TLS connection to the staging
        // IP. An IPv6 literal must be bracketed or curl mis-parses the colons;
        // the caller has already validated `ip` parses as an `IpAddr`.
        let target = if ip.contains(':') {
            format!("::[{ip}]:443")
        } else {
            format!("::{ip}:443")
        };
        args.push("--connect-to".to_owned());
        args.push(target);
    }
    args.push(format!("https://{domain}{path}"));
    args
}

/// Validate a caller-supplied probe path. It is appended to
/// `https://{domain}` to form one curl argument, so it must begin with
/// '/' and carry no whitespace or control characters that would break
/// the URL or smuggle a second token.
fn validate_probe_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("healthcheck --path must begin with '/': '{path}'"));
    }
    if path.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(format!(
            "healthcheck --path must not contain whitespace or control characters: '{path}'"
        ));
    }
    Ok(())
}

/// Retry a health probe. Returns `Ok(code)` on the first healthy
/// status, or `Err((last_code, message))` after exhausting attempts.
/// `between` runs between attempts (not after the last) so it can be a
/// no-op in tests.
fn probe_with_retries<P, S>(
    retry: u32,
    mut prober: P,
    mut between: S,
) -> Result<u16, (Option<u16>, String)>
where
    P: FnMut() -> Result<u16, String>,
    S: FnMut(),
{
    let attempts = retry.max(1);
    let mut last_code = None;
    let mut last_msg = "no probe attempts were made".to_owned();
    for attempt in 0..attempts {
        match prober() {
            Ok(code) if is_healthy_status(code) => return Ok(code),
            Ok(code) => {
                last_code = Some(code);
                last_msg = format!("unhealthy HTTP status {code}");
            }
            Err(err) => last_msg = err,
        }
        if attempt.saturating_add(1) < attempts {
            between();
        }
    }
    Err((last_code, last_msg))
}

/// Run `fastly <args>` in `cwd`, inheriting stdio, and map a non-zero
/// exit to an error.
fn run_fastly_status(fastly_args: &[String], cwd: &Path) -> Result<(), String> {
    let status = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .status()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`fastly {}` exited with status {status}",
            fastly_args.join(" ")
        ))
    }
}

/// Run `fastly <args>` in `cwd` capturing stdout+stderr (combined) for
/// version parsing. Errors on a non-zero exit.
fn run_fastly_capture(fastly_args: &[String], cwd: &Path) -> Result<String, String> {
    let output = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "`fastly {}` exited with status {}\n{}",
            fastly_args.join(" "),
            output.status,
            combined.trim()
        ))
    }
}

/// Run `curl -q -sS --config -`, piping `config` (which carries the
/// `Fastly-Key` header + url) through stdin so the token never touches
/// argv. Returns stdout on a zero exit.
///
/// `-q` MUST be the first argument: without it curl reads `~/.curlrc`
/// (or `$CURL_HOME/.curlrc`) and merges it into this token-bearing
/// config, so a `proxy = …` directive planted by an earlier same-job
/// build step could exfiltrate the `Fastly-Key` header. `--connect-timeout`
/// / `--max-time` bound the call.
fn curl_config_capture(config: &str) -> Result<String, String> {
    let connect_timeout = FASTLY_API_CONNECT_TIMEOUT_SECS.to_string();
    let max_time = FASTLY_API_MAX_TIME_SECS.to_string();
    let mut child = Command::new("curl")
        .args([
            "-q",
            "-sS",
            "--connect-timeout",
            &connect_timeout,
            "--max-time",
            &max_time,
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                "`curl` not found on PATH; install curl and retry".to_owned()
            } else {
                format!("failed to spawn `curl`: {err}")
            }
        })?;
    // Take stdin OUT of the child and hand it to a helper BY VALUE, so it drops at
    // that helper's scope end — a natural drop rather than an explicit `drop(stdin)`,
    // which trips `clippy::drop_non_drop` on wasm targets where `ChildStdin` is not
    // `Drop`. The drop must precede `wait_with_output` so curl sees EOF (same pattern
    // as `write_value_to_fastly_stdin` on the fastly path).
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `curl`".to_owned())?;
    write_config_to_curl_stdin(stdin, config)?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `curl`: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else if output.status.code() == Some(CURL_EXIT_TIMEOUT) {
        Err(format!(
            "`curl` timed out after connect-timeout {FASTLY_API_CONNECT_TIMEOUT_SECS}s / max-time {FASTLY_API_MAX_TIME_SECS}s: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    } else {
        Err(format!(
            "`curl` exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Write `config` to curl's stdin, taking the handle BY VALUE so it drops at this
/// function's scope end. That natural drop closes the pipe (curl sees EOF) without
/// an explicit `drop(stdin)`, which trips `clippy::drop_non_drop` on wasm targets
/// where `ChildStdin` is not `Drop` (mirrors `write_value_to_fastly_stdin`).
fn write_config_to_curl_stdin(mut stdin: ChildStdin, config: &str) -> Result<(), String> {
    stdin
        .write_all(config.as_bytes())
        .map_err(|err| format!("failed to write curl config to stdin: {err}"))
}

/// Wrap `value` in a curl-config double-quoted string, escaping the
/// characters that would otherwise let a value terminate its quote and
/// inject additional curl options. Within a curl `--config` file a
/// double-quoted value only honours the escapes `\\`, `\"`, `\n`, `\r`,
/// `\t` (and the config is parsed line-by-line, so a raw newline ends
/// the directive regardless of quoting). We escape backslash and quote
/// so the value cannot break out of the quotes, and map raw control
/// characters to their escape form so NO raw newline (or CR/tab) is
/// ever written into the config file. This is the second half of the
/// injection defence: untrusted identifiers are also validated (see
/// `validate_service_id` / `validate_version_str` / `validate_domain`),
/// but the token is a secret we cannot constrain to a charset, so it
/// relies on this escaping alone.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len().saturating_add(2));
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Validate an operator-supplied Fastly service id before it is
/// interpolated into an API URL. Fastly service ids are opaque
/// alphanumeric handles; constrain to `^[A-Za-z0-9_-]+$` so a value
/// carrying a quote / newline / space (which could inject curl options
/// via the `--config` file) is rejected with a clear error.
fn validate_service_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        Ok(())
    } else {
        Err(format!(
            "invalid service id {id:?}: expected only ASCII letters, digits, `_`, or `-`"
        ))
    }
}

/// Validate a service-version string is a plain non-negative integer
/// before it is interpolated into an API URL. Returns the parsed value
/// so callers can reuse it.
fn validate_version_str(version: &str) -> Result<u64, String> {
    version.parse::<u64>().map_err(|err| {
        format!("invalid version {version:?}: expected a non-negative integer: {err}")
    })
}

/// Validate a domain is a plausible hostname before it is placed into a
/// `curl` URL. Rejects anything outside the DNS label charset
/// (`[A-Za-z0-9-.]`), empty / over-long values, leading/trailing dots,
/// and empty labels so an injected quote / slash / space / newline
/// cannot smuggle curl options or a second URL.
fn validate_domain(domain: &str) -> Result<(), String> {
    let charset_ok = domain
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.');
    let shape_ok = !domain.is_empty()
        && domain.len() <= 253
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..");
    if charset_ok && shape_ok {
        Ok(())
    } else {
        Err(format!(
            "invalid domain {domain:?}: expected a hostname like `example.com`"
        ))
    }
}

/// `GET https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the response body ONLY on a 2xx status. Both the header (carrying the
/// secret token) and the URL are written through `curl_quote` so neither can
/// inject curl options into the `--config` document.
///
/// The HTTP status is captured explicitly via `write-out` (as the PUT helper
/// does) and required to be 2xx before the body is trusted. `--fail` alone would
/// reject 4xx/5xx but still accept a 3xx — whose (array-shaped) body could
/// otherwise be parsed as version data. No `location` directive is set, so a
/// redirect is never followed.
fn fastly_api_get(path: &str, token: &str) -> Result<String, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    // `write-out` appends the status on its own trailing line AFTER the body.
    let config = format!("header = {header}\nurl = {url}\nwrite-out = \"\\n%{{http_code}}\"\n");
    let out = curl_config_capture(&config)
        .map_err(|err| format!("Fastly API GET {path} failed: {err}"))?;
    let (body, status_line) = out
        .rsplit_once('\n')
        .ok_or_else(|| format!("Fastly API GET {path}: no HTTP status in the curl output"))?;
    let status: u16 = status_line.trim().parse().map_err(|err| {
        format!(
            "Fastly API GET {path}: could not parse the HTTP status {:?}: {err}",
            status_line.trim()
        )
    })?;
    if !(200..300).contains(&status) {
        return Err(format!("Fastly API GET {path} returned HTTP {status}"));
    }
    Ok(body.to_owned())
}

/// `PUT https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the HTTP status, erroring on non-2xx. Fastly's version
/// activate/deactivate endpoints require `PUT` (not `POST`). Header and
/// URL are escaped via `curl_quote`; the literal `request`, `output`,
/// and `write-out` directives are fixed constants.
fn fastly_api_put(path: &str, token: &str) -> Result<u16, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    let config = format!(
        "request = \"PUT\"\nheader = {header}\nurl = {url}\noutput = \"/dev/null\"\nwrite-out = \"%{{http_code}}\"\n"
    );
    let out = curl_config_capture(&config)?;
    let code: u16 = out.trim().parse().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            out.trim()
        )
    })?;
    if (200..300).contains(&code) {
        Ok(code)
    } else {
        Err(format!("Fastly API PUT {path} returned HTTP {code}"))
    }
}

/// Resolve the directory containing the Fastly manifest for a deploy
/// (production [`deploy`] or [`deploy_staged`]).
///
/// The CLI (`edgezero_cli::run_deploy`) resolves the `edgezero.toml`
/// manifest — honouring `EDGEZERO_MANIFEST` — and threads the
/// manifest-configured `[adapters.fastly.adapter].manifest` path in as
/// `--manifest-path <abs fastly.toml>`. Prefer that so a monorepo with
/// multiple Fastly apps deploys/stages the app the operator actually
/// selected, rather than whichever `fastly.toml` a bare working-directory
/// search happens to find first. Only when no `--manifest-path` is
/// threaded (e.g. a manifest that declares Fastly commands but no adapter
/// `manifest` key) do we fall back to the working-directory search.
fn resolve_manifest_dir(args: &[String]) -> Result<PathBuf, String> {
    if let Some(raw) = arg_value(args, "--manifest-path") {
        let path = PathBuf::from(raw);
        return path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("fastly manifest path {raw:?} has no parent directory"));
    }
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())
}

/// `deploy --adapter fastly --service-id <id> --staging`:
/// build, upload to a new draft version (no activation), stage it, and
/// emit `version=<N>`.
fn deploy_staged(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    // The Fastly CLI reads FASTLY_API_TOKEN from the env; fail fast
    // with a clear message when it's missing rather than deep in a
    // `fastly compute update` error.
    require_token()?;

    let manifest_dir_buf = resolve_manifest_dir(args)?;
    let manifest_dir = manifest_dir_buf.as_path();
    // The CLI threads the app's declared config-store logical ids as
    // `--edgezero-staging-config=<logical>` (one per store) so the staging relink
    // knows which selectors to redirect — read from the app manifest, never a
    // remote probe. These are EdgeZero-internal inline tokens; strip them so they
    // never reach `fastly compute update`.
    let config_logical_ids: Vec<String> = args
        .iter()
        .filter_map(|arg| {
            arg.strip_prefix("--edgezero-staging-config=")
                .map(str::to_owned)
        })
        .collect();
    let deploy_args: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--edgezero-staging-config="))
        .cloned()
        .collect();
    // Strip both the explicitly-threaded `--service-id` and the
    // CLI-injected `--manifest-path` (which `fastly compute update`
    // doesn't understand), then keep only the passthrough flags
    // `compute update` actually supports. `--comment` in particular is
    // NOT a `compute update` flag — it is lifted out here and applied to
    // the version below.
    let extra = args_without_flag_value(
        &args_without_flag_value(&deploy_args, "--service-id"),
        "--manifest-path",
    );
    let passthrough = split_staged_passthrough(&extra);
    if !passthrough.dropped.is_empty() {
        log::warn!(
            "[edgezero] ignoring deploy args not supported by `fastly compute update`: {}",
            passthrough.dropped.join(" ")
        );
    }

    // 1. Build the wasm package (no deploy / activation).
    run_fastly_status(
        &[
            "compute".to_owned(),
            "build".to_owned(),
            "--non-interactive".to_owned(),
        ],
        manifest_dir,
    )?;

    // 2. Clone the active version into a new draft and upload the
    //    package to it — `--autoclone` + `--version=active` keeps
    //    production traffic on the currently-active version.
    let mut update = vec![
        "compute".to_owned(),
        "update".to_owned(),
        "--autoclone".to_owned(),
        format!("--service-id={service_id}"),
        "--version=active".to_owned(),
    ];
    update.extend(passthrough.forwarded.iter().cloned());
    if !has_non_interactive(&passthrough.forwarded) {
        update.push("--non-interactive".to_owned());
    }
    let update_out = run_fastly_capture(&update, manifest_dir)?;

    // Resolve the new draft version from the update output. FAIL CLOSED:
    // if the version cannot be parsed with confidence we return an error
    // rather than guessing. The old fallback picked the service's
    // HIGHEST version, which under concurrent deploys could silently
    // stage/roll back a version created by someone else's run.
    let version = parse_fastly_version(&update_out).ok_or_else(|| {
        format!(
            "could not determine the staged version from `fastly compute update` output; \
             refusing to guess (a wrong version would stage another deploy's changes). \
             Raw output:\n{update_out}"
        )
    })?;

    // 3. Apply the operator's `--comment` to the freshly-created draft.
    //    `compute update` has no `--comment`; the version comment is set
    //    with `service-version update`. Done BEFORE staging, while the
    //    version is still an editable draft (and without `--autoclone`,
    //    so it can never clone into yet another version).
    if let Some(comment) = passthrough.comment.as_deref() {
        run_fastly_status(
            &[
                "service-version".to_owned(),
                "update".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                "--comment".to_owned(),
                comment.to_owned(),
            ],
            manifest_dir,
        )?;
    }

    // 4. Point the draft's runtime-override link at the STAGING selector store,
    //    so this version reads staged config and production keeps reading its
    //    own. Done while the version is still an editable draft.
    relink_runtime_env_for_staging(&service_id, version, &config_logical_ids, manifest_dir)?;

    // 5. Mark the draft version staged (no activation).
    run_fastly_status(
        &[
            "service-version".to_owned(),
            "stage".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
        ],
        manifest_dir,
    )?;

    // 6. Emit the staged version (parseable contract).
    log::info!("version={version}");
    Ok(())
}

/// Point a staged draft's `edgezero_runtime_env` link at the STAGING selector
/// store, so the staged version reads staged config.
///
/// Why this exists: `compute update --autoclone --version=active` clones the
/// active version, and a clone inherits its resource links. Without this, a
/// staged version opens the SAME `edgezero_runtime_env` store as production and
/// therefore reads production's config key — `config push --staging` would write
/// `<key>_staging` that nothing ever reads. Flipping the shared store's selector
/// instead is worse: it redirects production too.
///
/// Fastly resource links are per-version and their `name` is an overridable
/// alias, so linking the staging store under the name `edgezero_runtime_env`
/// gives this draft (and only this draft) staged config.
///
/// Fails closed: if the staging store does not exist we refuse rather than stage
/// a version that would silently serve production config.
fn relink_runtime_env_for_staging(
    service_id: &str,
    version: u64,
    config_logical_ids: &[String],
    manifest_dir: &Path,
) -> Result<(), String> {
    // An app that declares no config stores has no selector to isolate, so
    // staging is still perfectly meaningful for it (staged CODE, no config): the
    // draft keeps the inherited production link and this is a no-op.
    if config_logical_ids.is_empty() {
        log::info!(
            "app declares no config stores, so staged version {version} has no config selector to isolate; keeping the inherited runtime-env link"
        );
        return Ok(());
    }

    // Read the PRODUCTION runtime-override entries to mirror. Fail CLOSED on a
    // lookup FAILURE (CLI missing / non-zero exit / schema drift) — treating
    // "couldn't tell" as "no store" would stage a version that silently reads
    // production config. A genuine `NotFound` is NOT a no-op here: the app
    // DECLARES config (checked above), so the staged version must still be
    // isolated. There is simply nothing to mirror — the twin gets only the
    // derived `<logical>_staging` selectors, and the staged draft is relinked to
    // it so it reads staged config while production keeps its default key.
    let production = match classify_remote_config_store(RUNTIME_ENV_STORE_NAME)? {
        ConfigStoreLookup::Found(id) => read_config_store_entries(&id, manifest_dir)?,
        ConfigStoreLookup::NotFound => Vec::new(),
        ConfigStoreLookup::SchemaDrift(detail) => {
            return Err(format!(
                "could not parse `fastly config-store list --json` while resolving `{RUNTIME_ENV_STORE_NAME}` for a staged deploy: {detail}.\n  Refusing to stage rather than risk serving PRODUCTION config. Pin a known-compatible fastly CLI version and retry."
            ));
        }
    };

    // Mirror production's runtime overrides into the PER-SERVICE staging twin,
    // overriding only the config selectors to `<logical>_staging`, then point
    // THIS draft at the twin. Create the twin on demand so a staged deploy never
    // depends on a prior provision having created it.
    let staging_store_name = staging_selector_store_name(service_id);
    let staging_store_id = ensure_staging_selector_store(&staging_store_name)?;
    mirror_production_to_staging(
        &production,
        &staging_store_id,
        config_logical_ids,
        manifest_dir,
    )?;

    // Drop the inherited production link first: a version cannot carry two links
    // under the same name.
    let existing = run_fastly_capture(
        &[
            "resource-link".to_owned(),
            "list".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            "--json".to_owned(),
        ],
        manifest_dir,
    )?;
    if let Some(link_id) = find_resource_link_id(&existing, RUNTIME_ENV_STORE_NAME) {
        run_fastly_status(
            &[
                "resource-link".to_owned(),
                "delete".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                format!("--id={link_id}"),
            ],
            manifest_dir,
        )?;
    }

    // `--name` is the alias the runtime opens; the linked STORE is the staging
    // twin. No `--autoclone`: the draft is already editable, and cloning here
    // would silently move us onto yet another version.
    run_fastly_status(
        &[
            "resource-link".to_owned(),
            "create".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            format!("--resource-id={staging_store_id}"),
            format!("--name={RUNTIME_ENV_STORE_NAME}"),
        ],
        manifest_dir,
    )?;

    log::info!("staged version {version} now reads `{staging_store_name}` for its config selector");
    Ok(())
}

/// Production companion to `deploy`: resolve the active service version via the
/// Fastly API and emit it as a `version=<N>` line.
///
/// Distinguishes "confirmed no active version" from an operational failure: a
/// service with no active version yet (a first-ever deploy) is NOT an error — it
/// emits an empty `version=` line and succeeds, so the caller records an empty
/// rollback target. Only a real failure (API/auth error, or a version list that
/// cannot be parsed) returns `Err`, so the caller can fail closed instead of
/// silently proceeding without a rollback target.
///
/// `--require-active` flips the no-active-version case to an error: it is passed
/// by the production-`deploy` version fallback, where a version was JUST
/// activated, so "no active version" is not a valid first-deploy state but an
/// operational failure the CLI must not report as success.
fn emit_active_version(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let token = require_token()?;
    let json = fastly_api_get(&format!("/service/{service_id}/version"), &token)?;
    if let Some(version) =
        active_version_or_require(&json, arg_flag(args, "--require-active"), &service_id)?
    {
        log::info!("version={version}");
    } else {
        // Confirmed no active version (first-ever deploy), and it was not
        // required. Emit an explicit empty line so the caller records an empty
        // rollback target and succeeds — distinct from a failure (`Err`).
        log::info!("version=");
        log::info!(
            "service {service_id} has no active version yet; emitting an empty rollback target"
        );
    }
    Ok(())
}

/// Resolve the active version and apply the `--require-active` policy.
///
/// `Ok(Some(n))` — a version is active. `Ok(None)` — no active version and
/// `require_active` is false (a first-ever `active-version` call; the caller
/// records an empty rollback target). `Err` — the response was malformed
/// ([`resolve_active_version`]), OR no version is active while `require_active`
/// is true. The latter is the production-`deploy` fallback: a version was JUST
/// activated, so "no active version" is an error, not a valid empty result.
fn active_version_or_require(
    json: &str,
    require_active: bool,
    service_id: &str,
) -> Result<Option<u64>, String> {
    match resolve_active_version(json)? {
        Some(version) => Ok(Some(version)),
        None if require_active => Err(format!(
            "the deploy reported success but the Fastly API returns no active version for service {service_id}; refusing to report a deploy with no resolvable version"
        )),
        None => Ok(None),
    }
}

/// Require `version` to be the currently ACTIVE service version — the
/// production healthcheck's version contract.
///
/// The production probe hits the live domain, which serves whatever version is
/// active, so "healthcheck version N" is only a true statement about N while N is
/// active. `phase` (`before probing` / `after probing`) names when the check ran,
/// so a version activated by a concurrent deploy is reported clearly rather than
/// masquerading as a healthy `version`.
fn verify_version_active(
    service_id: &str,
    version: u64,
    token: &str,
    phase: &str,
) -> Result<(), String> {
    let json = fastly_api_get(&format!("/service/{service_id}/version"), token)?;
    version_active_verdict(resolve_active_version(&json)?, version, service_id, phase)
}

/// The pure decision behind [`verify_version_active`], split out so the version
/// contract is unit-testable without a live Fastly API.
fn version_active_verdict(
    active: Option<u64>,
    version: u64,
    service_id: &str,
    phase: &str,
) -> Result<(), String> {
    match active {
        Some(active_version) if active_version == version => Ok(()),
        Some(active_version) => Err(format!(
            "production healthcheck version {version} is not active {phase}: service {service_id} currently has version {active_version} active, so the live-domain probe reflects version {active_version}, not {version}"
        )),
        None => Err(format!(
            "production healthcheck version {version} could not be confirmed active {phase}: service {service_id} has no active version"
        )),
    }
}

/// `healthcheck --adapter fastly ...`: probe the domain
/// (production) or the version's staging IP (`--staging`), retrying up
/// to `--retry` times. Emits `status-code` / `healthy` and returns
/// `Err` (non-zero exit) when unhealthy after retries.
///
/// `--domain`, `--service-id` and `--version` are REQUIRED and validated
/// on BOTH the production and the staging path. GitHub Actions' `required:
/// true` does not actually fail a workflow when an input is omitted or
/// empty, so this is the real guard: a production healthcheck must never
/// probe on behalf of an absent/empty version it never verified — the
/// caller chains that same version into rollback.
///
/// On the PRODUCTION path the probe reaches whatever version is live, so when a
/// token is available `version` is verified ACTIVE before and after the probe
/// (see [`verify_version_active`]); without a token the check is service-level.
fn healthcheck(args: &[String]) -> Result<(), String> {
    let domain =
        arg_value(args, "--domain").ok_or_else(|| "healthcheck requires --domain".to_owned())?;
    validate_domain(domain)?;
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "healthcheck requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let path = arg_value(args, "--path").unwrap_or("/");
    validate_probe_path(path)?;
    let retry = arg_value(args, "--retry")
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_u32);
    let retry_delay = arg_value(args, "--retry-delay")
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_u64);
    let timeout = arg_value(args, "--timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_u64);
    // curl reads `--max-time 0` as "no limit", so a zero timeout lets a single
    // probe run indefinitely. Require a positive value.
    if timeout == 0 {
        return Err("healthcheck --timeout must be a positive number of seconds".to_owned());
    }

    let is_staging = arg_flag(args, "--staging");
    let staging_ip = if is_staging {
        let token = require_token()?;
        let json = fastly_api_get(
            &format!("/service/{service_id}/version/{version}/domain?include=staging_ips"),
            &token,
        )?;
        let ip = parse_staging_ip(&json).ok_or_else(|| {
            format!("no staging IP found for service {service_id} version {version}")
        })?;
        // `find_staging_ip` searches the response structurally and could surface a
        // non-address string; require a real `IpAddr` before it reaches curl's
        // `--connect-to`, which also settles IPv4-vs-IPv6 formatting.
        ip.parse::<IpAddr>().map_err(|err| {
            format!("resolved staging IP {ip:?} is not a valid IP address: {err}")
        })?;
        Some(ip)
    } else {
        None
    };

    // Production version contract: the probe hits the live domain, which serves
    // whatever version is ACTIVE — not necessarily `version`. When a token is
    // available, require `version` to be active both BEFORE and AFTER the probe, so
    // a version activated concurrently (by another deploy) cannot be reported as a
    // healthy `version`. Without a token the production check is inherently
    // service-level — say so rather than imply a version-specific guarantee. The
    // staging path already targets the specific version's staging IP, so it needs
    // no such check.
    let production_token = if is_staging {
        None
    } else {
        match env::var(FASTLY_API_TOKEN_ENV) {
            Ok(token) if !token.is_empty() => Some(token),
            _ => {
                log::info!(
                    "no {FASTLY_API_TOKEN_ENV} available; production healthcheck is service-level (probes the live domain for service {service_id}, not specifically version {version})"
                );
                None
            }
        }
    };
    if let Some(token) = production_token.as_deref() {
        verify_version_active(&service_id, version, token, "before probing")?;
    }

    let curl_args = build_curl_probe_args(domain, path, staging_ip.as_deref(), timeout);
    let delay = Duration::from_secs(retry_delay);
    let outcome = probe_with_retries(retry, || curl_status(&curl_args), || thread::sleep(delay));
    match outcome {
        Ok(code) => {
            // Confirm `version` is STILL active, so a deploy that activated a newer
            // version during the probe+retries is not reported as a healthy `version`.
            if let Some(token) = production_token.as_deref() {
                verify_version_active(&service_id, version, token, "after probing")?;
            }
            log::info!("status-code={code}");
            log::info!("healthy=true");
            Ok(())
        }
        Err((last_code, msg)) => {
            if let Some(code) = last_code {
                log::info!("status-code={code}");
            }
            log::info!("healthy=false");
            Err(format!(
                "healthcheck for {domain} failed after {} attempt(s): {msg}",
                retry.max(1)
            ))
        }
    }
}

/// Run a single `curl` health probe, returning the HTTP status. A
/// transport failure (timeout, DNS, refused) surfaces as `Err` so the
/// retry loop treats it as an unhealthy attempt.
fn curl_status(args: &[String]) -> Result<u16, String> {
    let output = Command::new("curl").args(args).output().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            "`curl` not found on PATH; install curl and retry".to_owned()
        } else {
            format!("failed to spawn `curl`: {err}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "curl transport failure (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u16>().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            stdout.trim()
        )
    })
}

/// `rollback --adapter fastly ...`: production activates the explicit
/// `--rollback-to` version (Fastly cannot infer a previous version);
/// staging deactivates `<version>`.
fn rollback(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "rollback requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let token = require_token()?;

    if arg_flag(args, "--staging") {
        // Staging rollback deactivates the STAGED version on the
        // `staging` environment. Fastly's environment-scoped
        // deactivate is `PUT .../deactivate/staging` (a plain
        // `.../deactivate` would target the production activation).
        fastly_api_put(
            &format!("/service/{service_id}/version/{version}/deactivate/staging"),
            &token,
        )?;
        log::info!(
            "[edgezero] deactivated staged version {version} on Fastly service {service_id}"
        );
    } else {
        // Production rollback re-activates an EXPLICIT target. Fastly's version
        // list has no field distinguishing a previously-live version from a
        // staged one (`staging`/`deployed` are documented "Unused"; `locked`
        // only means "not editable"), so the target cannot be inferred — it is
        // captured before the superseding deploy and passed in as --rollback-to.
        let previous = arg_value(args, "--rollback-to")
            .and_then(|raw| validate_version_str(raw).ok())
            .ok_or_else(|| {
                "production rollback requires a valid --rollback-to version".to_owned()
            })?;
        // Best-effort staleness check: the version being rolled back FROM
        // (`--version`) must STILL be the active version. A rollback workflow can
        // run long after its deploy — if a newer version was activated meanwhile,
        // activating the old target would clobber that newer deploy, so refuse.
        //
        // This is NOT atomic: Fastly's activate endpoint has no precondition, so
        // a deploy that lands BETWEEN this read and the activate below can still
        // be clobbered. It narrows the window (catching the common much-later
        // rollback) but does not close it — serialise deploys and rollbacks per
        // SERVICE (a service-scoped concurrency group) to eliminate the race.
        let json = fastly_api_get(&format!("/service/{service_id}/version"), &token)?;
        ensure_rollback_from_is_active(resolve_active_version(&json)?, version, &service_id)?;
        // Fastly's activate endpoint requires `PUT` (not `POST`).
        fastly_api_put(
            &format!("/service/{service_id}/version/{previous}/activate"),
            &token,
        )?;
        log::info!("rolled-back-to={previous}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    #[cfg(unix)]
    use edgezero_core::test_env::{EnvOverride, PathPrepend};
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

    // `PathPrepend` (RAII $PATH guard) is the shared helper imported above from
    // `edgezero_core::test_env`; the merge with edition-2024 main replaced our
    // local copy with it (its `set_var` calls are wrapped for 2024's unsafe-env).

    // ── Fastly staging lifecycle helpers ──────────────────────────────

    #[test]
    fn arg_value_reads_flag_value() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--version".to_owned(),
            "42".to_owned(),
        ];
        assert_eq!(arg_value(&args, "--service-id"), Some("SVC1"));
        assert_eq!(arg_value(&args, "--version"), Some("42"));
        assert_eq!(arg_value(&args, "--missing"), None);
    }

    #[test]
    fn arg_value_none_when_flag_is_last() {
        let args = vec!["--version".to_owned()];
        assert_eq!(arg_value(&args, "--version"), None);
    }

    #[test]
    fn arg_flag_detects_presence() {
        let args = vec!["--staging".to_owned()];
        assert!(arg_flag(&args, "--staging"));
        assert!(!arg_flag(&args, "--nope"));
    }

    #[test]
    fn args_without_flag_value_strips_pair() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--comment".to_owned(),
            "ci".to_owned(),
        ];
        assert_eq!(
            args_without_flag_value(&args, "--service-id"),
            vec!["--comment".to_owned(), "ci".to_owned()]
        );
    }

    #[test]
    fn resolve_manifest_dir_prefers_manifest_path_flag() {
        // When the CLI threads `--manifest-path <abs fastly.toml>`, the
        // deploy (production AND staged) must use its parent directory
        // rather than a bare working-directory search (which in a
        // monorepo could pick a different app's fastly.toml).
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            "/repo/apps/edge/fastly.toml".to_owned(),
        ];
        let dir = resolve_manifest_dir(&args).expect("resolves from --manifest-path");
        assert_eq!(dir, PathBuf::from("/repo/apps/edge"));
    }

    #[test]
    fn resolve_service_id_prefers_flag() {
        let args = vec!["--service-id".to_owned(), "SVC_FROM_ARG".to_owned()];
        assert_eq!(resolve_service_id(&args).unwrap(), "SVC_FROM_ARG");
    }

    // ── `compute update` passthrough filtering (`--comment`) ─────────

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn split_staged_passthrough_lifts_comment_out_of_compute_update() {
        // `fastly compute update` has NO `--comment` flag (verified against
        // `fastly compute update --help`, CLI v15) — forwarding it makes the
        // command exit non-zero and fails the whole staged deploy. It must be
        // lifted out and applied via `service-version update` instead.
        for args in [owned(&["--comment", "ci run 12"]), owned(&["--comment=x"])] {
            let split = split_staged_passthrough(&args);
            assert!(
                !split
                    .forwarded
                    .iter()
                    .any(|arg| arg.starts_with("--comment")),
                "--comment must never reach `compute update`: {:?}",
                split.forwarded
            );
            assert!(
                split.comment.is_some(),
                "comment must be captured: {args:?}"
            );
        }
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment", "ci run 12"])).comment,
            Some("ci run 12".to_owned())
        );
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment=x"])).comment,
            Some("x".to_owned())
        );
    }

    #[test]
    fn split_staged_passthrough_forwards_supported_flags_only() {
        let args = owned(&[
            "--package",
            "pkg.tar.gz",
            "--autoclone",
            "--verbose",
            "--comment",
            "note",
            "--env",
            "stage",
            "--status-check-off",
        ]);
        let split = split_staged_passthrough(&args);
        // Supported by `compute update`: kept (value flags keep their value).
        assert_eq!(
            split.forwarded,
            owned(&["--package", "pkg.tar.gz", "--autoclone", "--verbose"])
        );
        // `--env`/`--status-check-off` are `compute deploy` flags, not
        // `compute update` ones: dropped, and `--env`'s detached value
        // `stage` is dropped with it (never left as a bogus positional).
        assert_eq!(split.dropped, owned(&["--env", "--status-check-off"]));
        assert!(!split.forwarded.iter().any(|arg| arg == "stage"));
        assert_eq!(split.comment, Some("note".to_owned()));
    }

    // ── non-interactive CI safety (`--non-interactive`) ───────────────

    #[test]
    fn build_compute_deploy_args_is_non_interactive() {
        // Without this a production deploy can block on an interactive
        // prompt in CI.
        let argv = build_compute_deploy_args(&owned(&["--service-id", "SVC1"]));
        assert_eq!(
            argv,
            owned(&[
                "compute",
                "deploy",
                "--service-id",
                "SVC1",
                "--non-interactive"
            ])
        );
    }

    #[test]
    fn build_compute_deploy_args_does_not_duplicate_caller_flag() {
        for flag in ["--non-interactive", "-i"] {
            let argv = build_compute_deploy_args(&owned(&[flag]));
            assert_eq!(
                argv.iter()
                    .filter(|arg| *arg == "--non-interactive" || *arg == "-i")
                    .count(),
                1,
                "must not pass the non-interactive switch twice ({flag})"
            );
        }
    }

    // ── healthcheck / rollback input validation ───────────────────────
    //
    // GitHub Actions' `required: true` does NOT fail when an input is
    // omitted or empty, so the CLI is the real guard. An absent / empty /
    // malformed `--service-id` or `--version` must be rejected on BOTH
    // the production and the staging path — a production healthcheck
    // that probes anyway "verifies" a version it never looked at, and
    // the caller chains that same version into rollback.

    #[test]
    fn healthcheck_rejects_missing_or_empty_required_values_on_production() {
        for (args, needle) in [
            (
                owned(&["--domain", "example.com", "--service-id", "SVC1"]),
                "--version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "15.2.0",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "",
                    "--version",
                    "7",
                ]),
                "invalid service id",
            ),
            (
                owned(&["--domain", "", "--service-id", "SVC1", "--version", "7"]),
                "invalid domain",
            ),
            (
                owned(&["--service-id", "SVC1", "--version", "7"]),
                "--domain",
            ),
        ] {
            let err = healthcheck(&args).expect_err("must reject absent/empty required value");
            assert!(
                err.contains(needle),
                "expected {needle:?} in error for {args:?}, got: {err}"
            );
        }
    }

    #[test]
    fn healthcheck_rejects_empty_required_values_on_staging() {
        for args in [
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "",
                "--version",
                "7",
            ]),
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "SVC1",
                "--version",
                "",
            ]),
        ] {
            healthcheck(&args).expect_err("staging must reject empty required values");
        }
    }

    #[test]
    fn rollback_rejects_missing_or_invalid_required_values() {
        for staging in [&[][..], &["--staging".to_owned()][..]] {
            for bad in [
                owned(&["--service-id", "SVC1"]),
                owned(&["--service-id", "SVC1", "--version", ""]),
                owned(&["--service-id", "SVC1", "--version", "12abc"]),
                owned(&["--service-id", "", "--version", "7"]),
            ] {
                let mut args = bad.clone();
                args.extend_from_slice(staging);
                rollback(&args).expect_err("rollback must reject invalid required values");
            }
        }
    }

    // ── curl-config escaping + input validation (injection defence) ───

    #[test]
    fn curl_quote_escapes_quotes_and_backslashes() {
        assert_eq!(curl_quote("plain"), "\"plain\"");
        assert_eq!(curl_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(curl_quote("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn curl_quote_never_emits_raw_control_characters() {
        // A token carrying a `"` and a newline must not be able to
        // terminate its quoted value and inject a second `url = "..."`
        // directive. The `"` is escaped and the newline is folded to a
        // `\n` escape so NO raw newline reaches the curl config file.
        let token = "tok\"en\nurl = \"https://evil.example\"";
        let quoted = curl_quote(token);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert!(!quoted.contains('\n'), "no raw newline: {quoted}");
        assert!(!quoted.contains('\r'));
        // The only unescaped `"` are the wrapping pair; every interior
        // quote is preceded by a backslash.
        assert_eq!(quoted, "\"tok\\\"en\\nurl = \\\"https://evil.example\\\"\"");
        // A tab folds too.
        assert_eq!(curl_quote("a\tb"), "\"a\\tb\"");
    }

    #[test]
    fn validate_service_id_accepts_opaque_handles() {
        validate_service_id("SU1Z0isxPaozGVKXdv0eY").expect("alphanumeric handle");
        validate_service_id("abc_DEF-123").expect("underscore + dash handle");
    }

    #[test]
    fn validate_service_id_rejects_injection_and_empty() {
        // The canonical attack: a service id that closes the url value
        // and appends a second url directive.
        validate_service_id("abc\nurl = \"http://evil\"").expect_err("newline injection");
        validate_service_id("abc\"def").expect_err("quote");
        validate_service_id("has space").expect_err("space");
        validate_service_id("has/slash").expect_err("slash");
        validate_service_id("").expect_err("empty");
    }

    #[test]
    fn validate_version_str_accepts_integer_rejects_junk() {
        assert_eq!(validate_version_str("42"), Ok(42));
        assert_eq!(validate_version_str("0"), Ok(0));
        validate_version_str("-1").expect_err("negative");
        validate_version_str("4.2").expect_err("float");
        validate_version_str("42\nurl = \"x\"").expect_err("newline injection");
        validate_version_str("").expect_err("empty");
    }

    #[test]
    fn validate_domain_accepts_hostnames_rejects_injection() {
        validate_domain("example.com").expect("bare hostname");
        validate_domain("staging.example.co.uk").expect("multi-label hostname");
        validate_domain("host-1.example.com").expect("hostname with dash");
        validate_domain("").expect_err("empty");
        validate_domain(".example.com").expect_err("leading dot");
        validate_domain("example.com.").expect_err("trailing dot");
        validate_domain("exa..mple.com").expect_err("empty label");
        validate_domain("example.com/evil").expect_err("slash");
        validate_domain("example.com\nurl = \"x\"").expect_err("newline injection");
        validate_domain("has space.com").expect_err("space");
    }

    #[test]
    fn version_active_verdict_enforces_the_production_version_contract() {
        // The requested version is the active one: healthy.
        version_active_verdict(Some(7), 7, "SVC1", "before probing").expect("match is ok");
        // A different active version (a concurrent deploy) must fail closed and name
        // BOTH versions so the mismatch is diagnosable.
        let err = version_active_verdict(Some(9), 7, "SVC1", "after probing")
            .expect_err("a newer active version must fail the version contract");
        assert!(err.contains('7') && err.contains('9'), "{err}");
        // No active version at all is not a healthy version-7 report either.
        version_active_verdict(None, 7, "SVC1", "before probing")
            .expect_err("no active version must fail the contract");
    }

    #[test]
    fn is_healthy_status_covers_2xx_only() {
        assert!(is_healthy_status(200));
        assert!(is_healthy_status(204));
        assert!(is_healthy_status(299));
        // 3xx is NOT healthy: the probe does not follow redirects, so a 301 to an
        // error page must not pass a gate that suppresses an automatic rollback.
        assert!(!is_healthy_status(301));
        assert!(!is_healthy_status(399));
        assert!(!is_healthy_status(400));
        assert!(!is_healthy_status(500));
        assert!(!is_healthy_status(199));
    }

    #[test]
    fn parse_fastly_version_handles_the_shapes_fastly_emits() {
        // The Fastly CLI's own success lines. Go format strings:
        //   "Updated package (service %s, version %v)"  (compute update)
        //   "Deployed package (service %s, version %v)" (compute deploy)
        assert_eq!(
            parse_fastly_version("SUCCESS: Deployed package (service abc, version 7)"),
            Some(7)
        );
        assert_eq!(
            parse_fastly_version("\nSUCCESS: Updated package (service SU1Z0, version 42)\n"),
            Some(42)
        );
        // Our canonical contract line.
        assert_eq!(parse_fastly_version("version=12"), Some(12));
        // The --autoclone notice, when no success line is present.
        assert_eq!(
            parse_fastly_version(
                "Service version 3 is not editable, so it was automatically cloned because \
                 --autoclone is enabled. Now operating on version 4."
            ),
            Some(4)
        );
        // Full autoclone + success output: the SUCCESS line wins, and the
        // PRE-clone version (3) never does — even though stdout/stderr are
        // concatenated and their relative order is not guaranteed.
        let combined = "SUCCESS: \nUpdated package (service abc, version 4)\n\
             Service version 3 is not editable, so it was automatically cloned. \
             Now operating on version 4.";
        assert_eq!(parse_fastly_version(combined), Some(4));
        assert_eq!(parse_fastly_version("no numbers here"), None);
    }

    #[test]
    fn parse_fastly_version_rejects_confusable_lines() {
        // The old parser took ANY digits after the word "version", so each
        // of these silently produced a WRONG service version. They must now
        // all be `None`, which makes `deploy_staged` fail closed.
        assert_eq!(
            parse_fastly_version("Uploaded package to service 12345, version unchanged"),
            None
        );
        // The CLI's own semver must not be mistaken for a service version.
        assert_eq!(parse_fastly_version("Fastly CLI version 15.2.0"), None);
        assert_eq!(
            parse_fastly_version("Checking version compatibility for service 99"),
            None
        );
        // A bare `version <N>` mention with no success-line context is not
        // trusted either.
        assert_eq!(parse_fastly_version("cloning version 3"), None);
        // `--version=active` echoed in a command line is not a contract line.
        assert_eq!(
            parse_fastly_version("running: fastly compute update --version=active"),
            None
        );
    }

    #[test]
    fn parse_active_version_finds_active_entry() {
        let json = r#"[
            {"number": 1, "active": false},
            {"number": 2, "active": true},
            {"number": 3, "active": false}
        ]"#;
        assert_eq!(resolve_active_version(json), Ok(Some(2)));
    }

    #[test]
    fn parse_active_version_none_when_no_active() {
        // A parsed list with no active version is `Ok(None)` — confirmed
        // no active version (first deploy), NOT an operational failure.
        let json = r#"[{"number": 1, "active": false}]"#;
        assert_eq!(resolve_active_version(json), Ok(None));
    }

    #[test]
    fn resolve_active_version_errors_on_unparseable_payload() {
        // A truncated / non-array body is an operational failure, distinct from
        // "no active version" — the caller must fail closed, not record empty.
        resolve_active_version("not json").expect_err("non-JSON must be an operational error");
        resolve_active_version(r#"{"error":"unauthorized"}"#)
            .expect_err("a non-array body must be an operational error");
    }

    #[test]
    fn resolve_active_version_errors_on_malformed_active_entries() {
        // A garbled ACTIVE entry must fail closed, not read as "no active
        // version" — otherwise a production deploy proceeds with no rollback
        // target. Each of these is malformed and must be an operational error.
        resolve_active_version(r#"[{"active":true}]"#)
            .expect_err("active entry with no `number` must error");
        resolve_active_version(r#"[{"active":true,"number":"7"}]"#)
            .expect_err("active entry with a string `number` must error");
        resolve_active_version(r#"[{"active":"true","number":7}]"#)
            .expect_err("a non-boolean `active` must error");
        // A non-boolean `active` ANYWHERE is schema drift — the whole list is
        // scanned, so it is caught even AFTER a valid active entry (a naive
        // first-match parser would miss this one).
        resolve_active_version(r#"[{"active":"false"},{"active":true,"number":9}]"#)
            .expect_err("a non-boolean `active` before the active entry is schema drift");
        resolve_active_version(r#"[{"active":true,"number":9},{"active":"nope"}]"#)
            .expect_err("a non-boolean `active` AFTER the active entry is still schema drift");
        // More than one active version is ambiguous — refuse rather than pick one.
        resolve_active_version(r#"[{"active":true,"number":9},{"active":true,"number":10}]"#)
            .expect_err("two active versions must error as ambiguous");
        // EVERY element must be a version object with a numeric `number` — a
        // garbled entry must fail closed, not be skipped as "not active".
        resolve_active_version("[]").expect_err("an empty version list is an invalid response");
        resolve_active_version("[null]").expect_err("a null element must error");
        resolve_active_version("[{}]").expect_err("an entry with no `number` must error");
        resolve_active_version(r#"[{"number":"invalid"}]"#)
            .expect_err("a non-numeric `number` must error");
        // An omitted `active` field means "not active" (not an error), as long
        // as the entry is otherwise a well-formed version object.
        assert_eq!(resolve_active_version(r#"[{"number":42}]"#), Ok(None));
        // Sanity: a well-formed list still resolves.
        assert_eq!(
            resolve_active_version(r#"[{"active":false,"number":1},{"active":true,"number":2}]"#),
            Ok(Some(2))
        );
    }

    #[test]
    fn ensure_rollback_from_is_active_blocks_racing_deploys() {
        // The version being rolled back FROM is still active → proceed.
        assert_eq!(ensure_rollback_from_is_active(Some(7), 7, "svc"), Ok(()));
        // A NEWER version is active (a deploy raced the rollback) → refuse, so
        // the newer deploy is not clobbered.
        ensure_rollback_from_is_active(Some(9), 7, "svc")
            .expect_err("a newer active version must block the rollback");
        // No active version at all → refuse.
        ensure_rollback_from_is_active(None, 7, "svc")
            .expect_err("no active version must block the rollback");
    }

    #[test]
    fn active_version_or_require_enforces_require_active() {
        let active = r#"[{"active":true,"number":5}]"#;
        let none = r#"[{"active":false,"number":5}]"#;

        // A resolvable active version is returned regardless of the flag.
        assert_eq!(active_version_or_require(active, false, "svc"), Ok(Some(5)));
        assert_eq!(active_version_or_require(active, true, "svc"), Ok(Some(5)));

        // No active version: tolerated for `active-version` (first deploy), but an
        // ERROR for the production-deploy fallback (`--require-active`), which
        // must never report a deploy with no resolvable version.
        assert_eq!(active_version_or_require(none, false, "svc"), Ok(None));
        active_version_or_require(none, true, "svc")
            .expect_err("require-active with no active version must fail closed");

        // A malformed response is an error either way.
        active_version_or_require("not json", false, "svc").expect_err("malformed must error");
    }

    #[test]
    fn parse_staging_ip_reads_the_singular_staging_ip_field() {
        // The REAL Fastly response shape for
        // `GET /service/<id>/version/<n>/domain?include=staging_ips`:
        // an array of domain objects, each with a SINGULAR `staging_ip`
        // STRING. Body copied from go-fastly's recorded API fixture
        // `fastly/fixtures/domains/list_with_staging_ips.yaml`, matching
        // its `StagingIP *string `mapstructure:"staging_ip"`` field.
        // (`staging_ips` is only the `include=` query value, never a
        // field name — the previous parser looked for it as an array and
        // therefore NEVER found a staging IP.)
        let json = r#"[
            {
                "created_at": "2022-11-04T17:36:56Z",
                "service_id": "kKJb5bOFI47uHeBVluGfX1",
                "name": "integ-test-20221104.go-fastly-1.com",
                "version": 73,
                "comment": "comment",
                "deleted_at": null,
                "staging_ip": "167.82.81.194"
            }
        ]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("167.82.81.194"));
    }

    #[test]
    fn parse_staging_ip_tolerates_a_plural_array_shape() {
        let json = r#"[{"name": "example.com", "staging_ips": ["151.101.2.10"]}]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("151.101.2.10"));
    }

    #[test]
    fn parse_staging_ip_none_when_absent_or_null() {
        assert_eq!(parse_staging_ip(r#"[{"name": "example.com"}]"#), None);
        // `staging_ip` is nullable for services without staging enabled.
        assert_eq!(
            parse_staging_ip(r#"[{"name": "example.com", "staging_ip": null}]"#),
            None
        );
    }

    #[test]
    fn parse_config_store_entries_reads_key_value_pairs() {
        let entries = parse_config_store_entries(
            r#"[{"item_key":"A","item_value":"1"},{"item_key":"B","item_value":"2"}]"#,
        )
        .expect("well-formed listing parses");
        assert_eq!(
            entries,
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn parse_config_store_entries_errors_never_leak_the_value() {
        // The listing carries every entry's item_value (possibly a production secret),
        // and CLI status lines are logged verbatim into retained CI logs — so no error
        // path may echo the payload. A sentinel secret must NEVER appear in any error.
        const SECRET: &str = "s3cr3t-sentinel-value";

        // 1. Malformed JSON.
        let malformed_json = parse_config_store_entries(&format!("not json {SECRET}"))
            .expect_err("malformed JSON must error");
        assert!(
            !malformed_json.contains(SECRET),
            "malformed-JSON error leaked the value: {malformed_json}"
        );

        // 2. Schema drift: valid JSON that is neither a bare array nor an `items`
        //    envelope (here an object whose VALUE is the secret).
        let drift = parse_config_store_entries(&format!(r#"{{"unexpected":"{SECRET}"}}"#))
            .expect_err("schema drift must error");
        assert!(
            !drift.contains(SECRET),
            "schema-drift error leaked the value: {drift}"
        );

        // 3. Malformed entry: a valid array where an entry lacks item_key/item_value,
        //    while a SIBLING entry carries the secret in its value.
        let bad_entry = parse_config_store_entries(&format!(
            r#"[{{"item_key":"ok","item_value":"{SECRET}"}},{{"item_key":"bad"}}]"#
        ))
        .expect_err("a malformed entry must error");
        assert!(
            !bad_entry.contains(SECRET),
            "malformed-entry error leaked the value: {bad_entry}"
        );
    }

    #[test]
    fn build_curl_probe_args_production_has_no_connect_to() {
        let args = build_curl_probe_args("example.com", "/", None, 10);
        assert!(!args.iter().any(|arg| arg == "--connect-to"));
        assert!(args.contains(&"https://example.com/".to_owned()));
        assert!(args.contains(&"--max-time".to_owned()));
        assert!(args.contains(&"10".to_owned()));
        // Globbing must be off so bracket/brace characters in a path are literal.
        assert!(args.contains(&"--globoff".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_path_with_glob_chars_is_literal() {
        // A path with `[` `]` would be a curl glob without --globoff; here it must
        // appear verbatim in the single URL argument, with globbing disabled.
        let args = build_curl_probe_args("example.com", "/health?ids[0]=1", None, 10);
        assert!(args.contains(&"--globoff".to_owned()));
        assert!(args.contains(&"https://example.com/health?ids[0]=1".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_staging_reroutes_to_ip() {
        let args = build_curl_probe_args("staging.example.com", "/", Some("151.101.2.10"), 15);
        let idx = args
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        assert_eq!(args[idx + 1], "::151.101.2.10:443");
        assert!(args.contains(&"https://staging.example.com/".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_leads_with_q_to_ignore_curlrc() {
        // `-q` must be the FIRST argument or curl merges `~/.curlrc` before it.
        let args = build_curl_probe_args("example.com", "/", None, 10);
        assert_eq!(args.first().map(String::as_str), Some("-q"));
    }

    #[test]
    fn build_curl_probe_args_brackets_ipv6_connect_to() {
        let args = build_curl_probe_args("staging.example.com", "/", Some("2001:db8::1"), 10);
        let idx = args
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        // An IPv6 literal must be bracketed so curl does not misparse the colons.
        assert_eq!(args[idx + 1], "::[2001:db8::1]:443");
    }

    #[test]
    fn build_curl_probe_args_honors_path_on_production_and_staging() {
        // Production: the path is appended to the domain URL.
        let prod = build_curl_probe_args("example.com", "/health", None, 10);
        assert!(prod.contains(&"https://example.com/health".to_owned()));
        // Staging: same URL (with the path), rerouted to the staging IP.
        let staging =
            build_curl_probe_args("staging.example.com", "/health", Some("151.101.2.10"), 10);
        assert!(staging.contains(&"https://staging.example.com/health".to_owned()));
        let idx = staging
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        assert_eq!(staging[idx + 1], "::151.101.2.10:443");
    }

    #[test]
    fn validate_probe_path_requires_leading_slash_and_no_whitespace() {
        validate_probe_path("/").expect("root");
        validate_probe_path("/health").expect("simple path");
        validate_probe_path("/api/v1/status?ready=1").expect("path with query");
        validate_probe_path("health").expect_err("no leading slash");
        validate_probe_path("").expect_err("empty");
        validate_probe_path("/ with space").expect_err("whitespace");
        validate_probe_path("/inject\nHost: evil").expect_err("newline injection");
    }

    #[test]
    fn healthcheck_rejects_zero_timeout() {
        // A zero timeout becomes curl `--max-time 0` (no limit); reject it before
        // any probe. The other required args are valid so we reach the check.
        let args = [
            "--adapter",
            "fastly",
            "--domain",
            "example.com",
            "--service-id",
            "svc123",
            "--version",
            "1",
            "--timeout",
            "0",
        ]
        .map(str::to_owned)
        .to_vec();
        let err = healthcheck(&args).expect_err("zero timeout must be rejected");
        assert!(err.contains("timeout"), "unexpected error: {err}");
    }

    #[test]
    fn probe_with_retries_returns_first_healthy() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                Ok(200)
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 1_i32, "should stop after first healthy probe");
        assert_eq!(between, 0_i32, "no delay before the first attempt");
    }

    #[test]
    fn probe_with_retries_succeeds_after_unhealthy_attempts() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                if calls < 3_i32 { Ok(503) } else { Ok(200) }
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 3_i32);
        assert_eq!(
            between, 2_i32,
            "delay runs between each of the first 3 attempts"
        );
    }

    #[test]
    fn probe_with_retries_exhausts_and_reports_last_code() {
        let mut between: i32 = 0;
        let result = probe_with_retries(3, || Ok(500), || between += 1_i32);
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(
            between, 2_i32,
            "delay runs between attempts, not after the last"
        );
    }

    #[test]
    fn probe_with_retries_reports_transport_error() {
        let result: Result<u16, (Option<u16>, String)> =
            probe_with_retries(1, || Err("connection refused".to_owned()), || {});
        assert_eq!(result, Err((None, "connection refused".to_owned())));
    }

    #[test]
    fn probe_with_retries_treats_zero_retry_as_one_attempt() {
        let mut calls: i32 = 0;
        let result = probe_with_retries(
            0,
            || {
                calls += 1_i32;
                Ok(500)
            },
            || {},
        );
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(calls, 1_i32);
    }

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
        // The failed entry's outcome is UNKNOWN and recovery is a full idempotent
        // re-run, not a hand-resume from a claimed boundary.
        assert!(
            err.contains("UNKNOWN") && err.contains("outcome unknown"),
            "failed outcome must be stated unknown: {err}"
        );
        assert!(
            err.contains("re-run the SAME") && err.contains("idempotent"),
            "recovery must be a full idempotent re-run: {err}"
        );
        assert!(
            !err.contains("safe to skip on retry"),
            "must not claim committed entries can be skipped from a known boundary: {err}"
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
            err.contains("Not attempted: []"),
            "zero not-attempted when the last entry fails: {err}"
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

    /// The three provisioning parsers must NOT echo a malformed fastly.toml's
    /// source text (which can contain a stored secret) on a parse failure.
    #[test]
    fn provisioning_parsers_redact_malformed_toml() {
        const SENTINEL: &str = "SUPER_SECRET_IN_A_BROKEN_LINE";
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Malformed TOML whose offending line carries a secret.
        fs::write(&path, format!("service_id = \"{SENTINEL}\" = broken\n")).expect("write");

        let errs = [
            read_fastly_service_id(&path).expect_err("malformed toml must error"),
            setup_block_present(&path, "kv", TEST_KV_ID).expect_err("malformed toml must error"),
            append_fastly_setup(&path, "kv", TEST_KV_ID).expect_err("malformed toml must error"),
        ];
        for err in &errs {
            assert!(
                !err.contains(SENTINEL),
                "a parse error must not echo the stored value: {err}"
            );
            assert!(
                err.contains("redacted"),
                "error should say it redacted: {err}"
            );
        }
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
        // `setup_block_present` only
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
        // No `[local_server.*]` write — that empty stanza
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

    /// An existing `format = "json"` / `"file"` store points at an EXTERNAL file.
    /// Converting it here would either produce a manifest the local server rejects
    /// (a stray `file` key) or silently discard the sibling entries that file
    /// holds. The push must REFUSE and leave the manifest untouched, not convert.
    #[test]
    fn write_fastly_local_config_store_refuses_incompatible_format() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let before = format!(
            "name = \"demo\"\n\n[local_server.config_stores.{TEST_CONFIG_ID}]\nformat = \"json\"\nfile = \"cfg.json\"\n",
        );
        fs::write(&path, &before).expect("write");
        let err = write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hello".to_owned())],
            &[],
        )
        .expect_err("a non-inline store must be refused, not converted");
        assert!(
            err.contains("refusing to push") && err.contains("inline-toml"),
            "must refuse and point at migration: {err}"
        );
        // The manifest is left exactly as it was.
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, before, "the manifest must be untouched on refusal");
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
        // 1 KV + 1 config + 1 secret + runtime-env = 4 status lines. The staging
        // twin is created and populated by a staged deploy, NOT by provision, so
        // it does not appear here.
        assert_eq!(out.len(), 4, "dry-run rows: {out:?}");
        assert!(out[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(out[1].contains("would run `fastly config-store create --name=app_config`"));
        assert!(out[2].contains("would run `fastly secret-store create --name=default`"));
        assert!(
            out[3].contains("would run `fastly config-store create --name=edgezero_runtime_env`"),
            "runtime-env store row: {out:?}",
        );
        assert!(
            !out.iter()
                .any(|row| row.contains("edgezero_runtime_env_staging")),
            "provision must NOT create the staging twin (a staged deploy owns it): {out:?}",
        );
        // Manifest untouched.
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated fastly.toml");
    }

    #[test]
    fn provision_dry_run_reports_non_default_store_name_mapping() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let secret_ids = vec![ResolvedStoreId::new("default", "production_secrets")];
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &secret_ids,
        };

        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, true)
            .expect("dry-run succeeds");

        assert!(out.iter().any(|line| {
            line.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=production_secrets")
        }));
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
    fn find_config_store_id_fails_closed_on_a_malformed_row() {
        // A row that lacks a non-empty `name`/`id` could BE the requested store
        // (its unreadable name might have matched). Treating the listing as a
        // definite NotFound would authorise an overwrite of a store that exists,
        // so a malformed row must be SchemaDrift (a hard error), not NotFound --
        // even when another row is well-formed.
        let stdout = format!(
            r#"[{{"name": "", "id": "abc"}}, {{"name": "{TEST_CONFIG_ID}", "id": "store-1"}}]"#
        );
        let drift = find_config_store_id(&stdout, "some-other-store");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "an empty-name row must fail closed, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_rejects_duplicate_names() {
        // A duplicate name means we are not reading one consistent view of the
        // store, so resolving an id off it is ambiguous -> fail closed.
        let stdout = format!(
            r#"[{{"name": "{TEST_CONFIG_ID}", "id": "a"}}, {{"name": "{TEST_CONFIG_ID}", "id": "b"}}]"#
        );
        let drift = find_config_store_id(&stdout, TEST_CONFIG_ID);
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a duplicate name must fail closed, got {drift:?}"
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

    #[test]
    fn find_config_store_id_flags_schema_drift_when_any_entry_is_malformed() {
        // A well-formed, non-matching entry must NOT mask a malformed one: the
        // malformed entry could be the store we're looking for (hidden behind a
        // missing name/id). Deciding NotFound here would fail OPEN — staging
        // would then mirror no production overrides. Any malformed entry is drift.
        let stdout = r#"[
            {"id": "abc123", "name": "some_other_store"},
            {"id": "def456"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a malformed entry alongside a well-formed one must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_scans_past_a_match() {
        // The full list is scanned: a malformed entry AFTER the match must still
        // be caught (no short-circuit on the first Found).
        let stdout = r#"[
            {"id": "abc123", "name": "edgezero_runtime_env"},
            {"name": "broken"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a malformed entry after the match must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_flags_duplicate_names_as_ambiguous() {
        let stdout = r#"[
            {"id": "abc123", "name": "edgezero_runtime_env"},
            {"id": "def456", "name": "edgezero_runtime_env"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "two stores with the same name must be ambiguous drift, got {drift:?}"
        );
    }

    #[test]
    fn parse_rfc3339_secs_rounds_a_fraction_up() {
        let whole = parse_rfc3339_secs("2026-01-01T00:00:42Z").expect("whole");
        // A fractional second rounds UP to the next whole second, so the computed
        // age stays conservative and a key never ages into deletion early.
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T00:00:42.998Z"),
            Some(whole + 1),
            "a fractional creation time must round UP, not floor"
        );
        // Even a tiny fraction rounds up.
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T00:00:42.000001Z"),
            Some(whole + 1)
        );
        // A whole-second stamp is unchanged.
        assert_eq!(parse_rfc3339_secs("2026-01-01T00:00:42.000Z"), Some(whole));
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
        fake_fastly_returning_with_keys(stdout_body, stderr_body, exit_code, &[])
    }

    /// As [`fake_fastly_returning`], but also serves `config-store-entry list`
    /// with a bare array of the `entry_list_keys` as `item_key` entries. A
    /// describe FAILURE is confirmed against this listing: keys present here read
    /// as a present-but-unreadable hard error, keys absent read as `MissingKey`.
    #[cfg(unix)]
    fn fake_fastly_returning_with_keys(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
        entry_list_keys: &[&str],
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        let stdout_file = dir.path().join("stdout_payload.txt");
        let stderr_file = dir.path().join("stderr_payload.txt");
        let list_file = dir.path().join("list_payload.txt");
        let entry_list_file = dir.path().join("entry_list_payload.txt");
        // Store-list JSON: bare array with one entry matching TEST_CONFIG_ID.
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        let entry_list_json = {
            let items: Vec<String> = entry_list_keys
                .iter()
                .map(|key| format!(r#"{{"item_key":{}}}"#, serde_json::to_string(key).unwrap()))
                .collect();
            format!("[{}]", items.join(","))
        };
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        fs::write(&list_file, list_json).expect("write list payload");
        fs::write(&entry_list_file, entry_list_json).expect("write entry list payload");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\ncat '{}' >&2\nexit {exit_code}\n",
            list_file.display(),
            entry_list_file.display(),
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
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  echo '[]'\n  exit 0\nfi\ncat '{}'\nexit 0\n",
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
    fn read_remote_returns_missing_key_when_confirmed_absent() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // describe exits non-zero, and the complete store listing (empty here)
        // CONFIRMS the key is absent → MissingKey (not decided by the 404 alone).
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
    fn read_remote_fails_closed_when_the_list_call_itself_errors() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The list call EXITS NON-ZERO with "not found"-shaped stderr. That is an
        // OPERATIONAL failure (auth/network/server), not proof the store is absent
        // -- the absence signal is a SUCCESSFUL list that omits the store. So this
        // must fail closed (a hard error the operator retries), NEVER MissingStore:
        // reading it as absence could authorise an overwrite of a store we never
        // actually queried.
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
        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        assert!(
            result.is_err(),
            "a failed list call must fail closed, not read as MissingStore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_when_the_store_is_genuinely_absent() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The list call SUCCEEDS and returns a valid, empty store array. The store
        // is genuinely absent -> `no fastly config-store matches` -> MissingStore.
        let fake_dir = tempdir().expect("tempdir");
        let script_path = fake_dir.path().join("fastly");
        fs::write(&script_path, "#!/bin/sh\necho '[]'\nexit 0\n").expect("write script");
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
            .expect("a successful list that omits the store maps to MissingStore");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "store absent from a successful list => MissingStore"
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
        // The `config-store-entry list` response: a bare array of the keys present
        // in `key_responses`. Absence confirmation lists the store and checks
        // membership, so a key omitted here reads as CONFIRMED absent. Only
        // `item_key` is needed (the keys-only listing is value-tolerant).
        let entry_list_file = fake_dir.path().join("entry_list.json");
        let entries_json = {
            let items: Vec<String> = key_responses
                .iter()
                .map(|(key, _)| {
                    format!(r#"{{"item_key":{}}}"#, serde_json::to_string(key).unwrap())
                })
                .collect();
            format!("[{}]", items.join(","))
        };
        fs::write(&entry_list_file, entries_json).expect("write entry list");
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
        // `config-store` (store list) and `config-store-entry list` are served
        // from their files; a `describe` for an unknown key exits 1 "not found",
        // which the caller then CONFIRMS against the entry list.
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  cat '{}'\n  exit 0\nfi\n{dispatch_lines}echo 'Error: item not found' >&2\nexit 1\n",
            list_file.display(),
            entry_list_file.display()
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
if [ "$sub" = "delete" ]; then printf 'delete %s\n' "$key" >> '{{{oplog}}}'; printf 'delete-argv %s\n' "$*" >> '{{{oplog}}}'; if [ "$key" = "{{{fail}}}" ]; then echo 'Error: 404 item not found' >&2; exit 1; fi; exit 0; fi
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

    /// Schema drift must never echo the config payload — including OBJECT KEYS,
    /// which are provider/stored data. App config can hold credentials; CLI
    /// status lines are logged verbatim and CI logs are retained/shared. Only a
    /// size + field COUNT may be reported.
    #[cfg(unix)]
    #[test]
    fn read_config_entry_schema_drift_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_abc123";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The sentinel is an OBJECT KEY (not a value): the earlier redactor joined
        // keys into the diagnostic, so this is what pins the key-disclosure fix.
        let drift = format!(r#"{{"{SENTINEL}":"x"}}"#);
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
            "error must not leak an object KEY from the config payload: {err}"
        );
        assert!(
            err.contains("bytes") && err.contains("field(s)"),
            "error should carry a redacted size + field COUNT: {err}"
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
        // A hard failure that echoes the value. The key IS present in the store
        // listing, so absence confirmation fails and the read surfaces the
        // (redacted) describe stderr on the hard-error path.
        let stderr = format!("Error: internal failure processing value {SENTINEL}");
        let fake = fake_fastly_returning_with_keys("", &stderr, 1, &["cfg"]);
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

    /// The WRITE path leaks too: a failing `config-store-entry update --upsert`
    /// whose stderr quotes the value being written must be redacted.
    #[cfg(unix)]
    #[test]
    fn upsert_stderr_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_upsert1";
        let _lock = path_mutation_guard().lock().expect("guard");
        // A fake `fastly` that fails every call, echoing the value in stderr.
        let stderr = format!("Error: rejected value {SENTINEL}");
        let fake = fake_fastly_returning("", &stderr, 1);
        let _path = PathPrepend::new(fake.path());

        let err = create_config_store_entry("store-abc", "cfg", SENTINEL)
            .expect_err("a failing upsert must error");
        assert!(
            !err.contains(SENTINEL),
            "upsert stderr must be redacted, not echoed: {err}"
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

    /// Pushing two blobs under different root keys
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
    fn read_config_entry_reports_corrupt_on_a_confirmed_absent_chunk() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Only provide the root pointer; omit chunk responses so the chunk fetch
        // gets a CLEAN not-found (`Error: item not found`, no operational marker).
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
        // The chunk describe fails, and the complete store listing (which holds
        // only the root pointer) CONFIRMS the chunk is absent. The blob spec makes
        // persistent chunk loss REPAIRABLE by re-pushing, so the read reports
        // `Corrupt` (a push overwrites to repair), NOT a hard error -- otherwise
        // `config push` could never fix it. Absence is confirmed by the listing,
        // never by the describe 404 alone, so a proxy/auth failure (where the
        // listing also fails, or shows the chunk present) stays a hard error.
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a confirmed-absent chunk must be repairable Corrupt, not a hard error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reports_corrupt_on_chunk_hash_mismatch() {
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
        // A chunk-hash mismatch at an EXISTING entry is corrupt stored state the
        // push repairs by overwriting, so the CLI read reports `Corrupt`. (The
        // RUNTIME path keeps a hash mismatch as Internal — see config_store.rs.)
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a chunk-hash mismatch at an existing entry must be Corrupt (repairable), not an error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reports_corrupt_for_malformed_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Root value ANNOUNCES our chunk-pointer kind but is malformed. The
        // `describe` SUCCEEDS (the entry exists), so a resolve failure is CORRUPT
        // stored state, not an IO error: the read must report `Corrupt` so a push
        // can overwrite it (in-band repair), NOT hard-error and block recovery.
        let bad_json = r#"{"edgezero_kind":"fastly_config_chunks","some_field":"x"}"#;
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
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a malformed pointer at an EXISTING entry must be Corrupt (repairable), not an error"
        );
    }

    /// The read taxonomy distinguishes repairable corruption from cases a push
    /// must NOT overwrite: an infrastructure fetch failure (incomplete read) and
    /// an unknown/future format both stay hard errors, while a malformed direct
    /// value, a SHA mismatch, and a resolve error are repairable `Corrupt`.
    #[test]
    fn classify_resolved_read_separates_corrupt_from_infra_and_unknown() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({ "k": "v" }),
            "2026-01-01T00:00:00Z".to_owned(),
        ))
        .expect("envelope");

        // A valid envelope resolves to Present.
        assert!(matches!(
            classify_resolved_read(Ok(envelope.clone()), &envelope, false),
            Ok(ReadConfigEntry::Present(_))
        ));

        // A direct value with a wrong `sha256` is NOT a valid envelope -> Corrupt.
        let mut tampered_value: serde_json::Value = serde_json::from_str(&envelope).expect("parse");
        tampered_value["sha256"] = json!("0".repeat(64));
        let tampered = tampered_value.to_string();
        assert!(matches!(
            classify_resolved_read(Ok(tampered.clone()), &tampered, false),
            Ok(ReadConfigEntry::Corrupt(_))
        ));

        // Invalid JSON / a plain non-envelope value -> Corrupt (not Present).
        assert!(matches!(
            classify_resolved_read(Ok("not an envelope".to_owned()), "not an envelope", false),
            Ok(ReadConfigEntry::Corrupt(_))
        ));

        // A resolve error caused by an INFRASTRUCTURE fetch failure stays a HARD
        // error: the read was incomplete, so a push must not overwrite.
        let infra = classify_resolved_read(
            Err(ResolveFailure::Corrupt("boom".to_owned())),
            "{\"edgezero_kind\":\"fastly_config_chunks\"}",
            true,
        );
        assert!(
            infra
                .as_ref()
                .is_err_and(|err| err.contains("not fully read")),
            "an infra fetch failure must be a hard error, not Corrupt"
        );

        // A value announcing an UNKNOWN/future kind is a HARD error (upgrade CLI),
        // never offered for overwrite.
        let unknown = classify_resolved_read(
            Err(ResolveFailure::FutureFormat("x".to_owned())),
            r#"{"edgezero_kind":"fastly_config_chunks_v2"}"#,
            false,
        );
        assert!(
            unknown
                .as_ref()
                .is_err_and(|err| err.contains("does not recognise")),
            "an unknown/future kind must be a hard error"
        );

        // A NEWER INNER envelope (a valid v1 pointer wrapping a v2 envelope) is
        // only knowable AFTER reassembly: the raw value is a healthy v1 pointer,
        // so the typed `FutureFormat` failure is the ONLY signal. It must be a
        // hard error, never repairable Corrupt -- a downgrade push must not
        // overwrite it.
        let inner_future = classify_resolved_read(
            Err(ResolveFailure::FutureFormat(
                "newer inner envelope".to_owned(),
            )),
            r#"{"edgezero_kind":"fastly_config_chunks","version":1,"chunks":[]}"#,
            false,
        );
        assert!(
            inner_future
                .as_ref()
                .is_err_and(|err| err.contains("UPGRADE")),
            "a future INNER envelope (typed FutureFormat) must be a hard error, not Corrupt"
        );

        // An ordinary resolve error (bad/missing chunk) is repairable Corrupt.
        assert!(matches!(
            classify_resolved_read(
                Err(ResolveFailure::Corrupt("bad chunk".to_owned())),
                r#"{"edgezero_kind":"fastly_config_chunks","chunks":[]}"#,
                false
            ),
            Ok(ReadConfigEntry::Corrupt(_))
        ));

        // A future ENVELOPE version (passed through as Ok) is a hard error, NOT
        // the repairable Corrupt -- an older CLI must not overwrite it.
        let mut v2_value: serde_json::Value = serde_json::from_str(&envelope).expect("parse");
        v2_value["version"] = json!(2_u32);
        let v2_env = v2_value.to_string();
        assert!(
            classify_resolved_read(Ok(v2_env.clone()), &v2_env, false)
                .as_ref()
                .is_err_and(|err| err.contains("UPGRADE")),
            "a v2 direct envelope must be a hard error, not Corrupt"
        );

        // A future POINTER version (resolve fails on the version check) is a hard
        // error too -- the pointer kind is ours, but the version is newer.
        let v2_ptr = r#"{"edgezero_kind":"fastly_config_chunks","version":2,"chunks":[]}"#;
        assert!(
            classify_resolved_read(
                Err(ResolveFailure::FutureFormat(
                    "unsupported version".to_owned()
                )),
                v2_ptr,
                false
            )
            .as_ref()
            .is_err_and(|err| err.contains("UPGRADE")),
            "a v2 pointer must be a hard error, not Corrupt"
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
    /// `config push` is how an operator recovers, so the read reports `Corrupt`
    /// ("cannot diff; will overwrite") and lets the write proceed.
    #[test]
    fn read_config_entry_local_degrades_corrupt_prior_to_corrupt() {
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
            matches!(result, ReadConfigEntry::Corrupt(_)),
            "a corrupt prior value must degrade to Corrupt so the push can overwrite it"
        );
    }

    /// A `contents` that is not a table (a scalar or array) is malformed store
    /// state. It must degrade to `Unsupported`, not fall through to `MissingKey`
    /// (which would render an inaccurate "all values added" diff).
    #[test]
    fn read_config_entry_local_non_table_contents_is_unsupported() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(
            &fastly_toml,
            format!("[local_server.config_stores.{TEST_CONFIG_ID}]\ncontents = 42\n"),
        )
        .expect("seed");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("a non-table contents must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "a non-table `contents` must degrade to Unsupported, not MissingKey"
        );
    }

    /// A malformed PARENT table (`local_server` etc. as a scalar) must degrade to
    /// Unsupported, not collapse to `MissingStore`'s "all values added" diff.
    #[test]
    fn read_config_entry_local_non_table_parent_is_unsupported() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // `local_server` is a scalar, not a table.
        fs::write(&fastly_toml, "local_server = 42\n").expect("seed");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("a non-table parent must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "a non-table parent must degrade to Unsupported, not MissingStore"
        );
    }

    /// Spec 12.3 + 9.3: a second oversized push must converge the
    /// runtime on the NEW envelope — chunk keys are content-addressed
    /// by the full-envelope SHA, so push B writes a new chunk-set and
    /// installs a new root pointer.
    ///
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

    // ── staged deploy: end-to-end argv contract (fake `fastly`) ───────

    /// Fake `fastly` on `$PATH` that appends every invocation's argv (one
    /// space-joined line per call) to a record file, and echoes
    /// `update_stdout` for `fastly compute update`. Returns the temp dir
    /// (which must outlive the test) and the record path.
    #[cfg(unix)]
    fn fake_fastly_recorder(update_stdout: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let script_path = dir.path().join("fastly");
        // Answers every call `deploy_staged` makes. The staging relink needs the
        // selector store to resolve and the inherited link to be listed; without
        // these the staged path fails closed (which is correct, but not what
        // these tests are exercising).
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' '{update_stdout}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               case \"$*\" in\n    \
                 *--store-id=ENVSEL1*) printf '%s\\n' '[{{\"item_key\":\"EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL\",\"item_value\":\"debug\"}}]' ;;\n    \
                 *) printf '%s\\n' '[]' ;;\n  \
               esac\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
        );
        fs::write(&script_path, script).expect("write fake fastly");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        (dir, record)
    }

    /// Run `deploy_staged` against a fake `fastly`, returning the result
    /// and the recorded argv lines.
    #[cfg(unix)]
    fn run_deploy_staged_with_fake(
        update_stdout: &str,
        extra: &[&str],
    ) -> (Result<(), String>, Vec<String>) {
        let _lock = path_mutation_guard().lock().expect("guard");
        let (fake, record) = fake_fastly_recorder(update_stdout);
        let _path = PathPrepend::new(fake.path());
        let app = tempdir().expect("app dir");
        let manifest = app.path().join("fastly.toml");
        fs::write(&manifest, "name = \"app\"\n").expect("write fastly.toml");

        // RAII: set the token for the call, restore it on drop. Uses the shared
        // guard (edition-2024 wraps the env mutation's `unsafe` and holds the
        // lock we already took above).
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");
        let mut args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            manifest.display().to_string(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let result = deploy_staged(&args);

        let recorded = fs::read_to_string(&record).unwrap_or_default();
        let lines = recorded.lines().map(str::to_owned).collect();
        (result, lines)
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_routes_comment_to_service_version_update() {
        // `--comment` is allowlisted for `deploy-args` and recommended by the
        // adoption guide, but `fastly compute update` has no such flag. It
        // must NOT be forwarded there (that would fail the deploy) and must
        // instead land on the version via `service-version update`.
        for comment_args in [vec!["--comment", "ci run 12"], vec!["--comment=ci run 12"]] {
            let (result, argv) = run_deploy_staged_with_fake(
                "SUCCESS: Updated package (service SVC1, version 7)",
                &comment_args,
            );
            result.expect("staged deploy with --comment must succeed");

            let update = argv
                .iter()
                .find(|line| line.starts_with("compute update"))
                .expect("compute update was invoked");
            assert!(
                !update.contains("--comment"),
                "--comment must not be forwarded to `compute update`: {update}"
            );
            assert!(
                update.contains("--non-interactive"),
                "compute update must be non-interactive: {update}"
            );

            let comment_call = argv
                .iter()
                .find(|line| line.starts_with("service-version update"))
                .expect("`service-version update` must apply the version comment");
            assert_eq!(
                comment_call,
                "service-version update --service-id=SVC1 --version=7 --comment ci run 12"
            );

            // The comment lands on the version BEFORE it is staged (while it
            // is still an editable draft).
            let comment_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version update"))
                .expect("comment call");
            let stage_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version stage"))
                .expect("stage call");
            assert!(comment_idx < stage_idx, "comment must precede staging");
            assert_eq!(
                argv[stage_idx],
                "service-version stage --service-id=SVC1 --version=7"
            );
        }
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
        // The SIBLING chunks must survive too: protecting the holder drops the
        // generation to an incomplete group, which is left unprovable — so
        // nothing in this generation is deleted, not just the holder.
        for (key, _) in &entries[1..entries.len().saturating_sub(1)] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a sibling of a protected root must also survive (the group is left \
                 unprovable): `{key}`; log:\n{log}"
            );
        }
    }

    /// A self-scoped pointer at a chunk-shaped holder key (its chunks nest the
    /// infix twice) must NOT abort store-wide GC: the doubly-nested chunks are
    /// recognised as chunks (via the LAST infix), so the holder classifies as a
    /// root, its references are counted live, and other roots still reclaim.
    #[cfg(unix)]
    #[test]
    fn gc_tolerates_a_self_scoped_pointer_at_a_chunk_shaped_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A pointer parked at a chunk-shaped key, with chunks scoped to itself.
        let holder_key = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "e".repeat(64));
        let nested = gen_envelope("nested");
        let nested_entries = prepare_fastly_config_entries(&holder_key, &nested).expect("expand");
        let (_, holder_pointer) = nested_entries.last().expect("pointer").clone();

        // A normal live root, and a normal orphan generation that SHOULD reclaim.
        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));
        listing.push((holder_key.clone(), stamp.clone(), holder_pointer));
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // The run must SUCCEED (not abort) and still reclaim the ordinary orphan.
        run_gc(dir.path(), 86_400, false).expect("store-wide GC must not abort");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "an ordinary orphan must still be reclaimed despite the self-scoped pointer: `{key}`"
            );
        }
        assert!(
            !oplog_has(&oplog, &format!("delete {holder_key}")),
            "the chunk-shaped holder root must never be deleted"
        );
    }

    /// A nested ORPHAN generation (chunks scoped to a chunk-shaped root, with NO
    /// live pointer referencing them) must be grouped and reclaimed, not silently
    /// dropped. Age and grouping split on the LAST infix, so the nested chunks are
    /// attributed to their real (nested) root.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_a_nested_orphan_generation() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A chunk-shaped root, and a full generation of chunks SCOPED to it — but
        // no pointer references them, so they are orphaned.
        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "f".repeat(64));
        let nested = gen_envelope("nested-orphan");
        let nested_entries = prepare_fastly_config_entries(&nested_root, &nested).expect("expand");
        let nested_chunks: Vec<String> = nested_entries[..nested_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &nested_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "a nested orphan generation must be reclaimed, not silently dropped: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// FAIL CLOSED: a MALFORMED pointer sitting at a chunk-shaped root that HAS a
    /// nested generation beneath it must abort GC, not let that nested generation
    /// be reclaimed. The truncated pointer cannot announce its discriminator, so
    /// it looks like a chunk fragment -- but its nested chunks are proven
    /// independently and would be deleted while their (unreadable) root can no
    /// longer name them. That is exactly the truncated-pointer data loss the
    /// spec forbids, so the whole run must refuse.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_malformed_pointer_at_a_chunk_shaped_root_with_nested_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "f".repeat(64));
        let nested = gen_envelope("nested");
        let nested_entries = prepare_fastly_config_entries(&nested_root, &nested).expect("expand");
        let nested_chunks: Vec<String> = nested_entries[..nested_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // A truncated pointer at the chunk-shaped nested root: it WAS a pointer,
        // now cut off, so it cannot announce its `edgezero_kind`.
        listing.push((
            nested_root.clone(),
            stamp.clone(),
            r#"{"chunks":[{"key":"#.to_owned(),
        ));
        // ...its aged, independently-provable nested generation.
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false)
            .expect_err("an unreadable nested root must fail closed, not be reclaimed");
        assert!(
            err.contains("refusing to reclaim"),
            "must fail closed, not delete: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &nested_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a nested generation under an unreadable root must NOT be deleted: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// Age attribution works per NESTED root: a nested orphan generation whose
    /// nested root's live config went live RECENTLY must be RETAINED (POPs may
    /// still serve the superseded generation), even though the orphan's own
    /// chunks are old. This pins that `root_live_since` splits on the last infix.
    #[cfg(unix)]
    #[test]
    fn gc_retains_a_nested_orphan_under_a_recently_changed_nested_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "a".repeat(64));

        // The nested root's CURRENT (live) generation, created 30s ago.
        let live_nested = gen_envelope("live-nested");
        let live_entries = prepare_fastly_config_entries(&nested_root, &live_nested).expect("exp");
        let (_, live_pointer) = live_entries.last().expect("pointer").clone();

        // An OLD orphan generation under the SAME nested root (a week old).
        let old_nested = gen_envelope("old-nested-orphan");
        let old_entries = prepare_fastly_config_entries(&nested_root, &old_nested).expect("exp");
        let old_chunks: Vec<String> = old_entries[..old_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // The nested root holds its live pointer; its live chunks are 30s old.
        listing.push((nested_root.clone(), stamp_secs_ago(30), live_pointer));
        for (key, value) in &live_entries[..live_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp_secs_ago(30), value.clone()));
        }
        // The old orphan chunks are a week old.
        for (key, value) in &old_entries[..old_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp_secs_ago(604_800), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // A generous 1-day window: the orphan's OWN chunks are older, but the
        // nested root's live config is only 30s old, so its orphan is retained.
        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &old_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a nested orphan under a recently-changed nested root must be retained: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// A generation is aged by its YOUNGEST member, so a generation with one
    /// recent chunk is retained whole even if its other chunks are ancient.
    #[cfg(unix)]
    #[test]
    fn gc_ages_a_generation_by_its_youngest_member() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // `app_config` live is direct, so there is no live-config age signal —
        // aging falls to the generation's own chunks.
        let live_direct = gen_envelope_padded("live-direct", 100);
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");

        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            live_direct,
        )];
        // The doomed generation: chunk 0 written 30s ago (YOUNG), the rest a week
        // ago. Its youngest-member age (30s) is under the 1-day window.
        let dead_parts = chunked_parts(TEST_CONFIG_ID, &dead).0;
        for (idx, (key, value)) in dead_parts.iter().enumerate() {
            let age = if idx == 0 { 30 } else { 604_800 };
            listing.push((key.clone(), stamp_secs_ago(age), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &dead_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a generation with a recent member must be retained WHOLE (aged by its youngest): \
                 `{key}`; log:\n{log}"
            );
        }
    }

    /// A delete failure in one generation must not stop an INDEPENDENT
    /// generation's deletes.
    #[cfg(unix)]
    #[test]
    fn gc_failure_in_one_generation_does_not_stop_another() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead_a = gen_envelope("dead-a");
        let dead_b = gen_envelope("dead-b");
        let a_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead_a);
        let b_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead_b);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead_a, 604_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead_b, 604_800));

        // Generation A's first delete fails.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&a_chunks[0]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        // Generation B must still have been reclaimed despite A's failure.
        for key in &b_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "an independent generation must still be reclaimed after another one fails: \
                 `{key}`; err: {err}; log:\n{log}"
            );
        }
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

    /// GC and the RUNTIME must agree on what a readable pointer is. A pointer
    /// whose chunks reassemble to the correct bytes along boundaries this writer
    /// would never choose is REJECTED by the runtime resolver, so GC must not
    /// silently report it as a healthy root: the guest cannot read it, and its
    /// generation can never satisfy `prove_generation`, so it is permanently
    /// unreclaimable. GC still keeps it (fail-closed) but must SAY so.
    #[cfg(unix)]
    #[test]
    fn gc_warns_that_a_non_writer_split_root_is_not_runtime_readable() {
        use crate::chunked_config::CHUNK_PAYLOAD_TARGET;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Just over the entry limit => a full chunk plus a short remainder with
        // room to absorb the shifted bytes.
        let envelope = gen_envelope("shifted");
        let sha = sha256_hex(envelope.as_bytes());
        // Re-split 2 bytes early: still within every metadata bound the pointer
        // validator checks, but NOT where this writer splits.
        let cut = CHUNK_PAYLOAD_TARGET.saturating_sub(2);
        let head = envelope.get(..cut).expect("ascii boundary");
        let tail = envelope.get(cut..).expect("ascii boundary");
        let key0 = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{sha}.0");
        let key1 = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{sha}.1");
        let pointer_json = serde_json::json!({
            "chunks": [
                {"key": key0, "len": head.len(), "sha256": sha256_hex(head.as_bytes())},
                {"key": key1, "len": tail.len(), "sha256": sha256_hex(tail.as_bytes())},
            ],
            "data_sha256": "",
            "edgezero_kind": "fastly_config_chunks",
            "envelope_len": envelope.len(),
            "envelope_sha256": sha,
            "version": 1_u8,
        })
        .to_string();

        let stamp = stamp_secs_ago(604_800);
        let listing = vec![
            (TEST_CONFIG_ID.to_owned(), stamp.clone(), pointer_json),
            (key0.clone(), stamp.clone(), head.to_owned()),
            (key1.clone(), stamp, tail.to_owned()),
        ];
        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc must not abort on such a root");
        let rendered = out.join("\n");
        assert!(
            rendered.contains("NOT runtime-readable"),
            "GC must warn that this root is unreadable rather than call it healthy: {rendered}"
        );
        // This root is PROTECTED (kept) but not runtime-live, so the report must
        // list it as RETAINED and must NOT label it (or the store) "live".
        assert!(
            rendered.contains(&format!("keeping `{TEST_CONFIG_ID}`"))
                && rendered.contains("retained root(s)"),
            "an unreadable-but-protected root must be reported as retained: {rendered}"
        );
        assert!(
            !rendered.contains("live root") && !rendered.contains("live chunk"),
            "an unreadable root's chunks are protected/referenced, never labeled live: {rendered}"
        );
        // Fail-closed: nothing is deleted, including its chunks.
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in [&key0, &key1] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "must not delete a chunk of an unreadable root: `{key}`; log:\n{log}"
            );
        }
    }

    #[test]
    fn runtime_env_store_name_entries_include_only_non_default_mappings() {
        use edgezero_core::env_config::EnvConfig;

        let config = vec![ResolvedStoreId::from_logical("app_config")];
        let kv = vec![ResolvedStoreId::new("sessions", "production_sessions")];
        let secrets = vec![ResolvedStoreId::new("default", "production_secrets")];
        let stores = ProvisionStores {
            config: &config,
            kv: &kv,
            secrets: &secrets,
        };

        let entries = runtime_env_store_name_entries(&stores);
        assert_eq!(
            entries,
            vec![
                (
                    "EDGEZERO__STORES__KV__SESSIONS__NAME".to_owned(),
                    "production_sessions".to_owned(),
                ),
                (
                    "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                    "production_secrets".to_owned(),
                ),
            ]
        );

        let env = EnvConfig::from_vars(entries);
        assert_eq!(env.store_name("config", "app_config"), "app_config");
        assert_eq!(env.store_name("kv", "sessions"), "production_sessions");
        assert_eq!(env.store_name("secrets", "default"), "production_secrets");
    }

    #[test]
    fn runtime_env_key_matches_what_the_runtime_reads() {
        use edgezero_core::env_config::EnvConfig;

        // EnvConfig::from_vars strips `EDGEZERO__`, splits on `__`, lowercases;
        // store_key("config", id) looks up ["stores","config",id,"key"]. So the
        // entry name is the id uppercased. A near-miss is SILENT: the runtime
        // would fall back to the id and read production config.
        assert_eq!(
            runtime_env_key_for("app_config"),
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
        );

        // Prove it against the real reader rather than restating the format.
        let cfg = EnvConfig::from_vars([(
            runtime_env_key_for("app_config"),
            "app_config_staging".to_owned(),
        )]);
        assert_eq!(
            cfg.store_key("config", "app_config"),
            "app_config_staging",
            "the entry provision writes must be the one the runtime reads"
        );
    }

    #[test]
    fn staging_entries_from_production_mirrors_and_overrides() {
        // Production carries a non-config override, an explicit config selector,
        // and a __NAME redirect. The twin must copy the non-config entries
        // verbatim and redirect EVERY declared config store to `<logical>_staging`
        // — including one production has no explicit entry for (it relies on the
        // runtime default; the twin must NOT inherit that default).
        let production = vec![
            (
                "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL".to_owned(),
                "debug".to_owned(),
            ),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "custom_prod_key".to_owned(),
            ),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
                "app_config".to_owned(),
            ),
        ];
        let out = staging_entries_from_production(
            &production,
            &["app_config".to_owned(), "feature_flags".to_owned()],
        );

        // Non-selector overrides copied verbatim.
        assert!(out.contains(&(
            "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL".to_owned(),
            "debug".to_owned()
        )));
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
            "app_config".to_owned()
        )));
        // The selector production HAD is overridden to `<logical>_staging`, NOT
        // production's custom value.
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
            "app_config_staging".to_owned()
        )));
        assert!(!out.iter().any(|(_, value)| value == "custom_prod_key"));
        // The declared store production LACKED a selector for still gets one.
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__FEATURE_FLAGS__KEY".to_owned(),
            "feature_flags_staging".to_owned()
        )));
        // Exactly one entry per selector key (no duplicate from the copy path).
        assert_eq!(
            out.iter()
                .filter(|(key, _)| key == "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY")
                .count(),
            1
        );
    }

    #[test]
    fn runtime_store_name_entries_from_vars_filters_and_validates() {
        let entries = runtime_store_name_entries_from_vars([
            (
                "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                "physical_secrets".to_owned(),
            ),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "ignored_selector".to_owned(),
            ),
            ("UNRELATED".to_owned(), "ignored".to_owned()),
        ])
        .expect("valid store-name override");

        assert_eq!(
            entries,
            vec![(
                "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                "physical_secrets".to_owned(),
            )]
        );
        assert!(
            runtime_store_name_entries_from_vars([(
                "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                String::new(),
            )])
            .is_err(),
            "an empty mapped resource name must fail closed"
        );
    }

    #[test]
    fn process_store_name_overrides_win_before_staging_mirror() {
        let production = vec![
            (
                "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
                "old_secrets".to_owned(),
            ),
            ("EDGEZERO__LOGGING__LEVEL".to_owned(), "info".to_owned()),
        ];
        let overrides = vec![(
            "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
            "new_secrets".to_owned(),
        )];

        let effective = overlay_runtime_store_name_entries(&production, &overrides);
        let staging = staging_entries_from_production(&effective, &["app_config".to_owned()]);

        assert!(staging.contains(&(
            "EDGEZERO__STORES__SECRETS__DEFAULT__NAME".to_owned(),
            "new_secrets".to_owned(),
        )));
        assert!(!staging.iter().any(|(_, value)| value == "old_secrets"));
        assert!(staging.contains(&(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
            "app_config_staging".to_owned(),
        )));
    }

    #[test]
    fn find_resource_link_id_matches_on_link_name_not_resource_name() {
        // The link's `name` is an alias defaulting to the resource's name. The
        // staging relink depends on that alias: a store named
        // `edgezero_runtime_env_staging` is linked AS `edgezero_runtime_env`.
        let json = r#"[
            {"id":"LINK_KV","name":"sessions"},
            {"id":"LINK_ENV","name":"edgezero_runtime_env"}
        ]"#;
        assert_eq!(
            find_resource_link_id(json, "edgezero_runtime_env").as_deref(),
            Some("LINK_ENV")
        );
        // Absent link -> nothing to delete, not an error.
        assert_eq!(find_resource_link_id(json, "nope"), None);
        // Tolerates the `{"items": [...]}` envelope, like the store lookup.
        let enveloped = r#"{"items":[{"id":"L1","name":"edgezero_runtime_env"}]}"#;
        assert_eq!(
            find_resource_link_id(enveloped, "edgezero_runtime_env").as_deref(),
            Some("L1")
        );
        assert_eq!(find_resource_link_id("not json", "x"), None);
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_points_the_draft_at_the_staging_selector_store() {
        // The defect this closes: a clone inherits the active version's links,
        // so without a relink the staged version opens production's selector
        // store and reads PRODUCTION config -- `config push --staging` would
        // write a key nothing ever reads. The CLI threads the declared config
        // store as `--edgezero-staging-config=<logical>`.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--edgezero-staging-config=app_config"],
        );
        result.expect("staged deploy must succeed");

        // The twin MIRRORS production: the non-selector override is copied
        // verbatim, and the config selector is upserted (redirected to
        // `app_config_staging` via stdin) into the staging store.
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL"
            )),
            "production's non-config override must be mirrored into the twin: {argv:?}"
        );
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the config selector must be written into the twin: {argv:?}"
        );
        // The mirror runs while the draft is still editable, before the relink.
        let mirror_idx = argv
            .iter()
            .position(|line| line.starts_with("config-store-entry update --store-id=STAGEID1"))
            .expect("mirror upsert");

        // The inherited production link is dropped: a version cannot hold two
        // links under one name.
        let delete_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link delete"))
            .expect("the inherited runtime-env link must be deleted");
        assert_eq!(
            argv[delete_idx],
            "resource-link delete --service-id=SVC1 --version=7 --id=LINK1"
        );

        // The staging STORE is linked under the name the runtime opens.
        let create_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link create"))
            .expect("the staging selector store must be linked");
        assert_eq!(
            argv[create_idx],
            "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
        );

        // Order matters: delete before create (name collision), and both while
        // the version is still an editable draft -- i.e. before staging.
        assert!(delete_idx < create_idx, "delete must precede create");
        assert!(
            mirror_idx < delete_idx,
            "the twin must be mirrored before the draft is relinked to it"
        );
        let stage_idx = argv
            .iter()
            .position(|line| line.starts_with("service-version stage"))
            .expect("stage call");
        assert!(
            create_idx < stage_idx,
            "the relink must happen while the version is still a draft"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_works_for_an_app_that_selects_no_config() {
        use std::os::unix::fs::PermissionsExt as _;

        // An app declaring no config stores threads no
        // `--edgezero-staging-config`, so there is no selector to isolate:
        // staging is still meaningful (staged CODE, no config), the draft keeps
        // the inherited link, and no config-store lookup happens at all.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        // No config stores at all on the account.
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' '[]'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
        ])
        .expect("an app with no config selection must still be stageable");
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_auto_creates_the_staging_twin_when_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // A staged deploy owns the twin end to end: if the account has no
        // staging store yet, the deploy creates it (rather than failing), so a
        // provisioned app can stage without a separate setup step.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // Stateful fake: `config-store list` includes the twin ONLY after a
        // `config-store create` has touched the marker.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}}]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("staged deploy must auto-create the twin and succeed");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines()
                .any(|line| line == "config-store create --name=edgezero_runtime_env_staging_SVC1"),
            "the per-service twin must be created on demand: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_isolates_when_config_declared_but_prod_store_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // The app DECLARES config but has no `edgezero_runtime_env` store (never
        // provisioned an override store — production reads its default key). A
        // staged deploy must NOT silently inherit production config: it creates
        // the per-service twin, writes the `<logical>_staging` selector, and
        // relinks the draft to it. There is nothing to mirror (no production
        // entries), but staging is still isolated.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // No `edgezero_runtime_env` ever; the twin appears only after create.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("must isolate staging even with no production override store");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the staging selector must be written even with no production store: {argv}"
        );
        assert!(
            argv.lines().any(|line| line.starts_with(
                "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
            )),
            "the draft must be relinked to the staging twin: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_config_store_list_is_unreadable() {
        use std::os::unix::fs::PermissionsExt as _;

        // If the store listing can't be parsed (a CLI schema change), we cannot
        // tell whether production config exists — refuse rather than risk a
        // staged version that silently serves PRODUCTION config.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' 'not json at all'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        let err = deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect_err("an unreadable config-store listing must fail closed");
        assert!(
            err.contains("Refusing to stage") || err.contains("could not parse"),
            "the error must explain the refusal: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_without_comment_makes_no_version_comment_call() {
        let (result, argv) =
            run_deploy_staged_with_fake("SUCCESS: Updated package (service SVC1, version 7)", &[]);
        result.expect("staged deploy must succeed");
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version update")),
            "no comment => no `service-version update` call: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_version_is_unparseable() {
        // The old code fell back to the service's HIGHEST version here, which
        // could silently adopt a version created by a CONCURRENT deploy. We
        // must error out instead of guessing.
        let (result, argv) = run_deploy_staged_with_fake("uploaded, but nothing parseable", &[]);
        let err = result.expect_err("unparseable version must fail closed");
        assert!(
            err.contains("could not determine the staged version"),
            "unexpected error: {err}"
        );
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version stage")),
            "must not stage a guessed version: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_does_not_duplicate_non_interactive_from_passthrough() {
        // `--non-interactive` is an allowlisted `compute update` flag, so a
        // caller-supplied one is FORWARDED. We must not then append our own:
        // passing the switch twice makes the Fastly CLI exit non-zero.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--non-interactive"],
        );
        result.expect("staged deploy with a passthrough --non-interactive must succeed");
        let update = argv
            .iter()
            .find(|line| line.starts_with("compute update"))
            .expect("compute update was invoked");
        assert_eq!(
            update.matches("--non-interactive").count(),
            1,
            "the non-interactive switch must appear exactly once: {update}"
        );
    }

    /// Fake `fastly` on `$PATH` that records `<cwd>\t<argv>` for every
    /// invocation. Used to prove the production deploy runs in the
    /// manifest-selected app directory.
    #[cfg(unix)]
    fn fake_fastly_cwd_recorder() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let script_path = dir.path().join("fastly");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\t%s\\n' \"$PWD\" \"$*\" >> '{}'\nexit 0\n",
            record.display(),
        );
        fs::write(&script_path, script).expect("write fake fastly");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        (dir, record)
    }

    #[cfg(unix)]
    #[test]
    fn deploy_honours_threaded_manifest_path_and_strips_it_from_the_fastly_argv() {
        // Production deploys used to ignore the CLI-threaded
        // `--manifest-path` and fall back to `find_fastly_manifest(cwd)`,
        // which in a monorepo picks the CLOSEST fastly.toml — the wrong
        // app. The threaded path must select the app directory, and must
        // be STRIPPED from the argv (`fastly compute deploy` has no such
        // flag and would exit non-zero).
        let _lock = path_mutation_guard().lock().expect("guard");
        let (fake, record) = fake_fastly_cwd_recorder();
        let _path = PathPrepend::new(fake.path());

        let app = tempdir().expect("app dir");
        let manifest = app.path().join("fastly.toml");
        fs::write(&manifest, "name = \"app\"\n").expect("write fastly.toml");

        let args = vec![
            "--manifest-path".to_owned(),
            manifest.display().to_string(),
            "--service-id".to_owned(),
            "SVC1".to_owned(),
        ];
        deploy(&args).expect("deploy must run against the threaded manifest");

        let recorded = fs::read_to_string(&record).expect("fastly was invoked");
        let (cwd, recorded_argv) = recorded
            .trim_end()
            .split_once('\t')
            .expect("recorded `<cwd>\\t<argv>`");
        assert_eq!(
            fs::canonicalize(cwd).expect("cwd"),
            fs::canonicalize(app.path()).expect("app dir"),
            "deploy must run in the manifest-selected app directory"
        );
        assert_eq!(
            recorded_argv,
            "compute deploy --service-id SVC1 --non-interactive"
        );
    }

    #[test]
    fn kept_roots_report_wording_counts_and_empty_store() {
        // Empty: a single, unambiguous "nothing retained" line and no root list.
        let mut empty = Vec::new();
        append_kept_roots_report(&mut empty, &[], 0);
        assert_eq!(empty, vec!["keeping 0 retained root(s)".to_owned()]);

        // Non-empty: a heading naming the RETAINED-root count and the
        // REFERENCED-chunk count, then one line per root by key.
        let mut out = Vec::new();
        append_kept_roots_report(
            &mut out,
            &["app_config".to_owned(), "app_config_staging".to_owned()],
            5,
        );
        assert!(
            out[0].contains("keeping 2 retained root(s)")
                && out[0].contains("5 referenced chunk(s)"),
            "heading names the retained-root and referenced-chunk counts: {out:?}"
        );
        assert!(out.iter().any(|line| line == "  keeping `app_config`"));
        assert!(
            out.iter()
                .any(|line| line == "  keeping `app_config_staging`")
        );
        // Never the misleading "live" label -- a retained root may not be
        // runtime-live, and its chunks are protected/referenced, not live.
        assert!(
            !out.iter()
                .any(|line| line.contains("live root") || line.contains("live chunk")),
            "must not label retained roots/chunks as live: {out:?}"
        );
    }

    /// A legitimate FOREIGN sibling (the documented `greeting = "hello"`) must
    /// NOT block store-wide GC. The runtime returns such a value verbatim, so GC
    /// protects it as a zero-reference root and still reclaims an unrelated dead
    /// generation, rather than aborting the whole pass.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_despite_a_foreign_sibling_value() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        let mut listing = vec![
            listed_root(TEST_CONFIG_ID, &live, 172_800),
            // A plain, non-envelope, non-pointer sibling entry.
            (
                "greeting".to_owned(),
                stamp_secs_ago(172_800),
                "hello".to_owned(),
            ),
        ];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("a foreign sibling must not abort GC");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "the dead generation must still be reclaimed: `{key}`"
            );
        }
        assert!(
            !oplog_has(&oplog, "delete greeting"),
            "the foreign sibling must never be deleted"
        );
    }

    /// A DIRECT envelope from a NEWER writer at an ordinary key classifies as
    /// `Foreign` (no `edgezero_kind`), so without the future-format guard GC would
    /// wave it through as a zero-reference root and reclaim an otherwise-dead
    /// generation -- yet the newer format may reference those chunks under a
    /// scheme this build cannot read. GC must FAIL CLOSED and delete nothing.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_future_direct_envelope_at_an_ordinary_key() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let dead = gen_envelope("dead");

        // Envelope-shaped, no discriminator, VERSION 2 -> a newer direct envelope.
        let future = r#"{"data":{"x":1},"sha256":"0000000000000000000000000000000000000000000000000000000000000000","generated_at":"2026-01-01T00:00:00Z","version":2}"#;
        let mut listing = vec![(
            "app_config".to_owned(),
            stamp_secs_ago(172_800),
            future.to_owned(),
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let result = run_gc(dir.path(), 86_400, false);
        assert!(
            result.is_err(),
            "a future direct envelope must abort GC (fail closed)"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when GC fails closed; log:\n{log}"
        );
    }

    /// A valid v1 pointer whose chunks reassemble to a NEWER inner format. GC can
    /// validate the outer pointer and reassemble the bytes, but the reassembled
    /// value may reference generations this build cannot see, so trusting only the
    /// outer chunks as the live set could delete live data. GC must fail closed --
    /// `BlobEnvelope` deserialize alone would silently ignore the newer format.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_future_inner_generation() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A v1 envelope, chunked, then its inner `version` bumped to 2. The v1
        // pointer's content-address still matches the reassembled (v2) bytes.
        let v1 = gen_envelope("live");
        let mut v2_value: serde_json::Value = serde_json::from_str(&v1).expect("parse");
        v2_value["version"] = serde_json::json!(2_u32);
        let v2 = v2_value.to_string();

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &v2, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &v2, 172_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let result = run_gc(dir.path(), 86_400, false);
        assert!(
            result
                .as_ref()
                .is_err_and(|err| err.contains("newer format")),
            "a future inner generation must abort GC with a newer-format error: {result:?}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when GC fails closed; log:\n{log}"
        );
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
        // It must also report what it is KEEPING: the live root, by key.
        assert!(
            rendered.contains(&format!("keeping `{TEST_CONFIG_ID}`")),
            "must name the retained live root: {rendered}"
        );
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

    /// The local prune must NOT delete a prior chunk key whose VALUE is a
    /// runtime-readable root (a valid direct envelope). A small envelope padded
    /// with trailing whitespace chunks so that chunk 0 is itself a whole,
    /// verifying envelope; deleting it would drop live config.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_a_valid_envelope() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // A padded envelope: chunk 0 is the whole envelope plus trailing spaces
        // (still a valid, verifying envelope on its own).
        let envelope = BlobEnvelope::new(json!({ "k": "v" }), "2026-06-22T00:00:00Z".into());
        let mut padded = serde_json::to_string(&envelope).unwrap();
        padded.push_str(&" ".repeat(8_200));
        let entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &padded).expect("expand");
        let chunk0_key = entries[0].0.clone();
        // Confirm the fixture: chunk 0's value verifies as an envelope.
        let parsed: BlobEnvelope = serde_json::from_str(&entries[0].1).expect("chunk0 parses");
        parsed.verify().expect("chunk0 verifies");

        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), padded)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push");

        // Re-push a direct value: the prior generation's chunks become orphans.
        let direct = make_test_envelope(100);
        let expected_deletions = entries.len().saturating_sub(2); // chunks minus the protected chunk0

        // DRY-RUN first: its count must MATCH what the real prune deletes, i.e.
        // it must exclude the protected root-like chunk0.
        let dry = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run");
        assert!(
            dry.join("\n")
                .contains(&format!("would delete {expected_deletions} orphan chunks")),
            "dry-run must count only the prunable orphans (excluding the protected root): {dry:?}"
        );

        let warnings = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
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

        assert!(
            contents.contains_key(&chunk0_key),
            "a chunk key holding a valid envelope is a runtime-readable root and must be kept: \
             {after}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("runtime-readable root")),
            "the operator must be warned that the key was kept: {warnings:?}"
        );
    }

    /// The local prune must NOT delete a prior chunk key whose value was written
    /// by a NEWER format -- a v2 direct envelope (bumped version) stored under a
    /// chunk-shaped key. Cloud GC fails closed on it; local prune must be
    /// symmetric, or an older CLI destroys config a newer writer produced.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_a_future_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        // A v2 direct envelope (version bumped) parked at a chunk key.
        let mut v2_value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&BlobEnvelope::new(
                json!({ "k": "v" }),
                "2026-01-01T00:00:00Z".to_owned(),
            ))
            .unwrap(),
        )
        .unwrap();
        v2_value["version"] = json!(2_u32);
        let v2 = v2_value.to_string();

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let victim = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        contents.insert(&victim, toml_edit::value(v2));
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        let direct = make_test_envelope(100);
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        assert!(
            after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
                .as_table()
                .expect("contents")
                .contains_key(&victim),
            "a v2 (future-format) envelope must be KEPT, not pruned: {after}"
        );
    }

    /// The local prune must NOT delete a prior chunk key whose value claims our
    /// `edgezero_kind` namespace with an UNKNOWN/future kind. The cloud GC path
    /// fails closed on such a value; local replacement must be symmetric, or it
    /// would destroy a newer-format entry an older CLI cannot understand.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_an_unknown_kind() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // Seed a real chunked generation, then overwrite ONE chunk value with a
        // future-format value that claims our namespace but is not a v1 pointer.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let victim = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        contents.insert(
            &victim,
            toml_edit::value(r#"{"edgezero_kind":"fastly_config_chunks_v2","new":true}"#),
        );
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Re-push a direct value: every prior chunk becomes an orphan.
        let direct = make_test_envelope(100);
        let warnings = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let present = after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table()
            .expect("contents")
            .contains_key(&victim);
        assert!(
            present,
            "an unknown/future-kind value must be KEPT (symmetric with cloud GC fail-closed): {after}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("kept") && warning.contains("edgezero_kind")),
            "the operator must be warned the namespace-claiming key was kept: {warnings:?}"
        );
    }

    /// SYMMETRY with cloud GC: a local prune must NOT delete a truncated pointer
    /// at a chunk-shaped key that HAS a canonical chunk nested beneath it — it is
    /// a (broken) nested root, and removing it would orphan the nested chunks.
    #[test]
    fn push_config_entries_local_keeps_a_malformed_nested_root_holder() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // Seed a chunked generation, then turn ONE chunk key into a nested root:
        // give it a truncated (unclassifiable) value AND a canonical chunk nested
        // beneath it.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let holder = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        // Truncated pointer at the holder (unclassifiable, announces no kind).
        contents.insert(&holder, toml_edit::value(r#"{"chunks":[{"key":"#));
        // A canonical chunk nested BENEATH the holder.
        let nested_chunk = format!("{holder}{CHUNK_KEY_INFIX}{}.0", "b".repeat(64));
        contents.insert(&nested_chunk, toml_edit::value("nested-payload"));
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Re-push a direct value: every prior chunk becomes an orphan, including
        // the holder (which the OLD pointer referenced).
        let direct = make_test_envelope(100);
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let after_contents = after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table()
            .expect("contents");
        assert!(
            after_contents.contains_key(&holder),
            "a nested-root holder with chunks beneath it must be KEPT, not pruned: {after}"
        );
    }

    /// `preflight_config_write` rejects an infeasible push BEFORE any provider
    /// I/O: a reserved key, an empty key, and a body whose DERIVED chunk keys
    /// would exceed the store limit (caught by running expansion offline).
    #[test]
    fn preflight_config_write_rejects_infeasible_pushes_offline() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let small = make_test_envelope(100);

        let reserved = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");
        assert!(
            FastlyCliAdapter
                .preflight_config_write(&reserved, &small)
                .is_err_and(|err| err.contains("reserved infix")),
            "a reserved-namespace key must be rejected"
        );

        assert!(
            FastlyCliAdapter
                .preflight_config_write("", &small)
                .is_err_and(|err| err.contains("empty")),
            "an empty key must be rejected"
        );

        // A ~200-char root with a CHUNKED body: derived chunk keys (root + ~85)
        // exceed the 255-char limit. Caught offline by expansion.
        let long_root = "r".repeat(200);
        let big = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        assert!(
            FastlyCliAdapter
                .preflight_config_write(&long_root, &big)
                .is_err(),
            "an over-limit derived chunk key must be rejected before I/O"
        );

        // A normal push passes.
        FastlyCliAdapter
            .preflight_config_write("app_config", &small)
            .expect("a normal push must pass preflight");
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
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
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

    /// TOCTOU guard: if the locked reread finds the root now holds a NEWER format
    /// (installed between the pre-push check and the lock), the writer must REFUSE
    /// to overwrite it -- an older writer must never clobber a newer format.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_to_overwrite_a_future_prior() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // The root now holds a v2 direct envelope from a newer writer.
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"data\\\":{},\\\"sha256\\\":\\\"x\\\",\\\"generated_at\\\":\\\"t\\\",\\\"version\\\":2}\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a future prior must abort the push under the lock");
        assert!(
            err.contains("newer format") && err.to_lowercase().contains("refusing"),
            "must refuse to clobber a newer format: {err}"
        );
        // The v2 value must survive untouched.
        let after = fs::read_to_string(&fastly_toml).expect("read");
        assert!(
            after.contains("\\\"version\\\":2"),
            "the newer-format value must be left intact: {after}"
        );
    }

    /// The locked downgrade guard must catch a future INNER envelope hidden behind
    /// a VALID v1 pointer -- only knowable after reconstruction against the locked
    /// contents. The raw pointer looks like healthy v1.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_a_future_inner_prior() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // A large v1 envelope, chunked, with its inner version bumped to 2. Seed
        // the pointer + chunks as the prior local state.
        let v1 = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let mut v2_value: serde_json::Value = serde_json::from_str(&v1).expect("parse");
        v2_value["version"] = serde_json::json!(2_u32);
        let v2 = v2_value.to_string();
        let seed_entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &v2).expect("chunk");
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &seed_entries, &[])
            .expect("seed the prior v2-inner generation");

        let direct = make_test_envelope(100);
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a future INNER prior must abort the push under the lock");
        assert!(
            err.contains("newer format") && err.to_lowercase().contains("refusing"),
            "must refuse to clobber a future inner envelope: {err}"
        );
    }

    /// A generated chunk key must never clobber an existing root-like sibling.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_clobbering_a_root_like_chunk_sibling() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        // The chunk keys the push will generate for this body.
        let body = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let generated = prepare_fastly_config_entries(TEST_CONFIG_ID, &body).expect("chunk");
        let chunk_key = generated
            .iter()
            .map(|(key, _)| key.clone())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a generated chunk key");

        // Pre-seed that EXACT key with a root-like value (a valid direct envelope).
        let root_like = make_test_envelope(100);
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[(chunk_key.clone(), root_like)],
            &[],
        )
        .expect("seed a root-like value at a chunk-shaped key");

        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), body)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a generated chunk key clobbering a root-like sibling must abort");
        assert!(
            err.contains("refusing to push") && err.contains(&chunk_key),
            "must refuse and name the colliding chunk key: {err}"
        );
    }

    /// The dry-run count must EXCLUDE a prior chunk that is already absent from
    /// the file: the real prune's `remove()` is a no-op there, so counting it
    /// would over-report the number of deletions.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_excludes_already_missing_prior_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // Seed a multi-chunk generation.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        // Manually delete ONE chunk entry: a prior chunk that is already gone.
        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let chunk_keys: Vec<String> = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(CHUNK_KEY_INFIX))
            .collect();
        assert!(chunk_keys.len() >= 2, "seed must have chunked");
        contents.remove(&chunk_keys[0]);
        let present_after = chunk_keys.len().saturating_sub(1);
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Dry-run a shrink-to-direct re-push: every remaining chunk is an orphan.
        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run");

        let reported = out
            .join("\n")
            .split("would delete ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .expect("a numeric orphan count");
        assert_eq!(
            reported, present_after,
            "the already-absent chunk must not be counted (reported {reported}, present {present_after})"
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

    /// Two concurrent local pushes must not lose each other's edit. Each thread
    /// adds a DISTINCT key; the cross-process lock serialises the whole
    /// read-modify-write, so the second push reads what the first wrote and both
    /// keys survive. Without the lock, both would read the same base and the
    /// later rename would discard the earlier key -- the silent data loss.
    #[cfg(unix)]
    #[test]
    fn concurrent_local_pushes_do_not_lose_edits() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().expect("tempdir");
        let path = Arc::new(dir.path().join("fastly.toml"));
        fs::write(path.as_ref(), "name = \"demo\"\n").expect("seed");

        // Many rounds to make the interleaving likely to hit the race window.
        for round in 0_u32..25 {
            let path_a = Arc::clone(&path);
            let path_b = Arc::clone(&path);
            let key_a = format!("alpha_{round}");
            let key_b = format!("beta_{round}");
            let (ka, kb) = (key_a.clone(), key_b.clone());
            let ta = thread::spawn(move || {
                write_fastly_local_config_store(
                    &path_a,
                    TEST_CONFIG_ID,
                    &[(ka, "a".to_owned())],
                    &[],
                )
            });
            let tb = thread::spawn(move || {
                write_fastly_local_config_store(
                    &path_b,
                    TEST_CONFIG_ID,
                    &[(kb, "b".to_owned())],
                    &[],
                )
            });
            ta.join().expect("thread a").expect("push a");
            tb.join().expect("thread b").expect("push b");

            let after = fs::read_to_string(path.as_ref()).expect("read back");
            assert!(
                after.contains(&format!("{key_a} = \"a\"")),
                "round {round}: `{key_a}` was lost by a concurrent push:\n{after}"
            );
            assert!(
                after.contains(&format!("{key_b} = \"b\"")),
                "round {round}: `{key_b}` was lost by a concurrent push:\n{after}"
            );
        }
    }

    /// A concurrent edit to `fastly.toml` between the push's read and its write
    /// must NOT be clobbered: the rewrite refuses and reports, leaving the other
    /// writer's file intact so no sibling change is silently lost.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_refuses_to_clobber_a_concurrent_edit() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // Simulate: we read one thing, another writer moved the file, we write.
        let stale_view = "name = \"demo\"\n";
        fs::write(&fastly_toml, "name = \"demo\"\nother = \"sibling edit\"\n")
            .expect("concurrent write");

        let err = atomically_replace_file(&fastly_toml, stale_view, "name = \"clobbered\"\n")
            .expect_err("a concurrent edit must not be overwritten");
        assert!(
            err.contains("changed on disk"),
            "must report the conflict: {err}"
        );
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            "name = \"demo\"\nother = \"sibling edit\"\n",
            "the other writer's file must survive untouched"
        );
    }

    /// The happy path replaces contents and leaves no temp file behind.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_replaces_atomically_and_cleans_up() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "before\n").expect("seed");

        atomically_replace_file(&fastly_toml, "before\n", "after\n").expect("replace");
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            "after\n",
            "contents must be replaced"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file may be left behind");
    }

    /// The atomic replace must PRESERVE the target's permissions: a 0600 manifest
    /// must not widen to the umask default when it is replaced.
    #[cfg(unix)]
    #[test]
    fn atomic_replace_preserves_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let manifest = dir.path().join("fastly.toml");
        fs::write(&manifest, "before\n").expect("seed");
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).expect("chmod");

        atomically_replace_file(&manifest, "before\n", "after\n").expect("replace");

        let mode = fs::metadata(&manifest).expect("meta").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "restrictive permissions must survive the replace"
        );
        assert_eq!(fs::read_to_string(&manifest).expect("read"), "after\n");
    }

    /// A symlinked manifest must be updated THROUGH the link: the real file's
    /// contents change and the symlink itself is preserved (not replaced with a
    /// regular file). The lock and the replace both resolve to the real target.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_follows_a_symlinked_manifest() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().expect("tempdir");
        let real = dir.path().join("real-fastly.toml");
        let link = dir.path().join("fastly.toml");
        fs::write(&real, "name = \"demo\"\n").expect("seed real");
        symlink(&real, &link).expect("symlink");

        write_fastly_local_config_store(
            &link,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
            &[],
        )
        .expect("push through symlink");

        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink(),
            "the manifest symlink must be preserved, not replaced with a file"
        );
        assert!(
            fs::read_to_string(&real)
                .expect("read real")
                .contains("greeting = \"hi\""),
            "the real target behind the symlink must be updated"
        );
    }

    /// A DANGLING manifest symlink (points at a not-yet-created file) must be
    /// FOLLOWED: the write creates the intended target and preserves the symlink,
    /// rather than replacing the link with a regular file and leaving the target
    /// absent.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_follows_a_dangling_symlinked_manifest() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("real-fastly.toml"); // does NOT exist yet
        let link = dir.path().join("fastly.toml");
        symlink(&target, &link).expect("dangling symlink");
        assert!(!target.exists(), "target must start absent");

        write_fastly_local_config_store(
            &link,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
            &[],
        )
        .expect("push through dangling symlink");

        assert!(
            fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink(),
            "the symlink must be preserved, not replaced with a regular file"
        );
        assert!(
            fs::read_to_string(&target)
                .expect("target must now exist")
                .contains("greeting = \"hi\""),
            "the intended (formerly-missing) target must be created and written"
        );
    }

    /// A MULTI-HOP dangling symlink chain (fastly.toml -> middle.toml ->
    /// missing.toml) must be followed to the FINAL target: the last file is
    /// created and every intermediate link is preserved. A direct write to the
    /// final target also resolves to the same lock.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_follows_a_multi_hop_dangling_symlink_chain() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().expect("tempdir");
        let final_target = dir.path().join("missing.toml"); // absent
        let middle = dir.path().join("middle.toml");
        let link = dir.path().join("fastly.toml");
        symlink(&final_target, &middle).expect("middle -> missing");
        symlink(&middle, &link).expect("fastly -> middle");
        assert!(!final_target.exists(), "final target must start absent");

        write_fastly_local_config_store(
            &link,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
            &[],
        )
        .expect("push through the symlink chain");

        for intermediate in [&link, &middle] {
            assert!(
                fs::symlink_metadata(intermediate)
                    .expect("lstat")
                    .file_type()
                    .is_symlink(),
                "every intermediate link must be preserved: {}",
                intermediate.display()
            );
        }
        assert!(
            fs::read_to_string(&final_target)
                .expect("final target must now exist")
                .contains("greeting = \"hi\""),
            "the final target at the end of the chain must be created and written"
        );
        // Symmetry: a direct write to the final target resolves to the same real
        // file the chain does, so both share one lock.
        assert_eq!(
            canonical_manifest_target(&link).expect("chain resolves"),
            canonical_manifest_target(&final_target).expect("direct resolves"),
            "the chain and a direct path must resolve to the same lock target"
        );
    }

    /// A HARD-LINKED manifest cannot be replaced safely (rename breaks the link;
    /// path-based locks miss the other names), so the writer FAILS CLOSED with a
    /// fix rather than silently diverging.
    #[cfg(unix)]
    #[test]
    fn local_rewrite_refuses_a_hard_linked_manifest() {
        let dir = tempdir().expect("tempdir");
        let manifest = dir.path().join("fastly.toml");
        let other = dir.path().join("other-name.toml");
        fs::write(&manifest, "name = \"demo\"\n").expect("seed");
        fs::hard_link(&manifest, &other).expect("hard link");

        let err = write_fastly_local_config_store(
            &manifest,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
            &[],
        )
        .expect_err("a hard-linked manifest must be refused");
        assert!(
            err.contains("hard link"),
            "must explain the hard-link refusal: {err}"
        );
        // Nothing was written -- the original content is intact.
        assert_eq!(
            fs::read_to_string(&manifest).expect("read"),
            "name = \"demo\"\n",
            "a refused write must not modify the manifest"
        );
    }

    /// A provision write and a local push serialise on the SAME manifest lock, so
    /// neither loses the other's edit even though they are different writers.
    #[cfg(unix)]
    #[test]
    fn provision_and_push_serialise_on_the_manifest_lock() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().expect("tempdir");
        let manifest = Arc::new(dir.path().join("fastly.toml"));
        fs::write(manifest.as_ref(), "name = \"demo\"\n").expect("seed");

        for _round in 0_u32..25 {
            let p_provision = Arc::clone(&manifest);
            let p_push = Arc::clone(&manifest);
            let provision =
                thread::spawn(move || append_fastly_setup(&p_provision, "config", "app_config"));
            let push = thread::spawn(move || {
                write_fastly_local_config_store(
                    &p_push,
                    TEST_CONFIG_ID,
                    &[("greeting".to_owned(), "hi".to_owned())],
                    &[],
                )
            });
            provision
                .join()
                .expect("provision thread")
                .expect("provision");
            push.join().expect("push thread").expect("push");

            let after = fs::read_to_string(manifest.as_ref()).expect("read");
            assert!(
                after.contains("[setup.config_stores.app_config]"),
                "provision's setup block must survive:\n{after}"
            );
            assert!(
                after.contains("greeting = \"hi\""),
                "push's config edit must survive:\n{after}"
            );
        }
    }

    /// PARITY: the dry-run's reported orphan count equals the number of chunk
    /// keys the real (non-dry-run) push actually deletes, on ONE fixture. A
    /// divergence would make the dry-run a misleading preview of the delete.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_count_matches_real_deletions() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;

        fn count_chunk_keys(toml_src: &str) -> usize {
            let doc: toml_edit::DocumentMut = toml_src.parse().expect("parse");
            doc.get("local_server")
                .and_then(|ls| ls.get("config_stores"))
                .and_then(|cs| cs.get(TEST_CONFIG_ID))
                .and_then(|st| st.get("contents"))
                .and_then(toml_edit::Item::as_table)
                .map_or(0, |table| {
                    table
                        .iter()
                        .filter(|(key, _)| key.contains(CHUNK_KEY_INFIX))
                        .count()
                })
        }
        fn parse_would_delete_count(text: &str) -> Option<usize> {
            let marker = "would delete ";
            let idx = text.find(marker)?;
            text.get(idx.saturating_add(marker.len())..)?
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()
        }

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // Seed a multi-chunk generation, then measure how many chunk keys exist.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed push");
        let seeded = fs::read_to_string(&fastly_toml).expect("read");
        let prior_chunk_count = count_chunk_keys(&seeded);
        assert!(prior_chunk_count >= 2, "seed must have chunked: {seeded}");

        // Dry-run a shrink-to-direct re-push: capture the reported orphan count.
        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run");
        let reported = parse_would_delete_count(&out.join("\n"))
            .expect("dry-run must report a numeric orphan count");
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            seeded,
            "dry-run must not edit fastly.toml"
        );

        // Real re-push: count the chunk keys actually removed.
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("real push");
        let after = fs::read_to_string(&fastly_toml).expect("read");
        let actually_deleted = prior_chunk_count.saturating_sub(count_chunk_keys(&after));

        assert_eq!(
            reported, actually_deleted,
            "dry-run count {reported} must equal real deletions {actually_deleted}"
        );
        assert_eq!(
            reported, prior_chunk_count,
            "a shrink-to-direct re-push orphans every prior chunk"
        );
    }

    /// Real (non-dry-run) push over a MALFORMED prior pointer WARNS and deletes
    /// nothing: its chunk list is untrustworthy, so no key is removed and the
    /// root is simply overwritten with the new value. This is the real-push
    /// counterpart to the dry-run "unknown" degradation on the same prior state.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_real_push_over_malformed_prior_warns_and_deletes_nothing() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // A pointer-kind prior value missing its required fields — malformed, so
        // `prior_chunk_keys` returns Err (warn, delete nothing).
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
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
            .expect("real push must not fail on a malformed prior");

        assert!(
            out.iter().any(|line| line.contains("skipping chunk GC")),
            "must warn about the malformed prior pointer: {out:?}"
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
        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "root is overwritten with the new direct envelope: {after}"
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
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
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
