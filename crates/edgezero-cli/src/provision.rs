//! `provision` command.
//!
//! Thin delegate to the adapter registry. The CLI loads the manifest,
//! resolves the named adapter, hands it the declared store ids per
//! kind, and prints the human-readable status lines the adapter
//! returns. All the platform-specific work — `wrangler kv namespace
//! create`, `fastly *-store create`, `spin.toml` editing — lives in
//! each `edgezero-adapter-*` crate's `Adapter::provision` impl, not
//! here.

use std::fs;
use std::path::{Path, PathBuf};

use similar::{ChangeTag, TextDiff};
use toml_edit::{table, value, DocumentMut};

use crate::args::ProvisionArgs;
use crate::config::{
    enforce_single_store_capability, reject_merged_id_collisions, strict_handler_paths,
};
use crate::copy_tree::copy_dir_recursive;
use crate::ensure_adapter_defined;
use crate::path_safety::assert_provision_paths_contained;
use edgezero_adapter::registry::{self as adapter_registry, ProvisionStores, ResolvedStoreId};
use edgezero_adapter::AdapterDeployedState;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{Manifest, ManifestAdapter, ManifestLoader, StoreDeclaration};

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
/// - Axum: project-root `.edgezero/.env` only.
/// - Cloudflare: resolved `wrangler.toml` + sibling `.dev.vars`.
/// - Fastly: resolved `fastly.toml`.
/// - Spin: resolved `spin.toml` + sibling `runtime-config.toml` +
///   sibling `.env`.
///
/// `axum.toml` is NOT in this list (it stays tracked).
pub(crate) struct DryRunAllowList {
    /// (`project_path`, `staged_path`) pairs the driver diffs.
    pub pairs: Vec<(PathBuf, PathBuf)>,
}

/// # Errors
///
/// Returns an error string if the manifest can't be loaded, the
/// adapter isn't declared in `[adapters.*]`, the adapter isn't
/// registered in this build, or the adapter's `provision` impl
/// reports a failure.
#[inline]
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String> {
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

    // Path containment: reject `..` traversal and absolute paths in
    // the manifest-declared adapter paths before any adapter dispatch
    // or file resolution. Mirrors the `config push --local` guard
    // (Task 7); the same helper closes the spec's "local provision
    // never writes outside the adapter crate" promise. Cloud mode
    // still targets remote SDKs so containment isn't load-bearing;
    // gating on `args.local` also preserves the existing cloud
    // fixtures where `manifest` lives at project root outside `crate`.
    if args.local {
        let manifest_root_for_check = args
            .manifest
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        assert_provision_paths_contained(
            manifest_root_for_check,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.crate_path.as_deref(),
        )?;
    }

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

    // Capability gate: mirror the strict `config validate` check for
    // THIS adapter only. Without it, `provision --adapter spin`
    // happily accepts a manifest with two config ids and dispatches
    // to a backend that has no way to model multiple stores -- the
    // failure only surfaces at runtime as a confusing "wrong store"
    // miss. The check is unconditional (no --strict gate) because
    // it's not stylistic: the platform genuinely cannot honour the
    // declaration.
    enforce_single_store_capability(manifest, &args.adapter)?;

    // Manifest-shape gate: provision is the most expensive
    // operation in the CLI (it can create real Cloudflare / Fastly
    // resources), so a malformed handler path or a broken
    // adapter manifest should fail HERE rather than after the
    // remote create succeeded. `strict_handler_paths` is cheap
    // and unconditional in `config validate --strict`; we run it
    // unconditionally here for the same reason as the capability
    // check above. The per-adapter `validate_adapter_manifest`
    // hook (Spin's `[component.*]` discovery, etc.) is the other
    // half of the strict-validate preflight; it's adapter-specific
    // so we call it only for the targeted adapter.
    strict_handler_paths(manifest)?;
    let manifest_root = args
        .manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

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
            validate_and_dispatch(
                adapter,
                manifest,
                adapter_cfg,
                manifest_root,
                &args.adapter,
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
            write_baseline_to_disk(manifest_root, &baseline_pairs)?;
            validate_and_dispatch(
                adapter,
                manifest,
                adapter_cfg,
                manifest_root,
                &args.adapter,
                deployed.as_ref(),
                adapter_registry::ProvisionMode::Local,
                false,
            )?
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
            args.dry_run,
        )?;
    }
    Ok(())
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
/// tempdir writes (dry-run staging, Task 10+) — the only difference
/// is which root is passed in.
fn write_baseline_to_disk(root: &Path, pairs: &[(PathBuf, String)]) -> Result<(), String> {
    for (rel_path, contents) in pairs {
        let abs = root.join(rel_path);
        if abs.exists() {
            continue;
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
        fs::write(&abs, contents).map_err(|err| format!("write {}: {err}", abs.display()))?;
    }
    Ok(())
}

/// Translate the parent manifest's deployed block for `canonical_adapter_name`
/// into the neutral `AdapterDeployedState` shape. Task 14 introduces the typed
/// `ManifestAdapterDeployed` struct; until that lands this returns `None`
/// unconditionally. The synthesiser call path already receives
/// `Option<&AdapterDeployedState>` — just always `None` today. Section 4
/// fills in the real translation.
fn deployed_state_for(
    _manifest: &Manifest,
    _canonical_adapter_name: &str,
) -> Option<AdapterDeployedState> {
    None
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
pub(crate) fn merge_deployed_into_manifest(
    manifest_path: &Path,
    adapter_name: &str,
    state: &adapter_registry::AdapterDeployedState,
    dry_run: bool,
) -> Result<(), String> {
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
#[expect(
    clippy::too_many_arguments,
    reason = "the shared tail needs adapter, manifest, adapter cfg, manifest root, adapter name, deployed state, mode, and dry-run — same 8 argument shape as `Adapter::provision` itself, whose lint annotation applies for the same reason"
)]
fn validate_and_dispatch(
    adapter: &'static dyn adapter_registry::Adapter,
    manifest: &Manifest,
    adapter_cfg: &ManifestAdapter,
    manifest_root: &Path,
    adapter_name: &str,
    deployed: Option<&AdapterDeployedState>,
    mode: adapter_registry::ProvisionMode,
    dry_run: bool,
) -> Result<adapter_registry::ProvisionOutcome, String> {
    adapter.validate_adapter_manifest(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
    )?;
    let env_config = EnvConfig::from_env();
    reject_merged_id_collisions(adapter_name, adapter, manifest, &env_config)?;
    let config_ids = resolve_kind(manifest.stores.config.as_ref(), &env_config, "config");
    let kv_ids = resolve_kind(manifest.stores.kv.as_ref(), &env_config, "kv");
    let secret_ids = resolve_kind(manifest.stores.secrets.as_ref(), &env_config, "secrets");
    let stores = ProvisionStores {
        config: &config_ids,
        kv: &kv_ids,
        secrets: &secret_ids,
    };
    adapter.provision(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
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
        "cloudflare" => "wrangler.toml",
        "fastly" => "fastly.toml",
        "spin" => "spin.toml",
        _ => "", // axum has no per-adapter manifest in the allow-list
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
        let diff = TextDiff::from_lines(&old, &new);
        out.push('\n');
        out.push_str("--- ");
        out.push_str(&proj_path.display().to_string());
        out.push('\n');
        out.push_str("+++ ");
        out.push_str(&proj_path.display().to_string());
        out.push('\n');
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            out.push_str(sign);
            out.push_str(&change.to_string());
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
            write_baseline_to_disk(staged_root, &baseline_pairs)?;
            adapter.validate_adapter_manifest(
                staged_root,
                adapter_cfg.adapter.manifest.as_deref(),
                adapter_cfg.adapter.component.as_deref(),
            )?;
            let owned_stores = build_stores_against(staged_root, args, adapter, manifest)?;
            // Spec §"Dry-run" step 3: pass `dry_run = false` to the
            // adapter even though `args.dry_run == true`. The tempdir
            // IS the dry-run mechanism — the adapter takes its real
            // write branch against the staged tree so operators can
            // preview the actual files that would land. If we passed
            // `true`, adapters would early-return from their dry-run
            // branches (cloudflare cli.rs:263, spin cli.rs:223) and
            // leave the staged tree empty of the content the diff
            // report is supposed to show.
            adapter.provision(
                staged_root,
                adapter_cfg.adapter.manifest.as_deref(),
                adapter_cfg.adapter.component.as_deref(),
                &owned_stores.as_refs(),
                deployed,
                adapter_registry::ProvisionMode::Local,
                false,
            )
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
    Ok(outcome)
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
        register_adapter, Adapter, AdapterAction, ProvisionMode, ProvisionOutcome,
    };
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;
    use validator::Validate as _;

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
        reason = "the fake overrides name/deployed_fields/provision/synthesise_baseline_manifest/validate_adapter_manifest; every other trait method inherits its default (no-op or Unsupported)"
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

        fn synthesise_baseline_manifest(
            &self,
            _manifest_root: &Path,
            adapter_manifest_path: Option<&str>,
            _component_selector: Option<&str>,
            _app_name: &str,
            _deployed: Option<&AdapterDeployedState>,
        ) -> Result<Vec<(PathBuf, String)>, String> {
            SYNTH_CALLED.store(true, Ordering::SeqCst);
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
        SYNTH_CALLED.store(false, Ordering::SeqCst);
        VALIDATE_SAW_FILE.store(false, Ordering::SeqCst);
    }

    /// Walks the tree at `root` and returns a sorted `Vec<(relative
    /// path, content bytes)>`. Two calls yield equal `Vec`s iff the
    /// tree is byte-identical. Used by the dry-run cleanliness
    /// assertion — any staging leak that writes into the worktree
    /// flips one of the pairs.
    fn snapshot_dir(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        snapshot_walk(root, root, &mut out).expect("snapshot walk");
        out.sort_by(|left, right| left.0.cmp(&right.0));
        out
    }

    fn snapshot_walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) -> io::Result<()> {
        for read_result in fs::read_dir(dir)? {
            let entry = read_result?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                snapshot_walk(base, &path, out)?;
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
    fn run_provision_axum_prints_local_only_notes_for_each_store() {
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
        .expect("axum provision exits 0 (no remote resources)");
    }

    #[test]
    fn run_provision_axum_dry_run_is_also_a_no_op() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("axum dry-run also exits 0");
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
    fn run_provision_spin_dry_run_dispatches_to_adapter() {
        // Dry-run path doesn't edit spin.toml, so CI can exercise
        // dispatch by writing a single-component spin.toml the
        // resolver can locate.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
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
        .expect("spin dry-run dispatches cleanly");
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
        // The adapter-specific `validate_adapter_manifest` hook
        // also gates provision now -- a spin.toml with zero
        // components must error before we touch any remote.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        // spin.toml with NO [component.*] table.
        fs::write(
            temp.path().join("spin.toml"),
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
        // Stage 5: Spin moved `config` to KV (multi-capable). Secrets
        // remain Single-capable until we ship native secret support,
        // so a manifest declaring two secret ids must still trip the
        // gate before dispatching to the spin adapter dry-run. This
        // test pins parity with `config validate --strict`.
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
        // Stage 5: config is KV-backed for Spin, so multiple config
        // ids no longer trip enforce_single_store_capability. The
        // dispatch reaches the adapter dry-run and reports one
        // `key_value_stores` write per id.
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
        // M1: provision must run the same merged-id collision check
        // `config validate` runs. Without it, `provision --adapter
        // spin --dry-run` happily acks a manifest where distinct
        // logical ids `[stores.kv].sessions` and
        // `[stores.config].app_config` BOTH resolve to platform
        // label `shared` via the env overlay -- both writes would
        // silently land on the same Spin KV store at runtime.
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

    #[test]
    fn run_provision_skips_capability_gate_for_kinds_within_single_id_floor() {
        // Sanity: the capability gate fires ONLY when ids.len() > 1.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
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
        .expect("single-id case dispatches cleanly");
    }

    #[test]
    fn run_provision_cloudflare_dry_run_dispatches_to_adapter() {
        // Dry-run path doesn't shell out to wrangler, so CI can
        // exercise dispatch without wrangler installed.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        fs::write(temp.path().join("wrangler.toml"), "name = \"demo\"\n")
            .expect("write wrangler.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "cloudflare".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("cloudflare dry-run dispatches cleanly");
    }

    #[test]
    fn run_provision_fastly_dry_run_dispatches_to_adapter() {
        // Dry-run path doesn't shell out to fastly, so CI can
        // exercise dispatch without fastly installed.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, PROVISION_MANIFEST).expect("write manifest");
        fs::write(temp.path().join("fastly.toml"), "name = \"demo\"\n").expect("write fastly.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);

        run_provision(&ProvisionArgs {
            adapter: "fastly".to_owned(),
            dry_run: true,
            local: false,
            manifest: manifest_path.clone(),
        })
        .expect("fastly dry-run dispatches cleanly");
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
        // dispatch reaches the axum Section-5 stub.
        fs::create_dir_all(temp.path().join("crates/demo-axum")).expect("create adapter crate dir");
        let _cwd = CwdGuard::set(temp.path()).expect("chdir into tempdir");

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: PathBuf::from("edgezero.toml"),
        })
        .expect_err("axum's Section-5 stub errs from inside the staged dispatch");
        // Positive assertion: reaching axum's Section-5 stub proves
        // the manifest loaded, path-safety passed, AND `run_with_staging`
        // routed the closure through validate + build_stores + provision.
        // Without this, an earlier failure would silently satisfy the
        // negative assertions below and give false-positive coverage.
        assert!(
            err.contains("local mode lands in Section 5"),
            "must reach axum's Section-5 stub through staging: {err}"
        );
        assert!(
            !err.contains("must not contain `..` traversal")
                && !err.contains("must be a project-relative path")
                && !err.contains("resolves outside project root"),
            "path-safety must not fire for a valid fixture: {err}"
        );
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

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("axum's Section-5 stub errs from inside the staged dispatch");
        assert!(
            err.contains("local mode lands in Section 5"),
            "must reach axum's Section-5 stub through staging: {err}"
        );
        assert!(
            !err.contains("must not contain `..` traversal")
                && !err.contains("must be a project-relative path")
                && !err.contains("resolves outside project root"),
            "path-safety must not fire for a valid fixture: {err}"
        );
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

        let outcome = ProvisionOutcome {
            status_lines: vec![
                "wrote crates/spin/spin.toml".to_owned(),
                "created .edgezero/.env".to_owned(),
                "appended crates/spin/.env".to_owned(),
            ],
            ..ProvisionOutcome::default()
        };
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

        // Axum fixture: .edgezero/.env only
        let axum_project = TempDir::new().expect("axum project");
        let axum_staged = TempDir::new().expect("axum staged");
        fs::create_dir_all(axum_staged.path().join(".edgezero")).unwrap();
        fs::write(axum_staged.path().join(".edgezero/.env"), "K=V\n").unwrap();
        let axum_allow = build_dry_run_allow_list(
            axum_project.path(),
            axum_staged.path(),
            "axum",
            &axum_staged.path().join("axum.toml"), // adapter manifest not used for axum
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
    #[ignore = "re-enable after Section 5 lands per-adapter local provision"]
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

        for adapter in ["cloudflare", "fastly", "spin", "axum"] {
            let before = snapshot_dir(&app_demo_root);

            // Ignore the Result — today's stub adapters return
            // `Err("local mode lands in Section 5")`. Contract A is
            // the "was the worktree modified?" claim, and it holds
            // regardless of whether the adapter succeeded. Explicit
            // type annotation quiets `let_underscore_untyped` /
            // `let_underscore_must_use`.
            let _result: Result<(), String> = run_provision(&ProvisionArgs {
                adapter: (*adapter).to_owned(),
                dry_run: true,
                local: true,
                manifest: manifest_path.clone(),
            });

            let after = snapshot_dir(&app_demo_root);
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
        merge_deployed_into_manifest(&manifest_path, "Cloudflare", &state, false).unwrap();

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
        merge_deployed_into_manifest(&manifest_path, "cloudflare", &state, false).unwrap();

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

        merge_deployed_into_manifest(&manifest_path, "cloudflare", &state, true).unwrap();

        let after = fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(before, after, "dry-run must leave file byte-identical");
    }
}
