//! `provision` command.
//!
//! Thin delegate to the adapter registry. The CLI loads the manifest,
//! resolves the named adapter, hands it the declared store ids per
//! kind, and prints the human-readable status lines the adapter
//! returns. All the platform-specific work — `wrangler kv namespace
//! create`, `fastly *-store create`, `spin.toml` editing — lives in
//! each `edgezero-adapter-*` crate's `Adapter::provision` impl, not
//! here.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use similar::TextDiff;
use toml_edit::{table, value, DocumentMut};

use crate::args::ProvisionArgs;
use crate::config::{
    build_typed_secret_entries, enforce_single_store_capability,
    load_validation_context_with_options, reject_merged_id_collisions,
    resolve_app_config_path_primitive, run_typed_preflight, strict_handler_paths,
    validate_deployed_field_ownership, ValidationContext,
};
use crate::copy_tree::copy_dir_recursive;
use crate::ensure_adapter_defined;
use crate::path_safety::{assert_provision_paths_contained, assert_provision_paths_safe};
use crate::provision_lock::ProvisionLock;
use edgezero_adapter::registry::{
    self as adapter_registry, ProvisionOutcome, ProvisionStores, ResolvedStoreId, TypedSecretEntry,
};
use edgezero_adapter::AdapterDeployedState;
use edgezero_core::app_config::{self, AppConfigLoadOptions, AppConfigMeta};
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{Manifest, ManifestAdapter, ManifestLoader, StoreDeclaration};
use serde::de::DeserializeOwned;
use validator::Validate;

/// Owned counterpart to the borrowed `ProvisionStores<'_>`. Used by
/// dispatch arms that need to build resolved store ids per-root
/// (e.g. inside the `run_with_staging` closure where a borrowed
/// return would dangle when the `Vec` locals dropped). Task 29
/// (typed provision) consumes this too.
pub(crate) struct OwnedProvisionStores {
    pub config: Vec<ResolvedStoreId>,
    pub kv: Vec<ResolvedStoreId>,
    pub secrets: Vec<ResolvedStoreId>,
}

impl OwnedProvisionStores {
    pub(crate) fn as_refs(&self) -> ProvisionStores<'_> {
        ProvisionStores {
            config: &self.config,
            kv: &self.kv,
            secrets: &self.secrets,
        }
    }
}

/// Resolved per-adapter allow-list inputs the dry-run driver diffs.
/// Built by the CLI from the resolved adapter manifest path (NOT a
/// static filename) so nested paths like
/// `crates/cf/config/wrangler.toml` resolve correctly. Spec §"Per-
/// adapter local state" defines membership per adapter:
/// - Axum: resolved `axum.toml` + project-root `.edgezero/.env`.
/// - Cloudflare: resolved `wrangler.toml` + sibling `.dev.vars`.
/// - Fastly: resolved `fastly.toml`.
/// - Spin: resolved `spin.toml` + sibling `runtime-config.toml` +
///   sibling `.env`.
pub(crate) struct DryRunAllowList {
    /// (`project_path`, `staged_path`) pairs the driver diffs.
    pub pairs: Vec<(PathBuf, PathBuf)>,
}

/// # Errors
///
/// Manifest-shape gates run before the dispatch matrix: capability
/// gate, handler-path shape, and deployed-field ownership. The
/// ownership check exists here for parity with `run_shared_checks` in
/// the config path, so `config validate` / `push` / `diff` and
/// `provision` all reject the same manifests. Extracted from
/// `run_provision` to keep that fn under the workspace `too_many_lines`
/// lint; no behaviour change.
///
/// # Errors
///
/// Returns the first check's error string when any of the three gates
/// rejects the manifest.
fn run_manifest_shape_gates(manifest: &Manifest, adapter_name: &str) -> Result<(), String> {
    enforce_single_store_capability(manifest, adapter_name)?;
    strict_handler_paths(manifest)?;
    validate_deployed_field_ownership(manifest)?;
    Ok(())
}

/// Resolve the project root that hosts the manifest file. Returns
/// `.` when the manifest path has an empty parent (bare
/// `--manifest edgezero.toml`), matching `run_with_staging`'s
/// project-relative expectations.
fn manifest_root_from(manifest_path: &Path) -> &Path {
    manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// # Errors
///
/// Returns an error string if the manifest can't be loaded, the
/// adapter isn't declared in `[adapters.*]`, the adapter isn't
/// registered in this build, or the adapter's `provision` impl
/// reports a failure.
#[inline]
/// Acquire the cross-process provision lock when the invocation
/// will actually write. Returns `None` for dry-run so a long
/// staging + diff doesn't starve real writers.
/// Reject absolute paths + `..` traversal in the adapter-declared
/// manifest / crate strings UNCONDITIONALLY. Cloud provision also
/// joins `manifest_root` with the adapter-declared manifest path to
/// write local files (Cloudflare's `wrangler.toml`, Fastly's
/// `fastly.toml`), so a poisoned `manifest = "../outside/foo.toml"`
/// in `edgezero.toml` would escape the tree in cloud mode too.
///
/// The stricter "manifest must sit inside the adapter crate dir"
/// step is `--local`-only: existing cloud fixtures legitimately use
/// e.g. `manifest = "wrangler.toml"` at the project root with
/// `crate = "crates/demo-cf"`, and the strict-local rule would
/// reject them. Cloud paths go through `assert_provision_paths_safe`
/// (Step 1 only); local paths get `assert_provision_paths_contained`
/// (Steps 1+2).
fn enforce_adapter_path_guard(
    args: &ProvisionArgs,
    adapter_cfg: &ManifestAdapter,
) -> Result<(), String> {
    let manifest_root_for_check = manifest_root_from(&args.manifest);
    if args.local {
        assert_provision_paths_contained(
            manifest_root_for_check,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.crate_path.as_deref(),
        )
    } else {
        assert_provision_paths_safe(
            manifest_root_for_check,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.crate_path.as_deref(),
        )
    }
}

fn acquire_provision_lock(args: &ProvisionArgs) -> Result<Option<ProvisionLock>, String> {
    if args.dry_run {
        Ok(None)
    } else {
        Ok(Some(ProvisionLock::acquire(manifest_root_from(
            &args.manifest,
        ))?))
    }
}

/// # Errors
///
/// Returns an error string when the manifest fails to load, the
/// adapter isn't declared, path containment rejects a poisoned
/// manifest path, adapter dispatch fails, or the deployed-writeback
/// merge fails.
#[inline]
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String> {
    // Serialise concurrent invocations against the same tree so
    // read-modify-write on `.env` / `.dev.vars` / `edgezero.toml`
    // never silently drops a competing writer's edits. Dry-run
    // skips: nothing is written, and holding the lock during a
    // long staging + diff would starve real writers. See
    // `provision_lock.rs` for the design rationale.
    let _lock = acquire_provision_lock(args)?;
    run_provision_inner(args)
}

/// Lock-agnostic body of [`run_provision`]. Callers MUST hold the
/// `.edgezero-provision.lock` before invoking (see
/// [`acquire_provision_lock`]). Extracted so `run_provision_typed`
/// can hold a single lock across the base run + typed writeback +
/// deployed merge, without `run_provision`'s inner acquisition
/// re-entering flock (which is per-descriptor on Linux/macOS and
/// would deadlock a second acquire from the same process).
fn run_provision_inner(args: &ProvisionArgs) -> Result<(), String> {
    let manifest_loader = ManifestLoader::from_path(&args.manifest)
        .map_err(|err| format!("failed to load {}: {err}", args.manifest.display()))?;
    let manifest = manifest_loader.manifest();

    // Declared in `edgezero.toml`? (Catches typos before we try to
    // look the adapter up in the registry.)
    ensure_adapter_defined(&args.adapter, Some(&manifest_loader))?;
    let (_canonical, adapter_cfg) = manifest.adapter_entry(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is not declared in {}",
            args.adapter,
            args.manifest.display()
        )
    })?;

    enforce_adapter_path_guard(args, adapter_cfg)?;

    // Linked in this build? Adapters are feature-gated; a release
    // built without `--features cloudflare` won't have it
    // registered even if the manifest declares it.
    let adapter = adapter_registry::get_adapter(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is declared in {} but not registered in this build (rebuild `edgezero-cli` with its feature enabled)",
            args.adapter,
            args.manifest.display()
        )
    })?;

    run_manifest_shape_gates(manifest, &args.adapter)?;

    let manifest_root = manifest_root_from(&args.manifest);

    // Fallback to "" when [app].name is unset: today's synthesiser
    // default is a no-op so the value isn't consulted; per-adapter
    // overrides (Tasks 17/21/24) that DO use it treat empty as the
    // "operator hasn't set app.name yet" case.
    let app_name = manifest.app.name.clone().unwrap_or_default();

    // Translate the manifest's deployed block into the neutral
    // `AdapterDeployedState` for the synthesiser call site. Task 14
    // adds the typed struct that makes this a real translation;
    // today it's always `None`.
    let deployed = deployed_state_for(manifest, &args.adapter);

    let outcome = match (args.local, args.dry_run) {
        (false, dry_run) => {
            // Cloud: no synthesis. Validate + build stores against the
            // real worktree, dispatch with mode=Cloud, deployed=None.
            let dispatch = DispatchContext {
                adapter,
                adapter_cfg,
                adapter_name: &args.adapter,
                manifest_root,
            };
            validate_and_dispatch(
                &dispatch,
                manifest,
                None,
                adapter_registry::ProvisionMode::Cloud,
                dry_run,
            )?
        }
        (true, false) => {
            // Local real-write: synthesise baseline INSIDE this arm
            // so cloud never touches it. Write baseline to the
            // worktree, then validate + build stores + dispatch.
            let baseline_pairs = adapter.synthesise_baseline_manifest(
                manifest_root,
                adapter_cfg.adapter.manifest.as_deref(),
                adapter_cfg.adapter.component.as_deref(),
                &app_name,
                deployed.as_ref(),
            )?;
            let synthesised = write_baseline_to_disk(manifest_root, &baseline_pairs)?;
            let dispatch = DispatchContext {
                adapter,
                adapter_cfg,
                adapter_name: &args.adapter,
                manifest_root,
            };
            let merged = validate_and_dispatch(
                &dispatch,
                manifest,
                deployed.as_ref(),
                adapter_registry::ProvisionMode::Local,
                false,
            )?;
            prepend_baseline_status_lines_worktree(&args.adapter, &synthesised, merged)
        }
        (true, true) => run_local_dry_run(
            adapter,
            manifest,
            adapter_cfg,
            manifest_root,
            args,
            &app_name,
            deployed.as_ref(),
        )?,
    };

    if args.dry_run {
        log::info!("[edgezero] provision --dry-run for `{}`:", args.adapter);
    }
    for line in outcome.status_lines {
        log::info!("{line}");
    }
    if let Some(deployed_writeback) = outcome.deployed.as_ref() {
        let (canonical_adapter_key, _) = manifest
            .adapter_entry(&args.adapter)
            .ok_or_else(|| format!("adapter `{}` vanished from manifest", args.adapter))?;
        merge_deployed_into_manifest(
            &args.manifest,
            canonical_adapter_key,
            deployed_writeback,
            adapter.deployed_fields(),
            args.dry_run,
        )?;
    }
    Ok(())
}

/// Typed-secret companion to [`run_provision`]. Only meaningful in
/// local mode — cloud provisioning short-circuits to the base
/// `run_provision` (typed handling flows through vendor CLIs like
/// `wrangler secret put` at deploy time).
///
/// Combines `run_provision` (base) + `Adapter::provision_typed`
/// (secret placeholders) under a shared preflight. In dry-run,
/// both dispatch calls execute inside a single `run_with_staging`
/// tempdir so the typed merge sees the baseline manifest the base
/// step wrote.
///
/// # Errors
///
/// Returns an error string if the manifest can't be loaded, the
/// adapter isn't declared, the app-config fails validation, or any
/// adapter dispatch reports a failure.
#[inline]
pub fn run_provision_typed<C>(args: &ProvisionArgs) -> Result<(), String>
where
    C: DeserializeOwned + Validate + AppConfigMeta,
{
    // Cloud: delegate. No typed writeback in cloud mode.
    if !args.local {
        return run_provision(args);
    }

    // Acquire the cross-process lock ONCE and hold it for the entire
    // typed local flow: base run + typed writeback + deployed merge
    // are all read-modify-write against the same set of files, so a
    // peer sneaking in between the base and the typed step would
    // silently drop one of their appends. Dry-run skips (no writes).
    let _lock = acquire_provision_lock(args)?;

    // 1. Validation context (env_overlay=false — provision captures
    //    operator-typed values, not env-overridden ones).
    let ctx = load_validation_context_with_options(&args.manifest, None, false, false)?;

    // 2. Base preflight gates — must fire BEFORE any tempdir work
    //    so dry-run can't bypass expensive-mistake protection.
    //    Real-write inherits these from `run_provision_inner`'s
    //    own call below; dry-run must include ALL three gates
    //    (capability + handler paths + deployed-field ownership)
    //    or a manifest that the actual run would reject can slip
    //    past dry-run and mislead the operator into thinking
    //    provision will succeed.
    run_manifest_shape_gates(ctx.manifest(), &args.adapter)?;

    // 3. Canonical adapter lookup (case-insensitive on the key).
    //    Clone the canonical spelling so the borrow from
    //    `adapter_entry` frees before later re-borrows of
    //    `ctx.manifest()` inside the staging closure.
    let (canonical_borrow, adapter_cfg) = ctx
        .manifest()
        .adapter_entry(&args.adapter)
        .ok_or_else(|| format!("adapter `{}` not declared in manifest", args.adapter))?;
    let canonical_adapter_name = canonical_borrow.clone();
    let adapter_manifest_rel_owned = adapter_cfg.adapter.manifest.clone();
    let adapter_component_owned = adapter_cfg.adapter.component.clone();
    let adapter_crate_rel_owned = adapter_cfg.adapter.crate_path.clone();
    let manifest_root = manifest_root_from(&args.manifest);

    // 4. Path containment (mirrors run_provision's local-mode gate).
    assert_provision_paths_contained(
        manifest_root,
        adapter_manifest_rel_owned.as_deref(),
        adapter_crate_rel_owned.as_deref(),
    )?;

    // 5. Typed deserialise + non-secret validate. Reconstruct the
    //    app-config path + app name from the manifest instead of
    //    calling `ctx.app_config_path()` / `ctx.app_name()` — those
    //    accessors carry a `#[cfg_attr(not(test), expect(dead_code,
    //    ...))]` marker that would flip to `unfulfilled_lint
    //    _expectations` the moment this lib code exercised them.
    //    `load_validation_context_with_options` already guaranteed
    //    `manifest.app.name` is `Some`, so `unwrap_or_default` is
    //    load-bearing only for the impossible-shape case.
    let manifest = ctx.manifest();
    let app_name = manifest.app.name.clone().unwrap_or_default();
    let app_config_path = resolve_app_config_path_primitive(None, &args.manifest, &app_name);
    let mut opts = AppConfigLoadOptions::default();
    opts.env_overlay = false;
    let cfg: C =
        app_config::deserialize_app_config_with_options::<C>(&app_config_path, &app_name, &opts)
            .map_err(|err| format!("app config load failed: {err}"))?;
    app_config::validate_excluding_secrets(&cfg)
        .map_err(|err| format!("app config validation failed: {err}"))?;

    // 6. Shared typed preflight (typed_secret_checks + per-adapter
    //    validate_typed_secrets).
    run_typed_preflight(&cfg, &ctx)?;

    // 7. Build the TypedSecretEntry slice.
    let entries = build_typed_secret_entries::<C>(&ctx)?;

    // 8. Dispatch.
    let adapter = adapter_registry::get_adapter(&canonical_adapter_name).ok_or_else(|| {
        format!("adapter `{canonical_adapter_name}` is not registered in this build")
    })?;

    if !args.dry_run {
        // Real-write: base then typed against the live worktree.
        // Call the lock-agnostic inner so we don't re-acquire flock
        // from the same process (which would deadlock -- flock is
        // per-descriptor on Linux/macOS and a second descriptor from
        // this process would block waiting for ourselves).
        run_provision_inner(args)?;
        let outcome = adapter.provision_typed(
            manifest_root,
            adapter_manifest_rel_owned.as_deref(),
            adapter_component_owned.as_deref(),
            &entries,
            adapter_registry::ProvisionMode::Local,
            false,
        )?;
        for line in outcome.status_lines {
            log::info!("{line}");
        }
        // Same writeback path as the base run_provision: if the typed
        // arm surfaced any deployed fields (e.g. cloud secret store
        // ids captured during typed placeholder emission), merge them
        // into `[adapters.<name>.deployed]` in edgezero.toml. Today
        // every impl returns deployed: None so this is a no-op; the
        // hook exists so a future secrets-store-id capture doesn't
        // silently leak out the side.
        if let Some(deployed_writeback) = outcome.deployed.as_ref() {
            let (canonical_adapter_key, _) = manifest
                .adapter_entry(&args.adapter)
                .ok_or_else(|| format!("adapter `{}` vanished from manifest", args.adapter))?;
            merge_deployed_into_manifest(
                &args.manifest,
                canonical_adapter_key,
                deployed_writeback,
                adapter.deployed_fields(),
                args.dry_run,
            )?;
        }
        return Ok(());
    }

    let report = run_local_dry_run_typed(
        &DryRunTypedRequest {
            adapter,
            ctx: &ctx,
            canonical_adapter_name: &canonical_adapter_name,
            adapter_manifest_rel: adapter_manifest_rel_owned.as_deref(),
            adapter_component: adapter_component_owned.as_deref(),
            adapter_crate_rel: adapter_crate_rel_owned.as_deref(),
            manifest_root,
        },
        args,
        &entries,
    )?;
    if !report.is_empty() {
        log::info!("{report}");
    }
    Ok(())
}

/// Dry-run arm of [`run_provision_typed`]. Extracted so the parent
/// function stays under the workspace `too_many_lines` lint. Stages
/// a tempdir that hosts BOTH `adapter.provision` (base) AND
/// `adapter.provision_typed` (secret placeholders) so the typed
/// merge sees the baseline manifest the base step wrote. The report
/// is rendered inside the closure — `staged_root` is only valid
/// until the tempdir drops.
/// Bundle for [`run_local_dry_run_typed`]. Reduces the arg list from
/// 9 to 3 by grouping the adapter identity + dispatch paths under
/// one struct.
struct DryRunTypedRequest<'req> {
    adapter: &'static dyn adapter_registry::Adapter,
    ctx: &'req ValidationContext,
    canonical_adapter_name: &'req str,
    adapter_manifest_rel: Option<&'req str>,
    adapter_component: Option<&'req str>,
    adapter_crate_rel: Option<&'req str>,
    manifest_root: &'req Path,
}

fn run_local_dry_run_typed(
    req: &DryRunTypedRequest<'_>,
    args: &ProvisionArgs,
    entries: &[TypedSecretEntry<'_>],
) -> Result<String, String> {
    let &DryRunTypedRequest {
        adapter,
        ctx,
        canonical_adapter_name,
        adapter_manifest_rel,
        adapter_component,
        adapter_crate_rel,
        manifest_root,
    } = req;
    let adapter_crate_rel_path = adapter_crate_rel.map_or_else(|| Path::new("."), Path::new);
    let deployed_state = deployed_state_for(ctx.manifest(), canonical_adapter_name);
    let app_name = ctx.manifest().app.name.clone().unwrap_or_default();
    let baseline_pairs = adapter.synthesise_baseline_manifest(
        manifest_root,
        adapter_manifest_rel,
        adapter_component,
        &app_name,
        deployed_state.as_ref(),
    )?;

    let (report, _tempdir) = run_with_staging(
        manifest_root,
        adapter_crate_rel_path,
        |staged_root, _staged_crate| {
            // Sanitiser: rewrite raw staged tempdir paths back to the
            // project root's display form before bubbling any error
            // to the operator (spec §"Dry-run": stdout must NEVER
            // carry raw tempdir paths, in either the success OR
            // error paths). Mirrors the untyped dry-run arm's
            // treatment.
            let staged_str = staged_root.to_string_lossy().into_owned();
            let project_str = manifest_root.to_string_lossy().into_owned();
            let sanitize = |err: String| err.replace(&staged_str, &project_str);
            let synthesised =
                write_baseline_to_disk(staged_root, &baseline_pairs).map_err(&sanitize)?;
            adapter
                .validate_adapter_manifest(staged_root, adapter_manifest_rel, adapter_component)
                .map_err(&sanitize)?;
            let owned_stores = build_stores_against(staged_root, args, adapter, ctx.manifest())
                .map_err(&sanitize)?;
            let stores = owned_stores.as_refs();
            // Spec §"Dry-run" step 3: pass `dry_run = false` — the
            // tempdir IS the dry-run mechanism, not the flag.
            let base = adapter
                .provision(
                    staged_root,
                    adapter_manifest_rel,
                    adapter_component,
                    &stores,
                    deployed_state.as_ref(),
                    adapter_registry::ProvisionMode::Local,
                    false,
                )
                .map_err(&sanitize)?;
            let typed = adapter
                .provision_typed(
                    staged_root,
                    adapter_manifest_rel,
                    adapter_component,
                    entries,
                    adapter_registry::ProvisionMode::Local,
                    false,
                )
                .map_err(&sanitize)?;
            // Base + typed status merged; baseline "wrote …" lines
            // prepended after with staged tempdir paths rewritten to
            // the project root's display form (spec §"Dry-run":
            // stdout must NEVER carry raw tempdir paths).
            let base_status = base.status_lines;
            let typed_status = typed.status_lines;
            let merged_status: Vec<String> = base_status.into_iter().chain(typed_status).collect();
            let merged_outcome = match base.deployed.or(typed.deployed) {
                Some(state) => ProvisionOutcome::with_deployed(merged_status, state),
                None => ProvisionOutcome::from_status_lines(merged_status),
            };
            let combined = prepend_baseline_status_lines_with_rewrite(
                canonical_adapter_name,
                &synthesised,
                merged_outcome,
                |path| {
                    path.display()
                        .to_string()
                        .replace(&staged_str, &project_str)
                },
            );
            // Render INSIDE the closure — the allow-list builder /
            // `default_adapter_manifest_for` both match on lowercase,
            // so canonical (possibly mixed-case) names MUST be
            // lowercased before dispatch.
            let adapter_lower = canonical_adapter_name.to_ascii_lowercase();
            let adapter_manifest_rel_or_default = adapter_manifest_rel.map_or_else(
                || default_adapter_manifest_for(&adapter_lower).to_owned(),
                String::from,
            );
            let adapter_manifest_abs = staged_root.join(&adapter_manifest_rel_or_default);
            let allow_list = build_dry_run_allow_list(
                manifest_root,
                staged_root,
                &adapter_lower,
                &adapter_manifest_abs,
            );
            Ok(render_dry_run_report(
                manifest_root,
                staged_root,
                &allow_list,
                &combined,
            ))
        },
    )?;
    Ok(report)
}

/// Pair each declared id in `declaration` with its platform name
/// via the `EDGEZERO__STORES__<KIND>__<ID>__NAME` env overlay.
/// Returns empty when the manifest doesn't declare the kind.
fn resolve_kind(
    declaration: Option<&StoreDeclaration>,
    env_config: &EnvConfig,
    kind: &str,
) -> Vec<ResolvedStoreId> {
    declaration.map_or_else(Vec::new, |decl| {
        decl.ids
            .iter()
            .map(|id| ResolvedStoreId::new(id.clone(), env_config.store_name(kind, id)))
            .collect()
    })
}

/// Write each `(rel, contents)` baseline pair under `root`, skipping
/// files that already exist. Preserves operator content and earlier
/// synthesis output. Used for worktree writes (real-write local) and
/// tempdir writes (dry-run staging) — the only difference is which
/// root is passed in.
///
/// **Path containment**: each `rel_path` MUST be relative and MUST NOT
/// contain a `..` component. Adapter-returned baseline paths are trusted
/// less than manifest-declared paths (which `assert_provision_paths
/// _contained` gates upstream); a buggy or hostile synthesiser
/// returning an absolute path or `../../etc/passwd` would otherwise
/// escape the project tree. Reject before `root.join()`.
/// Prepend one `"<adapter>: wrote baseline <path>"` status line per
/// file the synthesiser step actually created (`write_baseline_to_disk`
/// skips existing files, so `synthesised` only lists first-write
/// events, not re-provision no-ops). Gives the operator a visible
/// confirmation for every adapter manifest -- notably `axum.toml`,
/// whose merge path is a no-op and wouldn't otherwise appear in the
/// status output.
fn prepend_baseline_status_lines_worktree(
    adapter_name: &str,
    synthesised: &[PathBuf],
    outcome: adapter_registry::ProvisionOutcome,
) -> adapter_registry::ProvisionOutcome {
    prepend_baseline_status_lines_with_rewrite(adapter_name, synthesised, outcome, |path| {
        path.display().to_string()
    })
}

fn prepend_baseline_status_lines_with_rewrite(
    adapter_name: &str,
    synthesised: &[PathBuf],
    outcome: adapter_registry::ProvisionOutcome,
    rewrite: impl Fn(&Path) -> String,
) -> adapter_registry::ProvisionOutcome {
    let baseline_lines: Vec<String> = synthesised
        .iter()
        .map(|path| format!("{adapter_name}: wrote baseline {}", rewrite(path.as_path())))
        .collect();
    let combined: Vec<String> = baseline_lines
        .into_iter()
        .chain(outcome.status_lines)
        .collect();
    match outcome.deployed {
        Some(state) => adapter_registry::ProvisionOutcome::with_deployed(combined, state),
        None => adapter_registry::ProvisionOutcome::from_status_lines(combined),
    }
}

fn write_baseline_to_disk(
    root: &Path,
    pairs: &[(PathBuf, String)],
) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    for (rel_path, contents) in pairs {
        if rel_path.is_absolute() {
            return Err(format!(
                "baseline path must be project-relative, got `{}`",
                rel_path.display()
            ));
        }
        if rel_path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(format!(
                "baseline path must not contain `..` traversal: `{}`",
                rel_path.display()
            ));
        }
        let abs = root.join(rel_path);
        if abs.exists() {
            continue;
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
        fs::write(&abs, contents).map_err(|err| format!("write {}: {err}", abs.display()))?;
        written.push(abs);
    }
    Ok(written)
}

/// Translate the parent manifest's `[adapters.<name>.deployed]` block
/// into the neutral `AdapterDeployedState` shape adapters consume via
/// `synthesise_baseline_manifest` and `provision`. Field mapping:
///   - `service_id` (scalar) → `state.fields["service_id"]`.
///   - `kv_namespaces` (map) → `state.sub_tables["kv_namespaces"]`.
///   - `preview_kv_namespaces` (map) → `state.sub_tables["preview_kv_namespaces"]`.
///
/// Returns `None` when the adapter has no `deployed` block OR when every
/// field is empty — synthesise / provision impls treat `None` the same as
/// an empty state, so building an empty `AdapterDeployedState` would be
/// wasteful. The lookup is case-insensitive via `manifest.adapter_entry`,
/// matching how `[adapters.Fastly]` and `[adapters.fastly]` resolve to
/// the same declaration.
fn deployed_state_for(
    manifest: &Manifest,
    canonical_adapter_name: &str,
) -> Option<AdapterDeployedState> {
    let (_, adapter_cfg) = manifest.adapter_entry(canonical_adapter_name)?;
    let deployed = adapter_cfg.deployed.as_ref()?;
    let mut state = AdapterDeployedState::default();
    if let Some(service_id) = deployed.service_id.as_ref() {
        state
            .fields
            .insert("service_id".to_owned(), service_id.clone());
    }
    if !deployed.kv_namespaces.is_empty() {
        state
            .sub_tables
            .insert("kv_namespaces".to_owned(), deployed.kv_namespaces.clone());
    }
    if !deployed.preview_kv_namespaces.is_empty() {
        state.sub_tables.insert(
            "preview_kv_namespaces".to_owned(),
            deployed.preview_kv_namespaces.clone(),
        );
    }
    if state.fields.is_empty() && state.sub_tables.is_empty() {
        None
    } else {
        Some(state)
    }
}

/// Merge `state` into `[adapters.<adapter_name>.deployed]` inside
/// `manifest_path`, preserving all sibling content and adjacent
/// operator comments via `toml_edit`. `adapter_name` MUST be the
/// canonical operator-spelled key (result of
/// `manifest.adapter_entry(...)`); passing the raw `args.adapter`
/// risks creating a parallel lowercased `[adapters.cloudflare.deployed]`
/// beside an operator-spelled `[adapters.Cloudflare]` table.
///
/// `state.fields` become scalar leaves; `state.sub_tables` become
/// nested `[<sub_name>]` sub-tables under `.deployed`. When
/// `dry_run` is true the helper builds the doc in memory then
/// returns without writing — used by callers who want the write
/// gated on the same `--dry-run` semantic as the surrounding
/// dispatch.
///
/// **Adapter-emitted schema check**: every key in `state.fields` and
/// `state.sub_tables` MUST be in the known `ManifestAdapterDeployed`
/// schema AND in `owned_fields`. `validate_deployed_field_ownership`
/// gates operator-written manifests before dispatch; this gate does
/// the same for adapter-emitted output before writing back. Without
/// it, a buggy adapter's `AdapterDeployedState` could persist
/// unknown or non-owned keys into `edgezero.toml`, breaking future
/// manifest loads.
pub(crate) fn merge_deployed_into_manifest(
    manifest_path: &Path,
    adapter_name: &str,
    state: &adapter_registry::AdapterDeployedState,
    owned_fields: &[&str],
    dry_run: bool,
) -> Result<(), String> {
    // The canonical `ManifestAdapterDeployed` schema — the field
    // arrays live on the struct itself in `edgezero_core::manifest`
    // so this check tracks the schema without a hand-copied duplicate
    // list to drift.
    use edgezero_core::manifest::ManifestAdapterDeployed;

    for key in state.fields.keys() {
        if !ManifestAdapterDeployed::SCALAR_FIELDS.contains(&key.as_str()) {
            return Err(format!(
                "adapter `{adapter_name}` returned unknown deployed field `{key}` (known scalar fields: [{}])",
                ManifestAdapterDeployed::SCALAR_FIELDS.join(", ")
            ));
        }
        if !owned_fields.contains(&key.as_str()) {
            return Err(format!(
                "adapter `{adapter_name}` returned deployed field `{key}` it does not own (owned: [{}])",
                owned_fields.join(", ")
            ));
        }
    }
    for key in state.sub_tables.keys() {
        if !ManifestAdapterDeployed::SUB_TABLE_FIELDS.contains(&key.as_str()) {
            return Err(format!(
                "adapter `{adapter_name}` returned unknown deployed sub-table `{key}` (known sub-tables: [{}])",
                ManifestAdapterDeployed::SUB_TABLE_FIELDS.join(", ")
            ));
        }
        if !owned_fields.contains(&key.as_str()) {
            return Err(format!(
                "adapter `{adapter_name}` returned deployed sub-table `{key}` it does not own (owned: [{}])",
                owned_fields.join(", ")
            ));
        }
    }

    let raw = fs::read_to_string(manifest_path)
        .map_err(|err| format!("read {}: {err}", manifest_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("parse {}: {err}", manifest_path.display()))?;

    // `entry(...).or_insert_with(table)` avoids the `IndexMut` lint
    // (`clippy::indexing_slicing`) that fires on `doc["adapters"]`.
    // If a sibling exists but isn't a table, we bail cleanly instead
    // of clobbering it — mirrors the fastly adapter's editor pattern.
    let adapters_item = doc.entry("adapters").or_insert_with(table);
    let adapters_tbl = adapters_item.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `adapters` exists but is not a table; refusing to edit in place",
            manifest_path.display()
        )
    })?;
    let named_item = adapters_tbl.entry(adapter_name).or_insert_with(table);
    let named_tbl = named_item.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `adapters.{adapter_name}` exists but is not a table; refusing to edit in place",
            manifest_path.display()
        )
    })?;
    let deployed_item = named_tbl.entry("deployed").or_insert_with(table);
    let deployed_tbl = deployed_item.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `adapters.{adapter_name}.deployed` exists but is not a table; refusing to edit in place",
            manifest_path.display()
        )
    })?;

    for (key, val) in &state.fields {
        deployed_tbl.insert(key, value(val.clone()));
    }
    for (sub_name, sub_map) in &state.sub_tables {
        let sub_item = deployed_tbl.entry(sub_name).or_insert_with(table);
        let sub_tbl = sub_item.as_table_mut().ok_or_else(|| {
            format!(
                "{}: `adapters.{adapter_name}.deployed.{sub_name}` exists but is not a table; refusing to edit in place",
                manifest_path.display()
            )
        })?;
        for (key, val) in sub_map {
            sub_tbl.insert(key, value(val.clone()));
        }
    }

    if dry_run {
        return Ok(());
    }
    fs::write(manifest_path, doc.to_string())
        .map_err(|err| format!("write {}: {err}", manifest_path.display()))?;
    Ok(())
}

/// Shared validate + env-overlay + collision-check + resolve-stores +
/// dispatch tail for both cloud and local live-mode arms. Baseline
/// synthesis (local only) fires BEFORE this helper — the tail after
/// synthesis is identical between the two arms, so factoring it here
/// keeps `run_provision` under the module's function-length lint AND
/// removes the copy-paste risk of two arms drifting out of sync.
/// Bundles the adapter identity + paths + root that both
/// `validate_and_dispatch` and its dry-run typed sibling need. Cuts
/// their argument lists from 8-9 down to a single ctx + the arm-
/// specific bits (deployed / mode / dry-run for the base tail;
/// entries / typed context for the dry-run typed tail).
struct DispatchContext<'dispatch> {
    adapter: &'static dyn adapter_registry::Adapter,
    adapter_cfg: &'dispatch ManifestAdapter,
    adapter_name: &'dispatch str,
    manifest_root: &'dispatch Path,
}

fn validate_and_dispatch(
    ctx: &DispatchContext<'_>,

    manifest: &Manifest,
    deployed: Option<&AdapterDeployedState>,
    mode: adapter_registry::ProvisionMode,
    dry_run: bool,
) -> Result<adapter_registry::ProvisionOutcome, String> {
    ctx.adapter.validate_adapter_manifest(
        ctx.manifest_root,
        ctx.adapter_cfg.adapter.manifest.as_deref(),
        ctx.adapter_cfg.adapter.component.as_deref(),
    )?;
    let env_config = EnvConfig::from_env();
    reject_merged_id_collisions(ctx.adapter_name, ctx.adapter, manifest, &env_config)?;
    let config_ids = resolve_kind(manifest.stores.config.as_ref(), &env_config, "config");
    let kv_ids = resolve_kind(manifest.stores.kv.as_ref(), &env_config, "kv");
    let secret_ids = resolve_kind(manifest.stores.secrets.as_ref(), &env_config, "secrets");
    let stores = ProvisionStores {
        config: &config_ids,
        kv: &kv_ids,
        secrets: &secret_ids,
    };
    ctx.adapter.provision(
        ctx.manifest_root,
        ctx.adapter_cfg.adapter.manifest.as_deref(),
        ctx.adapter_cfg.adapter.component.as_deref(),
        &stores,
        deployed,
        mode,
        dry_run,
    )
}

/// Same store-construction pattern `validate_and_dispatch` runs
/// inline, but returns the owned form so the caller can hold it
/// across a closure and `.as_refs()` immediately before dispatch.
/// Used by the `(true, true)` arm today; Task 29 (typed provision)
/// consumes it too.
///
/// The `_root` parameter is unused today — reserved for future
/// per-root state (e.g. reading a synthesised sidecar file).
///
/// `adapter` is `&'static dyn` to match `validate_and_dispatch` and
/// `reject_merged_id_collisions`, both of which take `'static`
/// trait objects because the registry only hands out `&'static`
/// references.
fn build_stores_against(
    _root: &Path,
    args: &ProvisionArgs,
    adapter: &'static dyn adapter_registry::Adapter,
    manifest: &Manifest,
) -> Result<OwnedProvisionStores, String> {
    let env_config = EnvConfig::from_env();
    reject_merged_id_collisions(&args.adapter, adapter, manifest, &env_config)?;
    Ok(OwnedProvisionStores {
        config: resolve_kind(manifest.stores.config.as_ref(), &env_config, "config"),
        kv: resolve_kind(manifest.stores.kv.as_ref(), &env_config, "kv"),
        secrets: resolve_kind(manifest.stores.secrets.as_ref(), &env_config, "secrets"),
    })
}

/// Build the allow-list from the resolved adapter manifest path.
/// `adapter_manifest_abs` is the absolute path the adapter would
/// write to (`staged_root.join(adapter_manifest_rel)`); the helper
/// computes its sibling paths and the corresponding project-tree
/// twins by prefix-swapping `staged_root` → `project_root`.
///
/// **Case contract:** callers MUST lowercase the adapter name
/// before passing it in. The manifest's canonical spelling (e.g.
/// `Fastly`) does NOT match the match arms.
pub(crate) fn build_dry_run_allow_list(
    project_root: &Path,
    staged_root: &Path,
    adapter_lower: &str,
    adapter_manifest_abs: &Path,
) -> DryRunAllowList {
    let project_manifest = adapter_manifest_abs.strip_prefix(staged_root).map_or_else(
        |_| adapter_manifest_abs.to_path_buf(),
        |rel| project_root.join(rel),
    );
    let manifest_parent_staged = adapter_manifest_abs
        .parent()
        .unwrap_or(staged_root)
        .to_path_buf();
    let manifest_parent_project = project_manifest
        .parent()
        .unwrap_or(project_root)
        .to_path_buf();
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    match adapter_lower {
        "axum" => {
            // Axum's synthesised axum.toml + the .edgezero/.env file
            // provision writes for the runtime store->platform-name
            // map. The manifest lives at the resolved
            // `[adapters.axum.adapter].manifest` path (or default
            // `axum.toml` if unset -- see `default_adapter_manifest_for`).
            pairs.push((project_manifest.clone(), adapter_manifest_abs.to_path_buf()));
            pairs.push((
                project_root.join(".edgezero/.env"),
                staged_root.join(".edgezero/.env"),
            ));
        }
        "cloudflare" => {
            pairs.push((project_manifest.clone(), adapter_manifest_abs.to_path_buf()));
            pairs.push((
                manifest_parent_project.join(".dev.vars"),
                manifest_parent_staged.join(".dev.vars"),
            ));
        }
        "fastly" => {
            pairs.push((project_manifest, adapter_manifest_abs.to_path_buf()));
        }
        "spin" => {
            pairs.push((project_manifest.clone(), adapter_manifest_abs.to_path_buf()));
            pairs.push((
                manifest_parent_project.join("runtime-config.toml"),
                manifest_parent_staged.join("runtime-config.toml"),
            ));
            pairs.push((
                manifest_parent_project.join(".env"),
                manifest_parent_staged.join(".env"),
            ));
        }
        _ => {}
    }
    DryRunAllowList { pairs }
}

/// Per-adapter default manifest filename. Fallback for when
/// `[adapters.<name>.adapter].manifest` is unset. Mirrors each
/// adapter crate's default.
pub(crate) fn default_adapter_manifest_for(adapter_lower: &str) -> &'static str {
    match adapter_lower {
        "axum" => "axum.toml",
        "cloudflare" => "wrangler.toml",
        "fastly" => "fastly.toml",
        "spin" => "spin.toml",
        _ => "",
    }
}

/// Render the dry-run report: rewritten status lines + per-file
/// unified diff. Status-line rewriting (`wrote X` → `would write X`)
/// uses only the (`project_root`, `staged_root`) prefix swap plus a
/// verb-prefix table.
pub(crate) fn render_dry_run_report(
    project_root: &Path,
    staged_root: &Path,
    allow_list: &DryRunAllowList,
    outcome: &adapter_registry::ProvisionOutcome,
) -> String {
    let mut out = String::new();

    // Status lines: rewrite staged-tempdir paths back to project-
    // relative AND prefix each verb with "would ".
    for line in &outcome.status_lines {
        let rewritten = line.replace(
            staged_root.to_string_lossy().as_ref(),
            project_root.to_string_lossy().as_ref(),
        );
        let with_verb = rewritten
            .replacen("wrote ", "would write ", 1)
            .replacen("created ", "would create ", 1)
            .replacen("appended ", "would append ", 1);
        out.push_str(&with_verb);
        out.push('\n');
    }

    // Per-file diff section: caller-provided pairs already resolved
    // (project_path, staged_path).
    for (proj_path, staged_path) in &allow_list.pairs {
        if !staged_path.exists() {
            continue;
        }
        let new = fs::read_to_string(staged_path).unwrap_or_default();
        let old = fs::read_to_string(proj_path).unwrap_or_default();
        if old == new {
            continue;
        }
        // Env / secret carriage files (`.env`, `.dev.vars`) hold
        // operator-authored secret values. `context_radius(2)` scopes
        // context to two lines around each hunk, but if provision
        // appends new lines at EOF the LAST two pre-existing lines
        // still surface as unchanged context — which for a
        // `.dev.vars` populated with real production secrets means
        // those secrets show up in CI logs, screen recordings, and
        // terminal scrollback. Redact both sides through a
        // `KEY=<redacted>` filter for env-shaped paths BEFORE
        // computing the diff. Comment lines and blank lines pass
        // through as-is so structural drift is still visible.
        let is_env_like = is_env_secret_carriage_path(proj_path);
        let old_render = if is_env_like {
            redact_env_body_for_diff(&old)
        } else {
            old.clone()
        };
        let new_render = if is_env_like {
            redact_env_body_for_diff(&new)
        } else {
            new.clone()
        };
        if old_render == new_render {
            // Diff was purely value churn on secret files. Emit a
            // single line so the operator knows something changed
            // without leaking the specific keys or values.
            let path_display = proj_path.display();
            out.push('\n');
            out.push_str("--- ");
            out.push_str(&path_display.to_string());
            out.push_str("\n+++ ");
            out.push_str(&path_display.to_string());
            out.push_str("\n@@ (redacted: env / secret carriage file -- values differ) @@\n");
            continue;
        }
        let diff = TextDiff::from_lines(&old_render, &new_render);
        let path_display = proj_path.display().to_string();
        out.push('\n');
        out.push_str(
            &diff
                .unified_diff()
                .context_radius(2)
                .header(&path_display, &path_display)
                .to_string(),
        );
    }
    out
}

/// True when the path is a secret-carriage env file (`.env`,
/// `.dev.vars`). Match on file NAME only so nested paths like
/// `.edgezero/.env` and `<spin_crate>/.env` are both covered.
fn is_env_secret_carriage_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some(".env" | ".dev.vars")
    )
}

/// Rewrite each `KEY=value` line in an env-shaped body as
/// `KEY=<redacted>`, preserving comment and blank lines verbatim.
/// Used by `render_dry_run_report` before diffing so a `.env` or
/// `.dev.vars` never surfaces operator secrets as context in the
/// unified-diff output. Structural changes (added/removed KEYS,
/// added comment lines) still show up as normal +/- hunks.
fn redact_env_body_for_diff(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        // Split off the trailing newline (if any) via char_indices
        // so we never slice inside a multi-byte codepoint. Env-file
        // content is normally 7-bit ASCII, but redaction of a
        // hand-authored operator note that includes UTF-8 must not
        // panic.
        let (content, tail) = match line.rfind('\n') {
            Some(idx) => line.split_at(idx),
            None => (line, ""),
        };
        if content.is_empty() || content.trim_start().starts_with('#') {
            out.push_str(line);
            continue;
        }
        if let Some((key, _value)) = content.split_once('=') {
            out.push_str(key);
            out.push('=');
            out.push_str("<redacted>");
            out.push_str(tail);
        } else {
            // Malformed line without `=`; emit as-is so the operator
            // sees the structural anomaly. Not a secret leak: any
            // real `.env` file uses `KEY=VALUE`, and a stray line
            // that isn't `KEY=` shape usually can't hold a secret.
            out.push_str(line);
        }
    }
    out
}

/// The `(true, true)` dispatch arm: synthesise the baseline manifest
/// (bytes only — no I/O against the real worktree), then stage the
/// adapter crate + `.edgezero/` + `edgezero.toml` into a tempdir and
/// run validate + build stores + dispatch entirely against the staged
/// root. The real worktree is never mutated.
///
/// The synthesiser runs against the REAL `manifest_root` because it
/// produces bytes only; every subsequent filesystem-touching call
/// (`write_baseline_to_disk`, `validate_adapter_manifest`,
/// `build_stores_against`, `adapter.provision`) receives
/// `staged_root` from inside the `run_with_staging` closure.
fn run_local_dry_run(
    adapter: &'static dyn adapter_registry::Adapter,
    manifest: &Manifest,
    adapter_cfg: &ManifestAdapter,
    manifest_root: &Path,
    args: &ProvisionArgs,
    app_name: &str,
    deployed: Option<&AdapterDeployedState>,
) -> Result<adapter_registry::ProvisionOutcome, String> {
    let baseline_pairs = adapter.synthesise_baseline_manifest(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
        app_name,
        deployed,
    )?;
    let adapter_crate_rel = adapter_cfg
        .adapter
        .crate_path
        .as_deref()
        .map_or_else(|| Path::new("."), Path::new);
    let (outcome, tempdir) = run_with_staging(
        manifest_root,
        adapter_crate_rel,
        |staged_root, _staged_crate| {
            // Errors surfaced from within the staging closure carry
            // raw `/var/folders/.../edgezero-staging-*` paths in the
            // formatted messages. Rewrite those back to the project
            // root's display form BEFORE bubbling -- spec §"Dry-run":
            // stdout must NEVER contain raw tempdir paths, in either
            // the success (status_lines) or error path.
            let staged_str = staged_root.to_string_lossy().into_owned();
            let project_str = manifest_root.to_string_lossy().into_owned();
            let sanitize = |err: String| err.replace(&staged_str, &project_str);
            let synthesised =
                write_baseline_to_disk(staged_root, &baseline_pairs).map_err(sanitize)?;
            adapter
                .validate_adapter_manifest(
                    staged_root,
                    adapter_cfg.adapter.manifest.as_deref(),
                    adapter_cfg.adapter.component.as_deref(),
                )
                .map_err(sanitize)?;
            let owned_stores =
                build_stores_against(staged_root, args, adapter, manifest).map_err(sanitize)?;
            // Spec §"Dry-run" step 3: pass `dry_run = false` to the
            // adapter even though `args.dry_run == true`. The tempdir
            // IS the dry-run mechanism — the adapter takes its real
            // write branch against the staged tree so operators can
            // preview the actual files that would land. If we passed
            // `true`, adapters would early-return from their dry-run
            // branches (cloudflare cli.rs:263, spin cli.rs:223) and
            // leave the staged tree empty of the content the diff
            // report is supposed to show.
            let outcome = adapter
                .provision(
                    staged_root,
                    adapter_cfg.adapter.manifest.as_deref(),
                    adapter_cfg.adapter.component.as_deref(),
                    &owned_stores.as_refs(),
                    deployed,
                    adapter_registry::ProvisionMode::Local,
                    false,
                )
                .map_err(sanitize)?;
            // Prepend baseline-write lines with staged paths rewritten
            // back to project-relative form (spec §"Dry-run": stdout
            // must NEVER carry raw tempdir paths).
            Ok(prepend_baseline_status_lines_with_rewrite(
                &args.adapter,
                &synthesised,
                outcome,
                |path| {
                    path.display()
                        .to_string()
                        .replace(&staged_str, &project_str)
                },
            ))
        },
    )?;

    let staged_root = tempdir.path();
    let adapter_lower = args.adapter.to_lowercase();
    let adapter_manifest_rel = adapter_cfg
        .adapter
        .manifest
        .as_deref()
        .unwrap_or_else(|| default_adapter_manifest_for(&adapter_lower));
    let adapter_manifest_staged = staged_root.join(adapter_manifest_rel);
    let allow_list = build_dry_run_allow_list(
        manifest_root,
        staged_root,
        &adapter_lower,
        &adapter_manifest_staged,
    );
    let report = render_dry_run_report(manifest_root, staged_root, &allow_list, &outcome);
    // Only emit the report if it's non-empty (avoids extraneous blank
    // log lines when the adapter status_lines are empty and no
    // allow-list file differs).
    if !report.is_empty() {
        log::info!("{report}");
    }
    // Clear status_lines: the sanitized report already includes the
    // rewritten "would write ..." lines with staged-tempdir paths
    // swapped back to project-relative form. If we returned them
    // untouched, `run_provision`'s trailing `for line in
    // outcome.status_lines` loop would re-log the raw versions —
    // leaking `/var/folders/…` tempdir paths to operators. Spec §"Dry-
    // run": stdout must NEVER contain raw tempdir paths. The
    // `deployed` payload is intentionally kept: cloud writeback under
    // (false, _) uses it, and local dry-run today always sees `None`
    // there (adapters populate `deployed` only when writing real
    // cloud state).
    Ok(match outcome.deployed {
        Some(state) => adapter_registry::ProvisionOutcome::with_deployed(Vec::new(), state),
        None => adapter_registry::ProvisionOutcome::from_status_lines(Vec::new()),
    })
}

/// Stage a real recursive copy of the adapter crate dir AND the
/// `.edgezero/` dir (if present) under a fresh `TempDir`, then invoke
/// `body` with the staged paths. The original project worktree is
/// never mutated. Caller is responsible for diffing the staged tree
/// against the project tree before the returned `TempDir` drops. See
/// spec §"Dry-run".
///
/// Called by the `(true, true)` arm of the `run_provision` dispatch
/// matrix — local dry-run stages everything into a tempdir and
/// discards it after `body` completes.
pub(crate) fn run_with_staging<F, R>(
    project_root: &Path,
    adapter_crate_rel: &Path,
    body: F,
) -> Result<(R, tempfile::TempDir), String>
where
    F: FnOnce(&Path, &Path) -> Result<R, String>,
{
    let tempdir = tempfile::TempDir::new()
        .map_err(|err| format!("failed to create staging tempdir: {err}"))?;
    let staged_root = tempdir.path();

    // Copy `edgezero.toml` (read-only input). Symlinking would be
    // tempting as an optimisation, but for the default
    // `--manifest edgezero.toml` shape `project_root` is "." and
    // `project_root.join("edgezero.toml")` is `./edgezero.toml`.
    // Unix `symlink(src, dst)` interprets a relative `src` as
    // relative to `dst`'s parent — so
    // `staged_root/edgezero.toml -> ./edgezero.toml` resolves back
    // to `staged_root/edgezero.toml` itself, a broken self-loop.
    // Copying is small and correct.
    let edgezero_toml = project_root.join("edgezero.toml");
    if edgezero_toml.exists() {
        let staged_edgezero = staged_root.join("edgezero.toml");
        if let Some(parent) = staged_edgezero.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create staged parent dir: {err}"))?;
        }
        fs::copy(&edgezero_toml, &staged_edgezero)
            .map_err(|err| format!("failed to stage edgezero.toml: {err}"))?;
    }

    // Real-copy the adapter crate dir (mutable). `adapter_crate_rel`
    // is project-relative (e.g. "crates/cf" or "."), so no
    // `strip_prefix` is needed — the earlier draft that computed
    // `crate_rel` via `strip_prefix(project_root)` silently failed for
    // the default `project_root == "."` shape.
    let src_crate = project_root.join(adapter_crate_rel);
    let staged_crate = staged_root.join(adapter_crate_rel);
    copy_dir_recursive(&src_crate, &staged_crate)
        .map_err(|err| format!("failed to stage adapter crate dir: {err}"))?;

    // Real-copy `.edgezero/` if present; otherwise create empty. Some
    // adapters own `.edgezero/local-config-*.json` state files (axum);
    // staging must preserve them, and their absence in a green-clone
    // case must still yield a mountable dir.
    let dot_edgezero = project_root.join(".edgezero");
    let staged_dot = staged_root.join(".edgezero");
    if dot_edgezero.exists() {
        copy_dir_recursive(&dot_edgezero, &staged_dot)
            .map_err(|err| format!("failed to stage .edgezero/: {err}"))?;
    } else {
        fs::create_dir_all(&staged_dot)
            .map_err(|err| format!("failed to create staged .edgezero/: {err}"))?;
    }

    let result = body(staged_root, &staged_crate)?;
    Ok((result, tempdir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ProvisionArgs;
    use crate::test_support::{manifest_guard, EnvOverride, PROVISION_MANIFEST};
    use edgezero_adapter::registry::{
        get_adapter, register_adapter, Adapter, AdapterAction, ProvisionMode, ProvisionOutcome,
    };
    use edgezero_core::app_config::{SecretField, SecretKind};
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ----- fixtures for CLI-owned first-run bootstrap synthesis -----
    //
    // A distinct fake adapter (`__test_bootstrap_fake__`) is
    // registered per-test into the global adapter registry via the
    // public `register_adapter` helper. The `manifest_guard()` mutex
    // already serialises tests that touch the registry, so a
    // second registration under the same name from a concurrent
    // test cannot race. Observability is via two module-scope
    // `AtomicBool` flags — `SYNTH_CALLED` for the synthesiser call
    // and `VALIDATE_SAW_FILE` for the downstream
    // `validate_adapter_manifest` invariant.
    //
    // The fake echoes `adapter_manifest_path` back as the
    // synthesised file's relative path, mirroring the Spin override
    // that lands at Task 24 — the file must land at
    // `<root>/<adapter_cfg.adapter.manifest>`, NOT at a hard-coded
    // path.

    const FAKE_MANIFEST_BODY: &str = r#"
[app]
name = "demo-app"

[adapters.__test_bootstrap_fake__.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.__test_bootstrap_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;

    static FAKE_ADAPTER: FakeBootstrapAdapter = FakeBootstrapAdapter;
    static NO_FIELDS_FAKE_ADAPTER: NoFieldsFakeAdapter = NoFieldsFakeAdapter;
    static RECORDED_DRY_RUN: AtomicBool = AtomicBool::new(false);
    // Captures the `deployed` argument the CLI passes into
    // `FakeBootstrapAdapter::synthesise_baseline_manifest`. Used by
    // Section-4 tests that assert `deployed_state_for` translated a
    // real `[adapters.<name>.deployed]` block and threaded it
    // through — not left it silently `None`.
    static RECORDED_SYNTH_DEPLOYED: Mutex<Option<AdapterDeployedState>> = Mutex::new(None);
    // Task 29: captures the `TypedSecretEntry` slice the CLI passes
    // into `FakeBootstrapAdapter::provision_typed`. Recorded as
    // `(store_id, field_name, key_value)` triples because the entry
    // itself borrows from the `ValidationContext`'s raw config; the
    // owned form outlives the closure so tests can read it back.
    static RECORDED_TYPED_ENTRIES: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());
    static SYNTH_CALLED: AtomicBool = AtomicBool::new(false);
    static VALIDATE_SAW_FILE: AtomicBool = AtomicBool::new(false);

    /// RAII guard: on `set`, chdir into `new_cwd`; on drop, restore
    /// the previous cwd. Callers MUST hold `manifest_guard()` while
    /// this is live — process cwd is global state and can only be
    /// mutated safely under that serialisation lock.
    struct CwdGuard(PathBuf);

    struct FakeBootstrapAdapter;

    struct NoFieldsFakeAdapter;

    impl CwdGuard {
        fn set(new_cwd: &Path) -> io::Result<Self> {
            let prev = env::current_dir()?;
            env::set_current_dir(new_cwd)?;
            Ok(Self(prev))
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            // Best-effort cwd restore during unwind or normal drop.
            // A failure here is unrecoverable at the drop site; the
            // manifest_guard() lock the caller holds is released
            // regardless, so the next test acquiring it will
            // set_current_dir explicitly if it needs to.
            drop(env::set_current_dir(&self.0));
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "the fake overrides name/deployed_fields/provision/provision_typed/single_store_kinds/synthesise_baseline_manifest/validate_adapter_manifest; every other trait method inherits its default (no-op or Unsupported)"
    )]
    impl Adapter for FakeBootstrapAdapter {
        fn deployed_fields(&self) -> &'static [&'static str] {
            &["service_id"]
        }

        fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
            Ok(())
        }

        fn name(&self) -> &'static str {
            "__test_bootstrap_fake__"
        }

        fn provision(
            &self,
            _manifest_root: &Path,
            _adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            _stores: &ProvisionStores<'_>,
            _deployed: Option<&AdapterDeployedState>,
            _mode: ProvisionMode,
            dry_run: bool,
        ) -> Result<ProvisionOutcome, String> {
            RECORDED_DRY_RUN.store(dry_run, Ordering::SeqCst);
            Ok(ProvisionOutcome::default())
        }

        fn provision_typed(
            &self,
            _manifest_root: &Path,
            _adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            typed_secrets: &[TypedSecretEntry<'_>],
            _mode: ProvisionMode,
            _dry_run: bool,
        ) -> Result<ProvisionOutcome, String> {
            if let Ok(mut slot) = RECORDED_TYPED_ENTRIES.lock() {
                slot.clear();
                slot.extend(typed_secrets.iter().map(|entry| {
                    (
                        entry.store_id.to_owned(),
                        entry.field_name.to_owned(),
                        entry.key_value.to_owned(),
                    )
                }));
            }
            Ok(ProvisionOutcome::default())
        }

        fn single_store_kinds(&self) -> &'static [&'static str] {
            // The fake advertises `secrets` as Single-capable so the
            // Task 29 capability-gate test can drive
            // `enforce_single_store_capability` without leaning on a
            // real adapter's registration. Existing fake fixtures
            // declare zero or one secret id, so this override does
            // not regress the other test cases.
            &["secrets"]
        }

        fn synthesise_baseline_manifest(
            &self,
            _manifest_root: &Path,
            adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            _app_name: &str,
            deployed: Option<&AdapterDeployedState>,
        ) -> Result<Vec<(PathBuf, String)>, String> {
            SYNTH_CALLED.store(true, Ordering::SeqCst);
            if let Ok(mut slot) = RECORDED_SYNTH_DEPLOYED.lock() {
                *slot = deployed.cloned();
            }
            let rel = adapter_manifest_path.unwrap_or("spin.toml").to_owned();
            Ok(vec![(PathBuf::from(rel), "# stub\n".to_owned())])
        }

        fn validate_adapter_manifest(
            &self,
            manifest_root: &Path,
            adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
        ) -> Result<(), String> {
            // The synthesised file MUST exist by the time validate
            // runs — that's the invariant this whole task guards.
            let rel = adapter_manifest_path.unwrap_or("spin.toml");
            let abs = manifest_root.join(rel);
            if abs.exists() {
                VALIDATE_SAW_FILE.store(true, Ordering::SeqCst);
                Ok(())
            } else {
                Err(format!(
                    "fake validate: {} missing at validate time",
                    abs.display()
                ))
            }
        }
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "the no-fields fake overrides execute/name/provision (required by the trait) and inherits every defaulted method — including deployed_fields, whose default `&[]` is the intent this fake exercises"
    )]
    impl Adapter for NoFieldsFakeAdapter {
        fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
            Ok(())
        }

        fn name(&self) -> &'static str {
            "__test_no_fields_fake__"
        }

        fn provision(
            &self,
            _manifest_root: &Path,
            _adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            _stores: &ProvisionStores<'_>,
            _deployed: Option<&AdapterDeployedState>,
            _mode: ProvisionMode,
            _dry_run: bool,
        ) -> Result<ProvisionOutcome, String> {
            Ok(ProvisionOutcome::default())
        }
    }

    fn reset_fake_state() {
        RECORDED_DRY_RUN.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = RECORDED_SYNTH_DEPLOYED.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = RECORDED_TYPED_ENTRIES.lock() {
            slot.clear();
        }
        SYNTH_CALLED.store(false, Ordering::SeqCst);
        VALIDATE_SAW_FILE.store(false, Ordering::SeqCst);
    }

    /// Walks the tree at `root` and returns a sorted `Vec<(relative
    /// path, content bytes)>`. Two calls yield equal `Vec`s iff the
    /// tree is byte-identical. Used by the dry-run cleanliness
    /// assertion — any staging leak that writes into the worktree
    /// flips one of the pairs.
    fn snapshot_dir(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        snapshot_dir_excluding(root, &[])
    }

    /// Same as `snapshot_dir` but skips directories whose file name
    /// matches an entry in `excluded_dir_names` (at any depth).
    /// Used by the app-demo dry-run test: `examples/app-demo/target`
    /// alone is tens of gigabytes of Rust build output whose contents
    /// are irrelevant to "did dry-run mutate the worktree" — reading
    /// them into memory would time the test out with no signal gain.
    fn snapshot_dir_excluding(root: &Path, excluded_dir_names: &[&str]) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        snapshot_walk(root, root, excluded_dir_names, &mut out).expect("snapshot walk");
        out.sort_by(|left, right| left.0.cmp(&right.0));
        out
    }

    fn snapshot_walk(
        base: &Path,
        dir: &Path,
        excluded_dir_names: &[&str],
        out: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> io::Result<()> {
        for read_result in fs::read_dir(dir)? {
            let entry = read_result?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name();
                if excluded_dir_names
                    .iter()
                    .any(|excluded| name.as_os_str() == *excluded)
                {
                    continue;
                }
                snapshot_walk(base, &path, excluded_dir_names, out)?;
            } else if !file_type.is_symlink() {
                // Regular files only — symlinks are intentionally
                // skipped so the snapshot mirrors `copy_dir_recursive`'s
                // regular-files-only semantics.
                let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
                let content = fs::read(&path)?;
                out.push((rel, content));
            } else {
                // Symlink — skip.
            }
        }
        Ok(())
    }

    #[test]
    fn run_provision_cloud_non_dry_run_succeeds_when_adapter_is_side_effect_free() {
        // Cloud non-dry-run smoke: the CLI dispatch matrix reaches
        // the adapter and exits 0 when the adapter's Cloud arm has no
        // side effects to perform. Uses axum because its Cloud
        // provision is a no-op; any adapter with an empty Cloud arm
        // would fit — the assertion is about the CLI's dispatch,
        // not axum-specific behavior. Stronger dispatch-shape
        // coverage (which dry_run value the adapter observes, which
        // arm of the (local, dry_run) matrix runs) is in the
        // fake-based tests: provision_cloud_dry_run_passes_dry_run
        // _true_to_adapter, provision_local_no_dry_run_writes_to
        // _worktree, provision_local_dry_run_leaves_worktree_clean.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: false,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("side-effect-free adapter cloud provision exits 0");
    }

    #[test]
    fn run_provision_errors_on_unknown_adapter() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let err = run_provision(&ProvisionArgs {
            adapter: "wat".to_owned(),
            dry_run: false,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("unknown adapter must error");
        assert!(
            err.contains("wat"),
            "error should name the unknown adapter: {err}"
        );
    }

    #[test]
    fn run_provision_rejects_malformed_handler_path_before_dispatching() {
        // Provision is the most expensive operation in the CLI --
        // it can create real platform resources. A trigger handler
        // path that isn't a well-formed Rust `module::function`
        // must fail HERE, not after the remote create succeeded.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[[triggers.http]]
path = "/"
methods = ["GET"]
handler = "not a valid path"
adapters = ["axum"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("malformed handler must error before dispatch");
        assert!(
            err.contains("handler") && err.contains("Rust path"),
            "error names handler + Rust-path hint: {err}"
        );
    }

    #[test]
    fn run_provision_spin_rejects_malformed_adapter_manifest_before_dispatching() {
        // CLI-logic test: `run_provision` MUST call
        // `adapter.validate_adapter_manifest` before dispatch, and
        // MUST surface its error. Spin is the illustrative example
        // because its `validate_adapter_manifest` actually validates
        // (a spin.toml with zero components errors); axum's is a
        // no-op so it can't drive this assertion. The check itself
        // is CLI-side, not Spin-specific.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        // spin.toml with NO [component.*] table. PROVISION_MANIFEST
        // declares Spin's manifest at `crates/demo-spin/spin.toml`
        // (nested inside the crate dir per the 2026-07 strict-local
        // containment requirement).
        let spin_manifest = temp.path().join("crates/demo-spin/spin.toml");
        fs::create_dir_all(spin_manifest.parent().unwrap()).expect("mkdir demo-spin");
        fs::write(
            &spin_manifest,
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .expect("write empty spin.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let err = run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("zero-component spin.toml must error pre-dispatch");
        assert!(
            err.contains("component") || err.contains("spin"),
            "error names the manifest shape problem: {err}"
        );
    }

    #[test]
    fn run_provision_spin_rejects_multi_secret_ids_via_capability_gate() {
        // CLI-logic test: the capability gate
        // (`enforce_single_store_capability`) reads the adapter's
        // `merged_id_kinds()` and rejects manifests declaring more
        // than one id in a single-capable kind. Spin is the
        // illustrative example because secrets remain Single-capable
        // there while `config` moved to KV; any adapter with a
        // Single-capable kind + a manifest exceeding that limit would
        // fit. Pins parity with `config validate --strict`.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]
default = "app_config"

[stores.secrets]
ids = ["default", "other"]
default = "default"
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(
            temp.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let err = run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("Single-capability violation must error");
        assert!(
            err.contains("spin") && err.contains("Single-capable for secrets"),
            "error names the adapter + kind: {err}"
        );
    }

    #[test]
    fn run_provision_spin_accepts_multi_config_ids_since_kv_migration() {
        // CLI-logic test: the capability gate accepts multiple ids
        // when the adapter's `merged_id_kinds()` includes the kind.
        // Spin is the illustrative example because its `config` kind
        // is KV-backed (multi-capable) post-migration; any adapter
        // with a multi-capable kind would fit.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config", "other_config"]
default = "app_config"

[stores.secrets]
ids = ["default"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(
            temp.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("multi-config dispatch must succeed under KV-backed config");
    }

    #[test]
    fn run_provision_spin_rejects_env_overlay_platform_label_collision_across_kv_and_config() {
        // CLI-logic test: `reject_merged_id_collisions` catches
        // env-overlay collisions where distinct logical ids across
        // merged kinds resolve to the same platform label. Spin is
        // the illustrative example because it merges kv + config
        // into a single KV backend (any adapter whose
        // `merged_id_kinds()` covers 2+ kinds would fit). Pins parity
        // with the same check `config validate` runs.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.kv]
ids = ["sessions"]

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(
            temp.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let _kv_override = EnvOverride::set("EDGEZERO__STORES__KV__SESSIONS__NAME", "shared");
        let _config_override =
            EnvOverride::set("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME", "shared");

        let err = run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("env-overlay platform-label collision must fail provision");
        assert!(
            err.contains("`shared`")
                && err.contains("[stores.kv].sessions")
                && err.contains("[stores.config].app_config"),
            "error names the resolved platform label + both logical ids: {err}"
        );
    }

    /// End-to-end lock for spec §"env-overlay precedence": the
    /// `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME` env var MUST flow
    /// through `EnvConfig::store_name()` at `provision.rs::build_stores`
    /// into every adapter's writeback so the PLATFORM name in the
    /// emitted file reflects the override, not the raw logical id.
    ///
    /// Every adapter-level "env-overlay round-trip" test constructs
    /// `ResolvedStoreId` directly and bypasses this resolver, so a
    /// refactor that drops the `env_config.store_name(kind, id)` call
    /// would silently write logical names into every adapter manifest
    /// and every adapter test still passes. This test locks the
    /// integration by driving `run_provision` all the way from env
    /// var → CLI orchestration → adapter → emitted file.
    ///
    /// Uses the axum adapter because it writes a single deterministic
    /// line-oriented file (`.edgezero/.env`) with no vendor CLI
    /// dependency.
    #[test]
    fn run_provision_local_flows_env_overlay_platform_name_into_emitted_file() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        // The env overlay: for logical id `sessions` in KV, resolve
        // the platform NAME to `custom_kv_platform`. Provision must
        // pick this up via `EnvConfig::store_name()` and write it
        // into `.edgezero/.env`.
        let _kv_override =
            EnvOverride::set("EDGEZERO__STORES__KV__SESSIONS__NAME", "custom_kv_platform");

        run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("local provision succeeds");

        let env_path = temp.path().join(".edgezero").join(".env");
        let env_contents = fs::read_to_string(&env_path).expect("read .edgezero/.env");
        assert!(
            env_contents.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=custom_kv_platform"),
            "overlay-resolved platform name must reach .edgezero/.env: {env_contents}"
        );
        // Negative: the raw logical id must NOT appear as the platform
        // value (that would mean the resolver was bypassed).
        assert!(
            !env_contents.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            ".env still carries the un-overridden logical id — the env overlay was bypassed: {env_contents}"
        );
    }

    #[test]
    fn run_provision_skips_capability_gate_for_kinds_within_single_id_floor() {
        // Sanity: the capability gate fires ONLY when ids.len() > 1.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let spin_manifest = temp.path().join("crates/demo-spin/spin.toml");
        fs::create_dir_all(spin_manifest.parent().unwrap()).expect("mkdir demo-spin");
        fs::write(
            &spin_manifest,
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("single-id case dispatches cleanly");
    }

    // ---------- provision --local path containment ----------

    #[test]
    fn provision_local_rejects_parent_traversal_in_adapter_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "../outside/spin.toml"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        // Canary: the traversal target resolves to `<parent>/outside/spin.toml`.
        // If path-safety fires before dispatch, this path must not exist after.
        let outside_dir = temp
            .path()
            .parent()
            .expect("tempdir has parent")
            .join("outside");
        assert!(!outside_dir.exists(), "sentinel: outside/ absent pre-call");

        let err = run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("parent traversal in adapter manifest must be rejected");
        assert!(
            err.contains("must not contain `..` traversal"),
            "error must name the traversal violation: {err}"
        );
        assert!(
            !outside_dir.exists(),
            "sentinel: outside/ still absent after call (dispatch did not fire)"
        );
    }

    #[test]
    fn provision_local_rejects_absolute_adapter_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        // Use a path inside a fresh tempdir subtree so we can prove
        // it stays absent even though nothing outside the test would
        // reasonably poke it: /tmp/some.toml would be a shared name.
        let outside_root = TempDir::new().expect("outside temp dir");
        let outside_abs = outside_root.path().join("some.toml");
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/demo-spin"
manifest = "{}"
[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#,
            outside_abs.display()
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        assert!(
            !outside_abs.exists(),
            "sentinel: absolute path absent pre-call"
        );

        let err = run_provision(&ProvisionArgs {
            adapter: "spin".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("absolute adapter manifest path must be rejected");
        assert!(
            err.contains("must be a project-relative path"),
            "error must name the absolute-path violation: {err}"
        );
        assert!(
            !outside_abs.exists(),
            "sentinel: absolute path still absent after call"
        );
    }

    #[test]
    fn provision_local_rejects_parent_traversal_in_adapter_crate() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "../escape"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let escape_dir = temp
            .path()
            .parent()
            .expect("tempdir has parent")
            .join("escape");
        assert!(!escape_dir.exists(), "sentinel: escape/ absent pre-call");

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("parent traversal in adapter crate must be rejected");
        assert!(
            err.contains("must not contain `..` traversal"),
            "error must name the traversal violation: {err}"
        );
        assert!(
            !escape_dir.exists(),
            "sentinel: escape/ still absent after call"
        );
    }

    #[test]
    fn provision_local_accepts_relative_manifest_root_default() {
        // Bare `--manifest edgezero.toml` — `args.manifest.parent()`
        // returns "", triggering the `.unwrap_or_else(|| Path::new("."))`
        // fallback. To reach that fallback we must actually load
        // edgezero.toml relative to cwd, so chdir into a tempdir
        // that holds a valid fixture. The `_cwd` RAII guard restores
        // the previous cwd on drop; `_lock` serialises all cwd + env
        // manipulation.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        fs::write(temp.path().join("edgezero.toml"), PROVISION_MANIFEST).expect("write manifest");
        // Task 11 wires the (true, true) arm through `run_with_staging`,
        // which recursively copies the adapter crate dir into the
        // staging tempdir. The fixture must pre-create the crate dir
        // referenced by PROVISION_MANIFEST or staging errors before
        // dispatch reaches axum's Local arm.
        fs::create_dir_all(temp.path().join("crates/demo-axum")).expect("create adapter crate dir");
        let _cwd = CwdGuard::set(temp.path()).expect("chdir into tempdir");

        // Task 27: axum's Local arm now succeeds (writes .env into a
        // `.edgezero/` under `manifest_root`). This test used to
        // sentinel on the Section-5 stub's error; the equivalent
        // positive signal is a status line that names axum's Local
        // outcome. Reaching THAT line proves the manifest loaded,
        // path-safety passed, AND `run_with_staging` routed the
        // closure through validate + build_stores + provision.
        run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: PathBuf::from("edgezero.toml"),
        })
        .expect("axum's Local arm succeeds through the staged dispatch");
    }

    #[test]
    fn provision_local_accepts_relative_manifest_root_nested() {
        // Nested `--manifest <tempdir>/edgezero.toml` — parent is the
        // tempdir path (non-empty), exercising the standard
        // `args.manifest.parent()` code path.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        fs::create_dir_all(temp.path().join("crates/demo-axum")).expect("create adapter crate dir");

        // Task 27: same successful-Local-arm sentinel as the "_default"
        // sibling above.
        run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("axum's Local arm succeeds through the staged dispatch");
    }

    // ---------- CLI-owned first-run bootstrap synthesis ----------

    #[test]
    fn provision_local_synthesises_missing_adapter_manifest_before_validation() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        // Fixture: crates/spin/ exists but spin.toml does NOT.
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("local provision synthesises baseline then validates");

        assert!(
            SYNTH_CALLED.load(Ordering::SeqCst),
            "synthesiser must fire in local mode"
        );
        assert!(
            VALIDATE_SAW_FILE.load(Ordering::SeqCst),
            "validate must see synthesised file"
        );
        let synth_path = temp.path().join("crates/spin/spin.toml");
        let synth = fs::read_to_string(&synth_path)
            .expect("synthesised file lands under the configured adapter manifest path");
        assert!(
            synth.contains("# stub"),
            "synthesised file contains the fake payload: {synth}"
        );
        // Regression guard: the synthesised file must NOT land at
        // <root>/spin.toml — that path is what the earlier "hard-code
        // spin.toml" shape produced.
        assert!(
            !temp.path().join("spin.toml").exists(),
            "sentinel: synthesis must not write to a hard-coded root-level path"
        );
    }

    #[test]
    fn provision_local_bootstrap_is_a_no_op_when_manifest_already_present() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");
        // Operator-authored content that must survive.
        let operator_body = "# operator-authored\n";
        fs::write(temp.path().join("crates/spin/spin.toml"), operator_body)
            .expect("write operator manifest");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("provision passes when manifest already exists");

        // The synthesiser fires, but write_baseline_to_disk skips
        // existing files, so operator content survives byte-for-byte.
        let after = fs::read_to_string(temp.path().join("crates/spin/spin.toml"))
            .expect("existing spin.toml still readable");
        assert_eq!(
            after, operator_body,
            "existing operator-authored file must remain byte-for-byte unchanged"
        );
    }

    #[test]
    fn provision_cloud_never_runs_bootstrap_synthesis() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        // crates/spin/ exists but spin.toml does NOT — validation must
        // therefore fail in cloud mode because bootstrap synthesis is
        // NOT invoked to fill the gap.
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);

        let err = run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect_err("cloud mode with missing adapter manifest must error at validate");
        assert!(
            err.contains("missing at validate time"),
            "error surfaces the missing-manifest failure: {err}"
        );
        assert!(
            !SYNTH_CALLED.load(Ordering::SeqCst),
            "synthesiser must NOT fire in cloud mode"
        );
    }

    // ---------- run_with_staging dry-run helper ----------

    #[test]
    fn run_with_staging_drops_tempdir_after_body() {
        let project = tempfile::TempDir::new().unwrap();
        fs::write(project.path().join("edgezero.toml"), "x").unwrap();
        let adapter_crate_rel = Path::new("crates/sample");
        let adapter_crate_abs = project.path().join(adapter_crate_rel);
        fs::create_dir_all(&adapter_crate_abs).unwrap();
        fs::write(adapter_crate_abs.join("manifest.toml"), "y").unwrap();

        let staged_paths = run_with_staging(
            project.path(),
            adapter_crate_rel,
            |staged_root, staged_crate| Ok((staged_root.to_path_buf(), staged_crate.to_path_buf())),
        )
        .unwrap();
        let (staged_root, staged_crate) = staged_paths.0;
        // After staging the original project tree is byte-identical:
        assert_eq!(
            fs::read_to_string(project.path().join("edgezero.toml")).unwrap(),
            "x"
        );
        // Staged copies existed during body execution:
        assert!(staged_root.is_absolute());
        assert!(staged_crate.starts_with(&staged_root));
    }

    #[test]
    fn run_with_staging_copies_edgezero_toml_into_staged_root() {
        // Regression for the relative-source-symlink bug AND the
        // strip_prefix bug (fixed by switching to project-RELATIVE
        // crate paths). Reads staged_root/edgezero.toml INSIDE the
        // closure and asserts the bytes match. Uses an ABSOLUTE
        // project_root to avoid mutating process cwd — the
        // strip_prefix bug is not about relative project_root
        // resolution itself, it's about the staging helper computing
        // crate_rel incorrectly.
        let project = tempfile::TempDir::new().unwrap();
        fs::write(project.path().join("edgezero.toml"), "real-project-bytes\n").unwrap();
        let adapter_crate_rel = Path::new("crates/sample");
        fs::create_dir_all(project.path().join(adapter_crate_rel)).unwrap();
        fs::write(
            project.path().join(adapter_crate_rel).join("manifest.toml"),
            "x",
        )
        .unwrap();

        let observed = run_with_staging(
            project.path(),
            adapter_crate_rel,
            |staged_root, _staged_crate| {
                fs::read_to_string(staged_root.join("edgezero.toml"))
                    .map_err(|err| format!("read staged edgezero.toml: {err}"))
            },
        )
        .unwrap();
        assert_eq!(observed.0, "real-project-bytes\n");
    }

    // ---------- (local, dry-run) dispatch matrix ----------

    #[test]
    fn provision_local_dry_run_leaves_worktree_clean() {
        // Snapshot the tempdir contents (relative path → content
        // bytes) before the call. Run run_provision with local=true,
        // dry_run=true. The axum adapter's Section-5 stub will Err
        // — that's fine. The core claim is: after the call, EVERY
        // file in the tempdir is byte-identical to its pre-call
        // snapshot. Any staging leak (a file written into the
        // worktree instead of the tempdir) would flip the assertion.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).unwrap();
        // Pre-create the axum adapter crate dir + a canary file so
        // the staging copy has real content to work with. This also
        // proves the crate dir itself is not clobbered.
        let axum_crate = temp.path().join("crates/demo-axum");
        fs::create_dir_all(&axum_crate).unwrap();
        fs::write(axum_crate.join("Cargo.toml"), "# stub").unwrap();

        let before = snapshot_dir(temp.path());

        // Ignore the Result — axum's Section-5 stub Errs today; the
        // core assertion is that the worktree is unchanged either way.
        // Explicit type annotation quiets `let_underscore_untyped`
        // and `let_underscore_must_use` — the Result is genuinely
        // irrelevant to the assertion below.
        let _result: Result<(), String> = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path,
        });

        let after = snapshot_dir(temp.path());
        assert_eq!(
            before, after,
            "dry-run must leave the worktree byte-identical"
        );
    }

    #[test]
    fn provision_local_no_dry_run_writes_to_worktree() {
        // Non-dry-run local mode DOES write. axum can't demonstrate
        // that until Section 5 lands, so use the fake adapter — its
        // synthesise_baseline_manifest returns a stub file at the
        // configured manifest path. In (true, false) mode, the CLI
        // calls write_baseline_to_disk which materialises that file
        // into the worktree before validate_adapter_manifest runs.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).unwrap();
        // Pre-create the crate dir the fake references so the
        // manifest validates.
        fs::create_dir_all(temp.path().join("crates/spin")).unwrap();
        let synthesised = temp.path().join("crates/spin/spin.toml");
        assert!(
            !synthesised.exists(),
            "pre-condition: synthesised file absent"
        );

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path,
        })
        .expect("(true, false) arm should succeed with the fake adapter");

        assert!(
            synthesised.exists(),
            "(true, false) arm must write synthesised baseline into the worktree"
        );
        let bytes = fs::read_to_string(&synthesised).expect("read synthesised file");
        assert!(
            bytes.contains("# stub"),
            "content should match fake's synthesiser output"
        );
    }

    // ---------- dry-run allow-list + report rendering ----------

    #[test]
    fn dry_run_status_lines_use_would_write_verb() {
        // Direct fn-under-test: call render_dry_run_report against a
        // synthetic ProvisionOutcome whose status_lines exercise all
        // three verbs the rewriter handles.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).expect("write manifest");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create crate dir");

        let outcome = ProvisionOutcome::from_status_lines(vec![
            "wrote crates/spin/spin.toml".to_owned(),
            "created .edgezero/.env".to_owned(),
            "appended crates/spin/.env".to_owned(),
        ]);
        let allow_list = DryRunAllowList { pairs: vec![] };
        let report = render_dry_run_report(temp.path(), temp.path(), &allow_list, &outcome);
        assert!(
            report.contains("would write crates/spin/spin.toml"),
            "verb-rewriting must turn 'wrote' into 'would write': {report}"
        );
        assert!(
            report.contains("would create .edgezero/.env"),
            "verb-rewriting must turn 'created' into 'would create': {report}"
        );
        assert!(
            report.contains("would append crates/spin/.env"),
            "verb-rewriting must turn 'appended' into 'would append': {report}"
        );
        // Negative: raw verbs must not survive
        assert!(!report.contains("\nwrote "), "raw 'wrote' must be gone");
        assert!(!report.contains("\ncreated "), "raw 'created' must be gone");
        assert!(
            !report.contains("\nappended "),
            "raw 'appended' must be gone"
        );
    }

    #[test]
    fn dry_run_diff_covers_all_allowlist_paths() {
        // Table-driven: for each adapter, build a fixture where the
        // allow-listed files exist in the staged tree, then call
        // render_dry_run_report and assert the printed diff section
        // mentions each expected path and excludes non-listed paths.
        let project = TempDir::new().expect("project");
        let staged = TempDir::new().expect("staged");

        // Cloudflare fixture: wrangler.toml + sibling .dev.vars
        fs::write(staged.path().join("wrangler.toml"), "name = \"cf\"\n").unwrap();
        fs::write(staged.path().join(".dev.vars"), "SECRET=abc\n").unwrap();
        let cf_allow = build_dry_run_allow_list(
            project.path(),
            staged.path(),
            "cloudflare",
            &staged.path().join("wrangler.toml"),
        );
        let cf_report = render_dry_run_report(
            project.path(),
            staged.path(),
            &cf_allow,
            &ProvisionOutcome::default(),
        );
        assert!(
            cf_report.contains("wrangler.toml"),
            "cloudflare must diff wrangler.toml: {cf_report}"
        );
        assert!(
            cf_report.contains(".dev.vars"),
            "cloudflare must diff .dev.vars: {cf_report}"
        );
        // Negative: axum-only paths must not appear
        assert!(
            !cf_report.contains("axum.toml"),
            "axum.toml must not appear in cloudflare diff"
        );

        // Axum fixture: axum.toml (provision-synthesised) + .edgezero/.env
        let axum_project = TempDir::new().expect("axum project");
        let axum_staged = TempDir::new().expect("axum staged");
        fs::create_dir_all(axum_staged.path().join(".edgezero")).unwrap();
        fs::write(axum_staged.path().join(".edgezero/.env"), "K=V\n").unwrap();
        fs::write(
            axum_staged.path().join("axum.toml"),
            "[adapter]\ncrate=\"demo\"\n",
        )
        .unwrap();
        let axum_allow = build_dry_run_allow_list(
            axum_project.path(),
            axum_staged.path(),
            "axum",
            &axum_staged.path().join("axum.toml"),
        );
        let axum_report = render_dry_run_report(
            axum_project.path(),
            axum_staged.path(),
            &axum_allow,
            &ProvisionOutcome::default(),
        );
        assert!(
            axum_report.contains(".edgezero/.env"),
            "axum must diff .edgezero/.env: {axum_report}"
        );
        assert!(
            axum_report.contains("axum.toml"),
            "axum must diff axum.toml (provision-synthesised): {axum_report}"
        );
        // Negative: cloudflare-only paths
        assert!(
            !axum_report.contains("wrangler.toml"),
            "wrangler.toml must not appear in axum diff"
        );
    }

    #[test]
    fn dry_run_diff_handles_manifest_in_subdir_of_adapter_crate() {
        // Fixture: manifest in a SUB-directory of the adapter crate.
        //   [adapters.cloudflare.adapter]
        //   crate = "crates/cf"
        //   manifest = "crates/cf/config/wrangler.toml"
        // The static-name allow-list would compute pair location as
        // `crates/cf/wrangler.toml` — WRONG. Both sides absent →
        // silent no-diff.
        let project = TempDir::new().expect("project");
        let staged = TempDir::new().expect("staged");
        fs::create_dir_all(staged.path().join("crates/cf/config")).unwrap();
        fs::write(
            staged.path().join("crates/cf/config/wrangler.toml"),
            "name = \"cf-nested\"\n",
        )
        .unwrap();
        fs::write(
            staged.path().join("crates/cf/config/.dev.vars"),
            "SECRET=abc\n",
        )
        .unwrap();

        let allow = build_dry_run_allow_list(
            project.path(),
            staged.path(),
            "cloudflare",
            &staged.path().join("crates/cf/config/wrangler.toml"),
        );
        let report = render_dry_run_report(
            project.path(),
            staged.path(),
            &allow,
            &ProvisionOutcome::default(),
        );

        // Positive: nested paths present
        assert!(
            report.contains("crates/cf/config/wrangler.toml"),
            "nested wrangler.toml must appear in the diff: {report}"
        );
        assert!(
            report.contains("crates/cf/config/.dev.vars"),
            "nested .dev.vars (sibling of the resolved manifest) must appear: {report}"
        );
        // Negative: the WRONG (static-name) location must NOT appear.
        // A regression to the old shape would silently write
        // `--- crates/cf/wrangler.toml` here.
        assert!(
            !report.contains("--- crates/cf/wrangler.toml"),
            "diff must not reference the wrong (adapter-crate-relative) location: {report}"
        );
    }

    #[test]
    fn provision_cloud_dry_run_passes_dry_run_true_to_adapter() {
        // Cloud dry-run must not synthesise (Task 8b covers that) and
        // must pass dry_run=true down to the adapter. Use the fake and
        // read back RECORDED_DRY_RUN to confirm the boolean rode
        // through the dispatch matrix untouched.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).unwrap();
        // Cloud validates the real worktree, so the fake's synthesised
        // file must already be present or validate_adapter_manifest
        // will Err before dispatch.
        fs::create_dir_all(temp.path().join("crates/spin")).unwrap();
        fs::write(temp.path().join("crates/spin/spin.toml"), "# stub\n").unwrap();

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path,
        })
        .expect("cloud dry-run should succeed with fake adapter");

        assert!(
            RECORDED_DRY_RUN.load(Ordering::SeqCst),
            "adapter.provision must have been called with dry_run = true"
        );
        assert!(
            !SYNTH_CALLED.load(Ordering::SeqCst),
            "cloud must never invoke synthesise_baseline_manifest"
        );
    }

    #[test]
    fn provision_local_dry_run_passes_dry_run_false_to_adapter() {
        // Spec §"Dry-run" step 3: local dry-run stages a tempdir and
        // dispatches with `dry_run = false` so adapters take their
        // real-write branches against the staged tree. If the CLI
        // hardcoded `true` here, adapters would early-return from
        // their dry-run branches (cloudflare cli.rs:263, spin
        // cli.rs:223) and leave the staged tree empty — the diff
        // report would then miss the very files operators want to
        // preview.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, FAKE_MANIFEST_BODY).unwrap();
        // Local dry-run stages into a tempdir, so crates/spin/ must
        // exist under the source tree for run_with_staging to copy
        // it. The fake's synthesiser writes spin.toml INSIDE the
        // staged tree, so we do NOT need to pre-create spin.toml
        // here.
        fs::create_dir_all(temp.path().join("crates/spin")).unwrap();

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path,
        })
        .expect("local dry-run should succeed with fake adapter");

        assert!(
            !RECORDED_DRY_RUN.load(Ordering::SeqCst),
            "adapter.provision must be called with dry_run = false: \
             the tempdir IS the dry-run mechanism, not the boolean"
        );
        assert!(
            SYNTH_CALLED.load(Ordering::SeqCst),
            "local dry-run must invoke synthesise_baseline_manifest"
        );
    }

    // ---------- Section-5 lock-in: dry-run cleanliness against the real
    // in-tree fixture, across every adapter. Ignored until Section 5's
    // per-adapter local writers land (Tasks 17-28); today the adapters'
    // Local-mode `provision` impls return
    // `Err("local mode lands in Section 5")` before touching disk, so
    // the assertions can't yet drive real behavior. This test defines
    // the contract now so the eventual implementation doesn't drift.
    //
    // Contract A (worktree byte-identical after dry-run) is asserted
    // via the existing `snapshot_dir` helper.
    //
    // Contract B (no tempdir path leakage into stdout) is left as a
    // `TODO(section-5)` comment: the CLI uses `log::info!` for status
    // lines, but `log::set_logger` is a process-wide one-shot and
    // installing a capturing logger here would race the other tests
    // that share the crate's default logger initialization. Adding a
    // per-thread capture shim would require workspace-scope churn
    // that this task explicitly declines. When Section 5 lands, a
    // follow-up task can retrofit either a subprocess-based capture
    // or a `tracing`-subscriber swap.
    #[test]
    fn provision_local_dry_run_worktree_clean_and_no_tempdir_paths_in_stdout() {
        let _lock = manifest_guard().lock().expect("manifest guard");

        // Resolve the repo root from the crate's manifest dir:
        // `<repo>/crates/edgezero-cli` → `<repo>`. `CARGO_MANIFEST_DIR`
        // is always set for `cargo test`.
        let manifest_dir = env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .expect("CARGO_MANIFEST_DIR must be set under cargo test");
        let repo_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("resolve repo root from CARGO_MANIFEST_DIR")
            .to_path_buf();
        let manifest_path = repo_root.join("examples/app-demo/edgezero.toml");
        let app_demo_root = manifest_path.parent().expect("app-demo dir").to_path_buf();
        assert!(
            manifest_path.exists(),
            "fixture missing: {}",
            manifest_path.display()
        );

        // Exclude directories that are gitignored / test-runtime-only.
        // `target/` alone is tens of gigabytes of Rust build output;
        // reading it would time the test out with no signal — build
        // artifacts are gitignored and outside the "did dry-run touch
        // the worktree" question. `.spin/` and `.wrangler/` hold
        // adapter runtime state (sqlite KV files, cached tokens) that
        // fluctuate under normal `run_serve` and are also gitignored.
        let excluded: &[&str] = &["target", ".git", ".spin", ".wrangler"];

        for adapter in ["cloudflare", "fastly", "spin", "axum"] {
            let before = snapshot_dir_excluding(&app_demo_root, excluded);

            // Ignore the Result — Contract A is the "was the worktree
            // modified?" claim, and it holds regardless of whether the
            // adapter's Local arm returned Ok or Err. Explicit type
            // annotation quiets `let_underscore_untyped` /
            // `let_underscore_must_use`.
            let _result: Result<(), String> = run_provision(&ProvisionArgs {
                adapter: (*adapter).to_owned(),
                dry_run: true,
                local: true,
                manifest: manifest_path.clone(),
            });

            let after = snapshot_dir_excluding(&app_demo_root, excluded);
            assert_eq!(
                before, after,
                "adapter {adapter}: dry-run must leave the worktree byte-identical"
            );

            // TODO(section-5): assert no tempdir path leakage in
            // stdout via captured log. The `log::info!` output from
            // `render_dry_run_report` should never contain
            // `/var/folders/`, `/private/var/folders/`, or `/tmp/`
            // — only project-relative paths under the manifest
            // root. Capture strategy TBD (see the module comment
            // above this test).
        }
    }

    #[test]
    fn validate_deployed_field_ownership_accepts_declared_field() {
        // Fake registers itself as owning `service_id`. A manifest
        // with [adapters.__test_bootstrap_fake__.deployed] service_id
        // = "..." must validate cleanly.
        use crate::config::validate_deployed_field_ownership;
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);

        let toml_body = r#"
        [app]
        name = "demo"
        [adapters.__test_bootstrap_fake__]
        [adapters.__test_bootstrap_fake__.adapter]
        crate = "crates/spin"
        manifest = "crates/spin/spin.toml"
        [adapters.__test_bootstrap_fake__.deployed]
        service_id = "SVC1"
        "#;
        let manifest: Manifest = toml::from_str(toml_body).unwrap();
        manifest.validate().unwrap();
        validate_deployed_field_ownership(&manifest)
            .expect("fake owns service_id -- must validate cleanly");
    }

    #[test]
    fn validate_deployed_field_ownership_rejects_undeclared_field() {
        // A different fake that owns NO deployed fields. A manifest
        // with service_id under its section must be rejected.
        use crate::config::validate_deployed_field_ownership;
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&NO_FIELDS_FAKE_ADAPTER);

        let toml_body = r#"
        [app]
        name = "demo"
        [adapters.__test_no_fields_fake__]
        [adapters.__test_no_fields_fake__.adapter]
        crate = "crates/spin"
        manifest = "crates/spin/spin.toml"
        [adapters.__test_no_fields_fake__.deployed]
        service_id = "SVC1"
        "#;
        let manifest: Manifest = toml::from_str(toml_body).unwrap();
        manifest.validate().unwrap();
        let err = validate_deployed_field_ownership(&manifest)
            .expect_err("adapter declares no fields -- must reject");
        assert!(
            err.contains("service_id") && err.contains("__test_no_fields_fake__"),
            "error must name the offending field and adapter: {err}"
        );
    }

    // ---------- deployed_state_for + run_provision wiring ----------

    #[test]
    fn deployed_state_for_translates_all_field_kinds() {
        // Manifest with a `demo` adapter carrying every deployed-field
        // kind: scalar service_id, kv_namespaces map, and
        // preview_kv_namespaces map. The translator must land them at
        // the neutral positions each adapter reads from — scalar
        // fields under `state.fields`, maps under `state.sub_tables`.
        let toml_body = r#"
[app]
name = "demo"

[adapters.demo]
[adapters.demo.adapter]
crate = "crates/x"
manifest = "crates/x/m.toml"

[adapters.demo.deployed]
service_id = "SVC1"
kv_namespaces.sessions = "abc123"
preview_kv_namespaces.sessions = "abc123_preview"
"#;
        let manifest: Manifest = toml::from_str(toml_body).unwrap();
        let state =
            deployed_state_for(&manifest, "demo").expect("populated deployed must be Some(state)");
        assert_eq!(
            state.fields.get("service_id").map(String::as_str),
            Some("SVC1")
        );
        assert_eq!(
            state
                .sub_tables
                .get("kv_namespaces")
                .and_then(|map| map.get("sessions"))
                .map(String::as_str),
            Some("abc123")
        );
        assert_eq!(
            state
                .sub_tables
                .get("preview_kv_namespaces")
                .and_then(|map| map.get("sessions"))
                .map(String::as_str),
            Some("abc123_preview")
        );
    }

    #[test]
    fn deployed_state_for_returns_none_when_all_fields_empty() {
        // Adapter has NO deployed block: translator returns None so
        // synthesise / provision impls see the same signal they did
        // in the pre-Task-14 world (empty state = None).
        let toml_body = r#"
[app]
name = "demo"

[adapters.demo]
[adapters.demo.adapter]
crate = "crates/x"
manifest = "crates/x/m.toml"
"#;
        let manifest: Manifest = toml::from_str(toml_body).unwrap();
        assert!(deployed_state_for(&manifest, "demo").is_none());
    }

    #[test]
    fn provision_local_threads_deployed_state_into_synthesiser() {
        // Regression: `deployed_state_for` was left returning None
        // through the whole of Section 4. Result: real deployed IDs
        // in edgezero.toml never reached the adapter's
        // synthesise_baseline_manifest call, defeating the "teammates
        // regenerate local manifests from tracked durable IDs" spec
        // promise. This test asserts the CLI reads
        // `[adapters.<fake>.deployed]` and passes it through.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo"

[adapters.__test_bootstrap_fake__]
[adapters.__test_bootstrap_fake__.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.__test_bootstrap_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.__test_bootstrap_fake__.deployed]
service_id = "SVC1"
"#;
        fs::write(&manifest_path, manifest_body).unwrap();
        fs::create_dir_all(temp.path().join("crates/spin")).unwrap();

        run_provision(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path,
        })
        .expect("local real-write should reach the fake's synthesiser");

        assert!(SYNTH_CALLED.load(Ordering::SeqCst));
        let observed = RECORDED_SYNTH_DEPLOYED
            .lock()
            .expect("recorded deployed slot poisoned")
            .clone()
            .expect("synthesiser must have received Some(state)");
        assert_eq!(
            observed.fields.get("service_id").map(String::as_str),
            Some("SVC1"),
            "manifest's `[adapters.*.deployed] service_id` must reach the adapter: {observed:?}"
        );
    }

    #[test]
    fn provision_rejects_deployed_block_with_field_adapter_does_not_own() {
        // The ownership check exists in run_shared_checks (config
        // validate + push + diff pick it up), but until this patch
        // run_provision did NOT call it. That gap let
        // `edgezero provision` accept manifests that `edgezero config
        // validate` correctly rejected. Regression test: register
        // NoFieldsFakeAdapter (owns nothing per deployed_fields()),
        // put a service_id under its deployed block, and assert
        // run_provision Errs with the ownership violation before
        // reaching the dispatch matrix.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&NO_FIELDS_FAKE_ADAPTER);
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo"

[adapters.__test_no_fields_fake__]
[adapters.__test_no_fields_fake__.adapter]
crate = "crates/x"
manifest = "crates/x/m.toml"

[adapters.__test_no_fields_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[adapters.__test_no_fields_fake__.deployed]
service_id = "SVC1"
"#;
        fs::write(&manifest_path, manifest_body).unwrap();
        fs::create_dir_all(temp.path().join("crates/x")).unwrap();

        let err = run_provision(&ProvisionArgs {
            adapter: "__test_no_fields_fake__".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path,
        })
        .expect_err("adapter owns no deployed fields: service_id must be rejected");
        assert!(
            err.contains("service_id") && err.contains("__test_no_fields_fake__"),
            "error must name offending field + adapter: {err}"
        );
        assert!(
            !SYNTH_CALLED.load(Ordering::SeqCst),
            "ownership check must fire before synthesise_baseline_manifest"
        );
    }

    // ---------- merge_deployed_into_manifest ----------

    #[test]
    fn merge_deployed_round_trips_cloudflare_namespaces_with_canonical_key() {
        // Fixture declares mixed-case [adapters.Cloudflare]. Merger
        // MUST use the canonical operator-spelled key — not a
        // lowercased sibling — otherwise a parallel
        // [adapters.cloudflare.deployed] table would appear beside
        // the operator's [adapters.Cloudflare] one.
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(
            &manifest_path,
            r#"
[app]
name = "demo"

[adapters.Cloudflare]
[adapters.Cloudflare.adapter]
crate = "crates/cf"
manifest = "crates/cf/wrangler.toml"
"#,
        )
        .unwrap();

        let mut state = AdapterDeployedState::default();
        let mut kv = BTreeMap::new();
        kv.insert("sessions".to_owned(), "abc123".to_owned());
        state.sub_tables.insert("kv_namespaces".to_owned(), kv);

        // Canonical key is "Cloudflare" (as written in the manifest).
        // owned_fields = &["kv_namespaces", "preview_kv_namespaces"] matches
        // Cloudflare's real deployed_fields() surface.
        merge_deployed_into_manifest(
            &manifest_path,
            "Cloudflare",
            &state,
            &["kv_namespaces", "preview_kv_namespaces"],
            false,
        )
        .unwrap();

        let raw = fs::read_to_string(&manifest_path).unwrap();
        // Must land under the operator's spelling; NO lowercased sibling.
        assert!(
            raw.contains("[adapters.Cloudflare.deployed"),
            "must land under operator spelling: {raw}"
        );
        assert!(
            !raw.contains("[adapters.cloudflare.deployed"),
            "must NOT create a lowercased parallel: {raw}"
        );
        // Value present.
        assert!(
            raw.contains("sessions = \"abc123\""),
            "kv id must round-trip: {raw}"
        );
    }

    #[test]
    fn merge_deployed_preserves_adjacent_operator_comments() {
        // Non-touched adapter sections must survive byte-for-byte,
        // including their comments.
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        let source = r#"
[app]
name = "demo"

# operator note about spin ordering
[adapters.spin]
[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.cloudflare]
[adapters.cloudflare.adapter]
crate = "crates/cf"
manifest = "crates/cf/wrangler.toml"
"#;
        fs::write(&manifest_path, source).unwrap();

        let mut state = AdapterDeployedState::default();
        state
            .fields
            .insert("service_id".to_owned(), "SVC1".to_owned());
        // owned_fields for the cloudflare-shaped comment test needs
        // to include service_id (the Fastly-only field the test uses
        // here) — this is a unit test of the toml_edit writeback,
        // not the ownership gate. Passing a superset keeps the test's
        // focus on comment preservation.
        merge_deployed_into_manifest(
            &manifest_path,
            "cloudflare",
            &state,
            &["service_id", "kv_namespaces", "preview_kv_namespaces"],
            false,
        )
        .unwrap();

        let raw = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            raw.contains("# operator note about spin ordering"),
            "operator comment must survive writeback: {raw}"
        );
    }

    #[test]
    fn merge_deployed_dry_run_does_not_mutate_file() {
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        let source = r#"
[app]
name = "demo"

[adapters.cloudflare]
[adapters.cloudflare.adapter]
crate = "crates/cf"
manifest = "crates/cf/wrangler.toml"
"#;
        fs::write(&manifest_path, source).unwrap();
        let before = fs::read_to_string(&manifest_path).unwrap();

        let mut state = AdapterDeployedState::default();
        let mut kv = BTreeMap::new();
        kv.insert("sessions".to_owned(), "abc".to_owned());
        state.sub_tables.insert("kv_namespaces".to_owned(), kv);

        merge_deployed_into_manifest(
            &manifest_path,
            "cloudflare",
            &state,
            &["kv_namespaces", "preview_kv_namespaces"],
            true,
        )
        .unwrap();

        let after = fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(before, after, "dry-run must leave file byte-identical");
    }

    #[test]
    fn merge_deployed_rejects_adapter_emitted_unknown_field() {
        // A buggy adapter returning a deployed key that isn't in the
        // `ManifestAdapterDeployed` schema must be rejected BEFORE we
        // write anything to edgezero.toml. Otherwise the manifest
        // would fail future loads via `deny_unknown_fields`.
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, "[app]\nname = \"demo\"\n").unwrap();

        let mut state = AdapterDeployedState::default();
        state
            .fields
            .insert("nonsense_key".to_owned(), "x".to_owned());

        let err = merge_deployed_into_manifest(
            &manifest_path,
            "cloudflare",
            &state,
            &["service_id", "kv_namespaces", "preview_kv_namespaces"],
            false,
        )
        .expect_err("unknown deployed field must be rejected before writeback");
        assert!(
            err.contains("nonsense_key") && err.contains("unknown"),
            "error must name the offending field: {err}"
        );
        // File must be untouched — write never happened.
        assert_eq!(
            fs::read_to_string(&manifest_path).unwrap(),
            "[app]\nname = \"demo\"\n"
        );
    }

    #[test]
    fn merge_deployed_rejects_adapter_emitted_non_owned_field() {
        // A buggy adapter that emits a known deployed field it does
        // NOT own must be rejected. Symmetric to
        // `validate_deployed_field_ownership`, which gates operator-
        // written manifests before dispatch.
        let temp = TempDir::new().unwrap();
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, "[app]\nname = \"demo\"\n").unwrap();

        let mut state = AdapterDeployedState::default();
        state
            .fields
            .insert("service_id".to_owned(), "SVC1".to_owned());

        // Cloudflare owns kv_namespaces + preview_kv_namespaces, NOT
        // service_id. A Cloudflare adapter emitting service_id is a
        // bug the writeback must catch.
        let err = merge_deployed_into_manifest(
            &manifest_path,
            "cloudflare",
            &state,
            &["kv_namespaces", "preview_kv_namespaces"],
            false,
        )
        .expect_err("known-but-non-owned deployed field must be rejected");
        assert!(
            err.contains("service_id") && err.contains("does not own"),
            "error must name the offending field: {err}"
        );
    }

    // ---------- write_baseline_to_disk containment ----------

    #[test]
    fn write_baseline_rejects_absolute_path() {
        // An adapter's `synthesise_baseline_manifest` returning an
        // absolute path would escape the project tree via
        // `root.join(abs)` (Rust replaces `root` when the joined
        // path is absolute).
        let temp = TempDir::new().unwrap();
        let pairs = vec![(PathBuf::from("/tmp/x.toml"), "content".to_owned())];
        let err = write_baseline_to_disk(temp.path(), &pairs)
            .expect_err("absolute baseline path must be rejected");
        assert!(
            err.contains("project-relative") && err.contains("/tmp/x.toml"),
            "error must name the violation + offending path: {err}"
        );
        assert!(
            !temp.path().join("tmp/x.toml").exists(),
            "no file must have been written"
        );
    }

    #[test]
    fn write_baseline_rejects_parent_traversal() {
        // `../` in the adapter-returned rel path would let a buggy
        // synthesiser write outside the staged root or the project
        // crate. Reject before touching disk.
        let temp = TempDir::new().unwrap();
        let pairs = vec![(PathBuf::from("../outside.toml"), "content".to_owned())];
        let err = write_baseline_to_disk(temp.path(), &pairs)
            .expect_err("`..` traversal in baseline path must be rejected");
        assert!(
            err.contains("`..` traversal") && err.contains("../outside.toml"),
            "error must name the violation + offending path: {err}"
        );
        assert!(
            !temp.path().parent().unwrap().join("outside.toml").exists(),
            "no file must have been written outside the root"
        );
    }

    // ---------- run_provision_typed ----------
    //
    // Task 29 wires the CLI's typed-secret companion to `run_provision`.
    // The public entry cloud-short-circuits (delegates to
    // `run_provision`) and only performs typed-secret handling in local
    // mode. Local mode runs the shared preflight (capability +
    // handler-path gates) BEFORE any staging, then dispatches
    // `provision` + `provision_typed` inside the same tempdir so the
    // typed merge sees the baseline manifest the base step wrote.

    /// Small `AppConfigMeta` fixture used across the
    /// `run_provision_typed` tests. Mirrors the shape of production
    /// configs: one non-secret field with a `validator` rule
    /// (`greeting`) so the "malformed non-secret" test can trigger
    /// `validate_excluding_secrets`, and one `#[secret]` field with
    /// `KeyInDefault` so `build_typed_secret_entries` produces a
    /// single entry against `[stores.secrets].default`.
    #[derive(Debug, serde::Deserialize, serde::Serialize, validator::Validate)]
    struct TypedTestConfig {
        api_token: String,
        #[validate(length(min = 1_u64))]
        greeting: String,
    }

    impl AppConfigMeta for TypedTestConfig {
        const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
            kind: SecretKind::KeyInDefault,
            name: "api_token",
        }];
    }

    const TYPED_APP_CONFIG: &str = r#"
api_token = "demo_api_token"
greeting = "hello"
"#;

    /// Manifest the local-mode typed-provision tests share. Uses the
    /// fake adapter so `provision` + `provision_typed` are observable
    /// via the module-scope statics. Declares a single-id secret store
    /// so the fake's `single_store_kinds = &["secrets"]` capability
    /// gate passes.
    const TYPED_FAKE_MANIFEST_BODY: &str = r#"
[app]
name = "demo-app"

[adapters.__test_bootstrap_fake__.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.__test_bootstrap_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;

    #[test]
    fn run_provision_typed_cloud_short_circuits_without_loading_app_config() {
        // Cloud mode has no typed writeback (Cloudflare's `wrangler
        // secret put` and Fastly's compute deploy handle secrets via
        // their own flows). The typed entry point MUST short-circuit
        // to `run_provision` BEFORE touching the app-config file —
        // otherwise a cloud call with a missing <app>.toml would fail
        // where it never used to.
        //
        // Fixture: valid edgezero.toml, deliberately NO `demo-app.toml`.
        // Cloud short-circuit = Ok. A regression that loaded the typed
        // config unconditionally would surface as an "io" / "not
        // found" error naming `demo-app.toml`.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        assert!(
            !temp.path().join("demo-app.toml").exists(),
            "pre-condition: no <app>.toml"
        );

        run_provision_typed::<TypedTestConfig>(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: false,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("cloud short-circuit must succeed without touching <app>.toml");
    }

    #[test]
    fn run_provision_typed_local_fails_loud_on_malformed_app_config() {
        // Local mode runs `validate_excluding_secrets` on the typed
        // config. A validator-rejected NON-secret value must surface
        // as our fn's wrapped `app config validation failed: …` error.
        // (Secret fields skip validators here; the check is in the
        // shared `run_typed_preflight` typed_secret_checks that we
        // gate on separately.)
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, TYPED_FAKE_MANIFEST_BODY).expect("write manifest");
        // `greeting = ""` violates `#[validate(length(min = 1))]` on
        // TypedTestConfig; `api_token` is non-empty so
        // typed_secret_checks would pass on its own.
        let malformed_app_config = "api_token = \"tok\"\ngreeting = \"\"\n";
        fs::write(temp.path().join("demo-app.toml"), malformed_app_config)
            .expect("write app config");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");

        let err = run_provision_typed::<TypedTestConfig>(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("malformed non-secret value must be rejected");
        assert!(
            err.contains("app config validation failed"),
            "error must be the wrapped validator failure: {err}"
        );
    }

    #[test]
    fn run_provision_typed_local_builds_typed_secret_entries_from_raw_table() {
        // The public typed entry point must feed adapters a slice of
        // `TypedSecretEntry` values derived from the raw app-config
        // table via `build_typed_secret_entries`. This test locks the
        // raw-table → `TypedSecretEntry` translation end-to-end: the
        // fake's `provision_typed` captures the slice, and we assert
        // (store_id, field_name, key_value) matches what the fixture
        // wrote.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, TYPED_FAKE_MANIFEST_BODY).expect("write manifest");
        fs::write(temp.path().join("demo-app.toml"), TYPED_APP_CONFIG).expect("write app config");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");

        run_provision_typed::<TypedTestConfig>(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("typed real-write path must succeed with the fake adapter");

        let recorded = RECORDED_TYPED_ENTRIES
            .lock()
            .expect("recorded typed entries lock")
            .clone();
        assert_eq!(
            recorded,
            vec![(
                "default".to_owned(),
                "api_token".to_owned(),
                "demo_api_token".to_owned()
            )],
            "typed secret entry must map (default store, api_token field, demo_api_token key): {recorded:?}"
        );
    }

    #[test]
    fn run_provision_typed_local_dry_run_runs_capability_preflight() {
        // The capability gate MUST fire BEFORE any staging so dry-run
        // can't bypass expensive-mistake protection. Fake declares
        // `single_store_kinds = &["secrets"]`, so a manifest with two
        // secret ids trips the gate. Assert the wording matches
        // `enforce_single_store_capability`'s existing message and
        // that `SYNTH_CALLED` is still false (the gate short-circuited
        // before the tempdir + baseline synthesis fired).
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.__test_bootstrap_fake__.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.__test_bootstrap_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default", "other"]
default = "default"
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(temp.path().join("demo-app.toml"), TYPED_APP_CONFIG).expect("write app config");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");

        let err = run_provision_typed::<TypedTestConfig>(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("capability gate must reject multi-id declaration");
        assert!(
            err.contains("Single-capable for secrets"),
            "error must match `enforce_single_store_capability`: {err}"
        );
        assert!(
            !SYNTH_CALLED.load(Ordering::SeqCst),
            "capability gate must fire BEFORE staging (synth not called)"
        );
    }

    #[test]
    fn run_provision_typed_local_dry_run_runs_handler_paths_preflight() {
        // Symmetric to the capability-gate test but for
        // `strict_handler_paths`. A malformed trigger handler must
        // reject BEFORE the tempdir + synthesis fire. Uses the fake
        // adapter so we can observe `SYNTH_CALLED` staying false.
        let _lock = manifest_guard().lock().expect("manifest guard");
        reset_fake_state();
        register_adapter(&FAKE_ADAPTER);
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.__test_bootstrap_fake__.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.__test_bootstrap_fake__.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]

[[triggers.http]]
path = "/"
methods = ["GET"]
handler = "not a valid path"
adapters = ["__test_bootstrap_fake__"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(temp.path().join("demo-app.toml"), TYPED_APP_CONFIG).expect("write app config");
        fs::create_dir_all(temp.path().join("crates/spin")).expect("create adapter crate dir");

        let err = run_provision_typed::<TypedTestConfig>(&ProvisionArgs {
            adapter: "__test_bootstrap_fake__".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("handler-path gate must reject malformed handler");
        assert!(
            err.contains("handler") && err.contains("Rust path"),
            "error must match `strict_handler_paths`: {err}"
        );
        assert!(
            !SYNTH_CALLED.load(Ordering::SeqCst),
            "handler-path gate must fire BEFORE staging (synth not called)"
        );
    }

    #[test]
    fn run_provision_typed_local_dry_run_handles_case_preserving_adapter_key() {
        // The allow-list builder / `default_adapter_manifest_for` both
        // match on lowercase — the CLI MUST lowercase the canonical
        // adapter name (as spelled in `[adapters.<name>]`) before
        // dispatch, or nested paths like `.edgezero/.env` would never
        // appear in the dry-run diff for a manifest that spells the
        // adapter with a leading capital.
        //
        // Fixture: `[adapters.Axum]` mixed case (canonical returned
        // from `adapter_entry("axum")` = "Axum"). If our code fails to
        // lowercase, the axum arm in `build_dry_run_allow_list` misses,
        // the allow-list stays empty, and `.edgezero/.env` never
        // appears in the rendered diff. Asserting the report contains
        // the `--- ` header for `.edgezero/.env` proves the lowercase
        // step happened. Call `run_local_dry_run_typed` directly (not
        // `run_provision_typed`) so the report is inspectable — the
        // public entry only `log::info!`s it.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = r#"
[app]
name = "demo-app"

[adapters.Axum.adapter]
crate = "crates/demo-axum"
manifest = "crates/demo-axum/axum.toml"

[adapters.Axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.secrets]
ids = ["default"]
"#;
        fs::write(&manifest_path, manifest_body).expect("write manifest");
        fs::write(temp.path().join("demo-app.toml"), TYPED_APP_CONFIG).expect("write app config");
        fs::create_dir_all(temp.path().join("crates/demo-axum")).expect("create adapter crate dir");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        let args = ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        };

        // First: prove the public entry point returns Ok end-to-end.
        run_provision_typed::<TypedTestConfig>(&args)
            .expect("typed dry-run must succeed with a mixed-case adapter key");

        // Second: re-run the dry-run helper directly so we can inspect
        // the rendered report. Reconstruct the same context the public
        // fn built — this is the CLI's own private path and the test
        // module is in the same crate.
        let ctx = load_validation_context_with_options(&args.manifest, None, false, false)
            .expect("load validation context");
        let (canonical_borrow, adapter_cfg) = ctx
            .manifest()
            .adapter_entry(&args.adapter)
            .expect("adapter_entry lookup");
        assert_eq!(
            canonical_borrow, "Axum",
            "sentinel: canonical spelling stays mixed-case"
        );
        let canonical_name = canonical_borrow.clone();
        let adapter_manifest_rel = adapter_cfg.adapter.manifest.clone();
        let adapter_component = adapter_cfg.adapter.component.clone();
        let adapter_crate_rel = adapter_cfg.adapter.crate_path.clone();
        let adapter = get_adapter(&canonical_name).expect("registry lookup case-insensitive");
        let entries = build_typed_secret_entries::<TypedTestConfig>(&ctx)
            .expect("build typed secret entries");
        let manifest_root = manifest_root_from(&args.manifest);
        let report = run_local_dry_run_typed(
            &DryRunTypedRequest {
                adapter,
                ctx: &ctx,
                canonical_adapter_name: &canonical_name,
                adapter_manifest_rel: adapter_manifest_rel.as_deref(),
                adapter_component: adapter_component.as_deref(),
                adapter_crate_rel: adapter_crate_rel.as_deref(),
                manifest_root,
            },
            &args,
            &entries,
        )
        .expect("dry-run helper must succeed");

        assert!(
            report.contains("--- "),
            "diff section must be present (allow-list arm matched via lowercase): {report}"
        );
        assert!(
            report.contains(".edgezero/.env"),
            "axum's `.edgezero/.env` file must appear in the diff: {report}"
        );
    }

    /// Reverse of the sibling test: manifest declares
    /// `[adapters.axum]` (lowercase, canonical form) and the
    /// operator passes `--adapter AXUM` (all-caps). Both directions
    /// of the case-insensitive lookup MUST work -- prior coverage only
    /// tested (Axum manifest, "axum" arg), leaving a code path that
    /// lowercased only ONE side untested.
    #[test]
    fn run_provision_local_handles_all_caps_adapter_arg_against_lowercase_manifest_key() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        // All-caps arg against the lowercase manifest key `axum`.
        run_provision(&ProvisionArgs {
            adapter: "AXUM".to_owned(),
            dry_run: false,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect("case-insensitive arg lookup must succeed");
    }

    // ---------- H3 regression: dry-run env / secret redaction ----------

    #[test]
    fn dry_run_diff_redacts_env_values_and_never_leaks_context() {
        // The pre-2026-07 dry-run renderer emitted the raw `.env`
        // pre-image around each hunk with a 2-line context radius.
        // When provision appends new lines at EOF, the LAST two
        // real operator secret lines surface as unchanged context —
        // and a real `.env` populated with API_TOKEN=<secret>
        // leaks the secret into CI logs, screen recordings, and
        // terminal scrollback.
        //
        // The current renderer redacts `KEY=<value>` lines through a
        // `KEY=<redacted>` filter for env-shaped paths BEFORE
        // diffing. This regression pins that behaviour: the diff
        // output MUST NOT contain the raw secret value.
        let project = TempDir::new().expect("project dir");
        let staged = TempDir::new().expect("staged dir");
        let project_env = project.path().join(".edgezero/.env");
        let staged_env = staged.path().join(".edgezero/.env");
        fs::create_dir_all(project_env.parent().unwrap()).expect("mkdir project");
        fs::create_dir_all(staged_env.parent().unwrap()).expect("mkdir staged");

        let secret_value = "sk-live-super-secret-do-not-leak";
        fs::write(
            &project_env,
            format!("# preexisting operator overrides\nAPI_TOKEN={secret_value}\n"),
        )
        .unwrap();
        fs::write(
            &staged_env,
            format!(
                "# preexisting operator overrides\nAPI_TOKEN={secret_value}\nEDGEZERO__STORES__KV__SESSIONS__NAME=sessions\n"
            ),
        )
        .unwrap();

        let allow_list = DryRunAllowList {
            pairs: vec![(project_env.clone(), staged_env.clone())],
        };
        let outcome = adapter_registry::ProvisionOutcome::default();
        let report = render_dry_run_report(project.path(), staged.path(), &allow_list, &outcome);

        assert!(
            !report.contains(secret_value),
            "SECURITY: real .env value leaked into dry-run diff — got: {report}"
        );
        assert!(
            report.contains("API_TOKEN=<redacted>"),
            "existing KEY=value line must appear in redacted form so structural diff still reads: {report}"
        );
        assert!(
            report.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=<redacted>"),
            "newly-appended KEY=value line must also be redacted (values never surface): {report}"
        );
    }

    #[test]
    fn dry_run_diff_emits_redacted_placeholder_when_only_values_changed() {
        // Provision that ONLY rewrites secret values (no structural
        // change) must show the operator that something changed
        // without leaking either the old or new value. The renderer
        // emits a single redacted-header line for that case.
        let project = TempDir::new().expect("project dir");
        let staged = TempDir::new().expect("staged dir");
        let project_env = project.path().join(".dev.vars");
        let staged_env = staged.path().join(".dev.vars");

        fs::write(&project_env, "API_TOKEN=old-secret\n").unwrap();
        fs::write(&staged_env, "API_TOKEN=new-secret\n").unwrap();

        let allow_list = DryRunAllowList {
            pairs: vec![(project_env.clone(), staged_env.clone())],
        };
        let outcome = adapter_registry::ProvisionOutcome::default();
        let report = render_dry_run_report(project.path(), staged.path(), &allow_list, &outcome);

        assert!(!report.contains("old-secret"), "old value leaked: {report}");
        assert!(!report.contains("new-secret"), "new value leaked: {report}");
        assert!(
            report.contains("(redacted:") && report.contains("values differ"),
            "value-only churn must surface a single redacted-header line: {report}"
        );
    }

    #[test]
    fn dry_run_diff_leaves_non_env_files_unredacted() {
        // Only `.env` / `.dev.vars` are treated as secret carriage.
        // Every other file (adapter manifests, JSON config, etc.)
        // must diff normally so operators can see the actual changes.
        let project = TempDir::new().expect("project dir");
        let staged = TempDir::new().expect("staged dir");
        let project_toml = project.path().join("spin.toml");
        let staged_toml = staged.path().join("spin.toml");

        fs::write(&project_toml, "name = \"old-name\"\n").unwrap();
        fs::write(&staged_toml, "name = \"new-name\"\n").unwrap();

        let allow_list = DryRunAllowList {
            pairs: vec![(project_toml.clone(), staged_toml.clone())],
        };
        let outcome = adapter_registry::ProvisionOutcome::default();
        let report = render_dry_run_report(project.path(), staged.path(), &allow_list, &outcome);

        assert!(
            report.contains("old-name") && report.contains("new-name"),
            "non-env file must diff normally without redaction: {report}"
        );
    }

    #[test]
    fn redact_env_body_preserves_comments_and_blank_lines() {
        let body = "\
# A comment\n\
\n\
API_TOKEN=very-secret-value\n\
\n\
# another comment\n\
DATABASE_URL=postgres://user:pass@host/db\n\
";
        let redacted = redact_env_body_for_diff(body);
        assert!(redacted.contains("# A comment"), "comment preserved");
        assert!(
            redacted.contains("# another comment"),
            "second comment preserved"
        );
        assert!(
            redacted.contains("API_TOKEN=<redacted>"),
            "value redacted, key preserved: {redacted}"
        );
        assert!(
            redacted.contains("DATABASE_URL=<redacted>"),
            "value with = in it fully redacted: {redacted}"
        );
        assert!(
            !redacted.contains("very-secret-value"),
            "secret value must not survive: {redacted}"
        );
        assert!(
            !redacted.contains("postgres://"),
            "value must be replaced entirely (nothing after the first `=`): {redacted}"
        );
    }
}
