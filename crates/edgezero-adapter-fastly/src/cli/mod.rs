use std::collections::HashSet;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::id as process_id;

use ctor::ctor;
use edgezero_adapter::cli_support;
use edgezero_adapter::cli_support::run_native_cli;
use edgezero_adapter::registry::{
    Adapter, AdapterAction, AdapterDeployedState, AdapterExecContext, AdapterPushContext,
    ProvisionMode, ProvisionOutcome, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
    TypedSecretEntry, register_adapter,
};
use edgezero_adapter::scaffold::{
    AdapterBlueprint, AdapterFileSpec, CommandTemplates, DependencySpec, LoggingDefaults,
    ManifestSpec, ReadmeInfo, TemplateRegistration, register_adapter_blueprint,
};

use crate::chunked_config::{
    CHUNK_KEY_INFIX, ResolveFailure, chunk_key_generation, gc_classify_root,
    prepare_fastly_config_entries, value_announces_our_kind, value_is_future_format,
};

mod gc;
mod provision_cloud;
mod provision_local;
mod push_cloud;
mod push_local;
mod run;
#[cfg(test)]
mod test_support;

/// Fastly's INTERNAL runtime-override config store. Provision creates it and
/// writes the `__NAME` / `__KEY` overlays into it (local Viceroy block and the
/// cloud config-store). A user-declared store resolving to the SAME platform
/// name would be merged into it, cross-contaminating operator config with the
/// runtime overrides -- so the name is reserved.
pub(super) const RUNTIME_ENV_STORE_NAME: &str = "edgezero_runtime_env";

/// Reject a SINGLE resolved store whose PLATFORM name collides with the
/// reserved [`RUNTIME_ENV_STORE_NAME`]. Used by the config read/write
/// dispatch so a runtime env overlay can't route application config into the
/// internal runtime-override store (which provision manages) and overwrite
/// runtime configuration.
fn reject_reserved_store(store: &ResolvedStoreId) -> Result<(), String> {
    if store.platform == RUNTIME_ENV_STORE_NAME {
        return Err(format!(
            "fastly: store `{}` (platform name `{}`) collides with the reserved runtime-override config store `{RUNTIME_ENV_STORE_NAME}` that EdgeZero manages. Rename the store id or its `EDGEZERO__STORES__..__NAME` override.",
            store.logical, store.platform
        ));
    }
    Ok(())
}

/// Reject any declared store whose PLATFORM name collides with the reserved
/// [`RUNTIME_ENV_STORE_NAME`], BEFORE provision writes anything. Shared by the
/// local and cloud provision arms.
pub(super) fn reject_reserved_store_names(stores: &ProvisionStores<'_>) -> Result<(), String> {
    for (kind, group) in [
        ("kv", stores.kv),
        ("config", stores.config),
        ("secrets", stores.secrets),
    ] {
        for store in group {
            if store.platform == RUNTIME_ENV_STORE_NAME {
                return Err(format!(
                    "fastly: {kind} store `{}` (platform name `{}`) collides with the reserved runtime-override config store `{RUNTIME_ENV_STORE_NAME}` that provision manages. Rename the store id or its `EDGEZERO__STORES__{}__..__NAME` override.",
                    store.logical,
                    store.platform,
                    kind.to_ascii_uppercase(),
                ));
            }
        }
    }
    Ok(())
}

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
        emit_commands: true,
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

// `fastly.toml` is intentionally absent from the scaffold
// registration — same rationale as Axum, Cloudflare, and Spin.
// The scaffold-time provision loop
// (`generator::provision_all_selected_adapters` ->
// `Adapter::synthesise_baseline_manifest` -> `run::synthesise_fastly_toml`)
// is the single writer. Registering a scaffold template would
// make the file exist before provision runs; provision's
// `write_baseline_to_disk` skips files that already exist (spec §
// "Adapter manifests are gitignored"), so `edgezero new` and
// clean-clone `provision --local` would diverge.
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
];

static FASTLY_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "fastly_Cargo_toml",
        contents: include_str!("../templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_src_main_rs",
        contents: include_str!("../templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "fastly_cargo_config_toml",
        contents: include_str!("../templates/.cargo/config.toml.hbs"),
    },
];

pub(super) const FASTLY_INSTALL_HINT: &str = "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

pub(super) struct FastlyCliAdapter;

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
pub(super) enum ConfigStoreLookup {
    Found(String),
    NotFound,
    SchemaDrift(String),
}

// The three `validate_*` trait methods exist on `Adapter` because
// spin requires them (variable-name regex, `[component.*]`
// discovery, flat-namespace collision). The trait surface is typed
// generically so any future adapter with similar constraints can
// override:
//
// - `validate_app_config_keys`: Fastly Config Store keys accept
//   alphanumeric + `-` / `_` / `.` up to 256 chars. Any reasonable
//   Rust struct field name passes; no regex check needed — no-op.
// - `validate_adapter_manifest`: would require shelling out to
//   `fastly compute validate` at validate-time. We keep
//   `config validate` pure-Rust so it stays fast and
//   tool-independent — no-op.
// - `validate_typed_secrets`: IS implemented. Fastly's KV / Config
//   / Secret stores are independent namespaces, so there is no
//   spin-style flat-namespace collision on the CLOUD path. The
//   LOCAL path has its own: `provision --local` derives each
//   secret's Viceroy env var as `key.to_ascii_uppercase()`, which
//   is lossy — see the impl for the collision this rejects.
impl Adapter for FastlyCliAdapter {
    fn deployed_fields(&self) -> &'static [&'static str] {
        &["service_id"]
    }

    fn execute(
        &self,
        action: AdapterAction,
        args: &[String],
        ctx: &AdapterExecContext<'_>,
    ) -> Result<(), String> {
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
                let artifact = run::build(args, ctx)?;
                log::info!("[edgezero] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => run::deploy(args, ctx),
            AdapterAction::Serve => run::serve(args, ctx),
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
        gc::gc_fastly_config_store(store.platform.as_str(), older_than_secs, dry_run)
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

    // Fastly's KV / Config / Secret stores are independent
    // namespaces — no flat-namespace merging like Spin.
    #[inline]
    fn merged_id_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    // No spin-style multi-component discovery in fastly.toml; the
    // adapter's per-manifest validation is deferred to
    // `fastly compute validate` at deploy time.
    #[inline]
    fn validate_adapter_manifest(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _allow_component_refresh: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    // Fastly Config Store keys accept alphanumeric + `-` / `_` /
    // `.` up to 256 chars — any reasonable Rust field name passes.
    #[inline]
    fn validate_app_config_keys(&self, _keys: &[&str]) -> Result<(), String> {
        Ok(())
    }

    // Fastly Secret Store keys share Config Store's naming rules, so
    // the key itself needs no canonicalisation check. The LOCAL path
    // does: `provision --local` writes each key into `fastly.toml` as
    // `{ key = "<key>", env = "<KEY>" }`, where the env name is
    // `key.to_ascii_uppercase()` -- Viceroy sources the secret's value
    // from that variable. The env name has no store qualifier, so two
    // DISTINCT (store, key) secrets that upper-case to the same name --
    // whether they differ by case (`api_token` / `API_TOKEN`) or by
    // store (`store_a`/`api_token` vs `store_b`/`api_token`) -- both
    // read `$API_TOKEN` and silently collapse to one value. Reject at
    // validation rather than let a wrong secret be served.
    fn validate_typed_secrets(&self, entries: &[TypedSecretEntry<'_>]) -> Result<(), String> {
        use std::collections::HashMap;
        // A secret's production identity is (store, key): the same key
        // in two DIFFERENT stores is two DIFFERENT secrets whose values
        // may differ. But `provision --local` derives the Viceroy env
        // var from the key alone (`key.to_ascii_uppercase()`), with no
        // store qualifier, so both would read the same `$KEY` and
        // silently resolve to one value. Reject any two DISTINCT
        // (store, key) pairs that collide on the same env var -- whether
        // they differ by case (`api_token` / `API_TOKEN`) or by store
        // (`store_a`/`api_token` vs `store_b`/`api_token`). The same
        // (store, key) referenced twice is fine (one secret, two refs).
        let mut seen: HashMap<String, (&str, &str, &str)> = HashMap::with_capacity(entries.len());
        for entry in entries {
            let env_name = entry.key_value.to_ascii_uppercase();
            if let Some((prev_store, prev_key, prev_field)) = seen.get(&env_name) {
                if (*prev_store, *prev_key) != (entry.store_id, entry.key_value) {
                    return Err(format!(
                        "`#[secret]` fields `{prev_field}` (store `{prev_store}`, key `{prev_key}`) \
                         and `{this_field}` (store `{this_store}`, key `{this_key}`) both map to the \
                         Viceroy environment variable `{env_name}` in `fastly.toml` -- provision \
                         derives it by upper-casing the key with no store qualifier, so these two \
                         DISTINCT secrets would resolve to a single value. Pick keys that differ by \
                         more than case, even across stores.",
                        this_field = entry.field_name,
                        this_store = entry.store_id,
                        this_key = entry.key_value,
                    ));
                }
            } else {
                seen.insert(
                    env_name,
                    (entry.store_id, entry.key_value, entry.field_name.as_str()),
                );
            }
        }
        Ok(())
    }

    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        deployed: Option<&AdapterDeployedState>,
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        match mode {
            ProvisionMode::Local => provision_local::provision(
                manifest_root,
                adapter_manifest_path,
                stores,
                deployed,
                dry_run,
            ),
            ProvisionMode::Cloud => provision_cloud::provision(
                manifest_root,
                adapter_manifest_path,
                stores,
                deployed,
                dry_run,
            ),
            // ProvisionMode is #[non_exhaustive]; a future mode variant
            // is an explicit error so we don't dispatch via one of the
            // two known arms by accident.
            other => Err(format!(
                "fastly adapter does not implement provision mode {other:?}"
            )),
        }
    }

    fn provision_typed(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        typed_secrets: &[TypedSecretEntry<'_>],
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        // Cloud secret storage uses `fastly secret-store-entry create`
        // at deploy time. Local mode delegates to `provision_local`
        // which seeds Viceroy's `[[local_server.secret_stores.<id>]]`
        // array-of-tables — cloud mode is a documented no-op.
        if !matches!(mode, ProvisionMode::Local) {
            return Ok(ProvisionOutcome::default());
        }
        provision_local::provision_typed(
            manifest_root,
            adapter_manifest_path,
            typed_secrets,
            dry_run,
        )
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
        reject_reserved_store(store)?;
        push_cloud::write_entries(store, entries, dry_run)
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
        reject_reserved_store(store)?;
        push_local::write_entries(
            manifest_root,
            adapter_manifest_path,
            store,
            entries,
            dry_run,
        )
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
        reject_reserved_store(store)?;
        push_cloud::read_entry(store, key)
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
        reject_reserved_store(store)?;
        push_local::read_entry(manifest_root, adapter_manifest_path, store, key)
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        // Explicit `&[]` rather than inheriting the trait default,
        // so the "Multi for every store kind" intent is documented
        // at the call site. Fastly KV / Config / Secrets all
        // support multiple distinct platform resources per kind,
        // unlike spin's flat-namespace single-store model.
        &[]
    }

    fn synthesise_baseline_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        adapter_crate_path: Option<&str>,
        _component_selector: Option<&str>,
        app_name: &str,
        deployed: Option<&AdapterDeployedState>,
        _allowed_outbound_hosts: &[String],
    ) -> Result<Vec<(PathBuf, String)>, String> {
        // The CLI's `deployed_state_for` translator copies
        // `[adapters.fastly.deployed].service_id` into
        // `deployed.fields["service_id"]` before calling this override,
        // so the adapter reads the flat field bag and never links to
        // `edgezero-core`.
        let deployed_service_id = deployed
            .and_then(|state| state.fields.get("service_id"))
            .map(String::as_str);
        let rel = adapter_manifest_path.map_or_else(|| PathBuf::from("fastly.toml"), PathBuf::from);
        // Prefer the ACTUAL adapter crate name. The authoritative source
        // is the declared `[adapters.fastly.adapter].crate`; fall back to
        // the ancestor `Cargo.toml` search only when it's undeclared, and
        // finally to the scaffold convention. (An ancestor search alone
        // could pick a nested package between the manifest and the crate.)
        let crate_name = match cli_support::read_crate_name_at(manifest_root, adapter_crate_path)? {
            Some(name) => name,
            None => cli_support::read_adapter_crate_name(manifest_root, adapter_manifest_path)
                .unwrap_or_else(|| {
                    if app_name.is_empty() {
                        "app-adapter-fastly".to_owned()
                    } else {
                        format!("{app_name}-adapter-fastly")
                    }
                }),
        };
        Ok(vec![(
            rel,
            run::synthesise_fastly_toml(&crate_name, deployed_service_id),
        )])
    }
}

/// Hard-error message for a value written by a NEWER format this v1 CLI must not
/// overwrite. Shared by the read paths so the wording stays consistent.
const FUTURE_FORMAT_READ_ERROR: &str = "the remote value uses a config format this CLI version does not recognise (a newer \
     `edgezero_kind` or envelope/pointer version); UPGRADE the CLI to push to this store rather \
     than overwrite a newer format.";

/// An exclusive, cross-process advisory lock covering a local `fastly.toml`
/// rewrite. Serialises concurrent pushes so their read-modify-write cycles
/// cannot interleave and lose each other's edits.
///
/// The lock is a persistent sidecar file next to the manifest. It is never
/// unlinked — deleting it would reintroduce a create/lock race between two
/// processes each making their own lock file. Dropping the guard releases the
/// OS lock (closing the file descriptor). `File::lock` is advisory, so it only
/// coordinates other lockers, which is exactly the pushes we control.
pub(super) struct ManifestLock {
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

impl ManifestLock {
    pub(super) fn acquire(manifest_path: &Path) -> Result<Self, String> {
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
    pub(super) fn target(&self) -> &Path {
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

/// Replace an already-canonical `target`'s contents ATOMICALLY. Callers pass
/// [`ManifestLock::target`] and hold the lock across the surrounding
/// read-modify-write, so this is not racing another writer; the re-read + compare
/// is a defence-in-depth corruption check, not the concurrency guard.
pub(super) fn atomically_replace_file(
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

/// Expand ONE logical `(root_key, body)` into its physical entries, the
/// exact keep-set for that root, and the value written at the root key.
/// No cross-root prefix scanning (a free-form `--key` can't mislead it).
#[expect(
    clippy::type_complexity,
    reason = "one-off internal return; a named type would not aid readability"
)]
pub(super) fn expand_root(
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

/// Reject logical keys that collide with the reserved chunk namespace.
/// `--key` is free-form, so this is enforced at the Fastly adapter
/// boundary: such a key would let a push write into another key's chunk
/// space, and could not be reclaimed correctly.
pub(super) fn reject_reserved_root_keys(entries: &[(String, String)]) -> Result<(), String> {
    for (key, _) in entries {
        if key.contains(CHUNK_KEY_INFIX) {
            return Err(format!(
                "config key `{key}` contains the reserved infix `{CHUNK_KEY_INFIX}`, which collides with Fastly chunk storage; choose a different config key (or --key override)"
            ));
        }
    }
    Ok(())
}

/// Reject a batch that names the same logical root key more than once.
///
/// GC builds one plan per entry and snapshots every plan against the SAME prior
/// generation. With `[(root, A), (root, B)]` the last tuple wins the upsert
/// (root = B), yet A's plan would still reclaim `prior - A_keys` — which includes
/// B's freshly-written chunks — leaving the final pointer referencing missing
/// chunks. Rejecting is safer than silently coalescing.
pub(super) fn reject_duplicate_root_keys(entries: &[(String, String)]) -> Result<(), String> {
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
pub(super) fn reject_generated_key_collisions(
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

/// Does `body` parse AND integrity-verify as a `BlobEnvelope`?
///
/// A value that is not a verifying envelope (invalid JSON, missing fields, or a
/// SHA mismatch) is corrupt FOR THE PUSH -- something to overwrite, not to diff
/// against.
fn body_is_valid_envelope(body: &str) -> bool {
    use edgezero_core::blob_envelope::BlobEnvelope;
    serde_json::from_str::<BlobEnvelope>(body).is_ok_and(|envelope| envelope.verify().is_ok())
}

/// Map a `resolve_fastly_config_value` result to a read outcome, distinguishing
/// the cases that must NOT be treated as overwritable corruption:
///
/// - a FUTURE format → a hard error (checked FIRST): overwriting a newer format
///   with this v1 CLI would lose it. Detected on the raw stored value AND via a
///   typed [`ResolveFailure::FutureFormat`] from the resolver.
/// - a resolve error where a chunk FETCH failed for infrastructure reasons
///   (`fetch_failed`) → a hard error: the read was incomplete.
/// - `Ok(body)` that verifies as an envelope → `Present`.
/// - `Ok(body)` that is NOT a valid envelope, or any other resolve error → `Corrupt`.
pub(super) fn classify_resolved_read(
    resolved: Result<String, ResolveFailure>,
    raw_value: &str,
    fetch_failed: bool,
) -> Result<ReadConfigEntry, String> {
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
        // not overwrite. The resolver's message is already redacted.
        Err(err) if fetch_failed => Err(format!(
            "a chunk fetch failed while reading the remote value ({}); the remote was not fully \
             read, so nothing was changed. Fix connectivity/auth and retry.",
            err.into_message()
        )),
        Ok(body) if body_is_valid_envelope(&body) => Ok(ReadConfigEntry::Present(body)),
        Ok(_) => Ok(ReadConfigEntry::Corrupt(
            "remote value is not a valid config envelope; a push will overwrite it",
        )),
        // A confirmed-absent chunk, a hash mismatch, or a malformed pointer.
        Err(_) => Ok(ReadConfigEntry::Corrupt(
            "remote prior value could not be resolved (corrupt or incomplete chunk state); a push \
             will overwrite it",
        )),
    }
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

// Shared process-wide mutex serialising PATH-mutating tests across every
// submodule test suite in this crate. Tests in `provision_local`, `push_cloud`,
// etc. all install shell shims via `PathPrepend` and would otherwise race on
// the environment variable.
#[cfg(all(test, unix))]
use std::sync::Mutex as PathMutationMutex;

#[cfg(all(test, unix))]
pub(crate) fn path_mutation_guard() -> &'static PathMutationMutex<()> {
    use std::sync::OnceLock;
    static GUARD: OnceLock<PathMutationMutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| PathMutationMutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::{TEST_CONFIG_ID, make_test_envelope};
    use edgezero_adapter::TypedSecretEntry;
    use edgezero_adapter::registry::{AdapterPushContext, ResolvedStoreId};
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn config_dispatch_rejects_reserved_runtime_env_store() {
        // An env overlay that routes a config store's platform name to the
        // reserved `edgezero_runtime_env` must be refused at the read/write
        // dispatch -- not just during provision -- so app config can't
        // overwrite the internal runtime-override store.
        let store = ResolvedStoreId::new("app_config", "edgezero_runtime_env");
        let ctx = AdapterPushContext::new();
        let entries = [("k".to_owned(), "v".to_owned())];
        for result in [
            FastlyCliAdapter
                .read_config_entry(Path::new("."), Some("fastly.toml"), None, &store, "k", &ctx)
                .map(|_| ()),
            FastlyCliAdapter
                .push_config_entries(
                    Path::new("."),
                    Some("fastly.toml"),
                    None,
                    &store,
                    &entries,
                    &ctx,
                    true,
                )
                .map(|_| ()),
            FastlyCliAdapter
                .read_config_entry_local(
                    Path::new("."),
                    Some("fastly.toml"),
                    None,
                    &store,
                    "k",
                    &ctx,
                )
                .map(|_| ()),
            FastlyCliAdapter
                .push_config_entries_local(
                    Path::new("."),
                    Some("fastly.toml"),
                    None,
                    &store,
                    &entries,
                    &ctx,
                    true,
                )
                .map(|_| ()),
        ] {
            let Err(err) = result else {
                panic!("a config op against the reserved runtime store must be refused");
            };
            assert!(
                err.contains("reserved") && err.contains("edgezero_runtime_env"),
                "error explains the reserved-store collision: {err}"
            );
        }
    }

    #[test]
    fn validate_typed_secrets_passes_with_no_collision() {
        FastlyCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("default", "field_a", "api_token"),
                TypedSecretEntry::new("default", "field_b", "db_password"),
            ])
            .expect("distinct keys must pass");
    }

    /// The SAME key in two DIFFERENT stores is two distinct secrets
    /// (identity is (store, key)) whose values may differ, but both
    /// derive the same `API_TOKEN` env var with no store qualifier --
    /// so provision would collapse them to one value. Reject it.
    #[test]
    fn validate_typed_secrets_rejects_same_key_across_two_stores() {
        let err = FastlyCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("store_a", "field_a", "api_token"),
                TypedSecretEntry::new("store_b", "field_b", "api_token"),
            ])
            .expect_err("same key in two stores collides on one Viceroy env var");
        assert!(
            err.contains("store_a") && err.contains("store_b") && err.contains("API_TOKEN"),
            "error names both stores and the shared env var: {err}"
        );
    }

    /// The exact same (store, key) referenced twice is one secret with
    /// two references -- not a collision.
    #[test]
    fn validate_typed_secrets_allows_same_store_and_key_referenced_twice() {
        FastlyCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("default", "field_a", "api_token"),
                TypedSecretEntry::new("default", "field_b", "api_token"),
            ])
            .expect("one secret referenced by two fields is fine");
    }

    /// Regression: keys
    /// differing only in case both upper-case to `API_TOKEN`, so
    /// `fastly.toml` gets two secret-store rows reading the same
    /// Viceroy env var and the two secrets silently share a value.
    #[test]
    fn validate_typed_secrets_rejects_keys_differing_only_by_case() {
        let err = FastlyCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("default", "lower_field", "api_token"),
                TypedSecretEntry::new("default", "upper_field", "API_TOKEN"),
            ])
            .expect_err("keys differing only by case must collide on the derived env var");
        assert!(
            err.contains("API_TOKEN") && err.contains("lower_field") && err.contains("upper_field"),
            "error names the shared env var and BOTH fields: {err}"
        );
    }

    /// The collision is on the derived env var, not the store, so it
    /// must be caught across stores too.
    #[test]
    fn validate_typed_secrets_rejects_case_collision_across_stores() {
        let err = FastlyCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("store_a", "lower_field", "api_token"),
                TypedSecretEntry::new("store_b", "upper_field", "Api_Token"),
            ])
            .expect_err("case collision must be caught across stores");
        assert!(err.contains("API_TOKEN"), "{err}");
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
}
