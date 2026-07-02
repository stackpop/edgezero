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

use crate::args::ProvisionArgs;
use crate::config::{
    enforce_single_store_capability, reject_merged_id_collisions, strict_handler_paths,
};
use crate::ensure_adapter_defined;
use crate::path_safety::assert_provision_paths_contained;
use edgezero_adapter::registry::{self as adapter_registry, ProvisionStores, ResolvedStoreId};
use edgezero_adapter::AdapterDeployedState;
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{Manifest, ManifestAdapter, ManifestLoader, StoreDeclaration};

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
        (true, true) => {
            // Local dry-run: staging harness lands in Task 10/11.
            return Err("local dry-run staging lands in Task 10/11".to_owned());
        }
    };

    if args.dry_run {
        log::info!("[edgezero] provision --dry-run for `{}`:", args.adapter);
    }
    for line in outcome.status_lines {
        log::info!("{line}");
    }
    // outcome.deployed wiring lands in Task 16.
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

/// Stage a real recursive copy of the adapter crate dir AND the
/// `.edgezero/` dir (if present) under a fresh `TempDir`, then invoke
/// `body` with the staged paths. The original project worktree is
/// never mutated. Caller is responsible for diffing the staged tree
/// against the project tree before the returned `TempDir` drops. See
/// spec §"Dry-run".
///
/// Gated on `#[cfg(test)]` for now: the only callers are the
/// same-file tests. Task 11 lifts this gate (and `lib.rs`'s
/// `mod copy_tree;` gate) together when the `(true, true)` dispatch
/// arm gains a real caller.
#[cfg(test)]
pub(crate) fn run_with_staging<F, R>(
    project_root: &Path,
    adapter_crate_rel: &Path,
    body: F,
) -> Result<(R, tempfile::TempDir), String>
where
    F: FnOnce(&Path, &Path) -> Result<R, String>,
{
    use crate::copy_tree::copy_dir_recursive;

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
    use std::env;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
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
    static SYNTH_CALLED: AtomicBool = AtomicBool::new(false);
    static VALIDATE_SAW_FILE: AtomicBool = AtomicBool::new(false);

    /// RAII guard: on `set`, chdir into `new_cwd`; on drop, restore
    /// the previous cwd. Callers MUST hold `manifest_guard()` while
    /// this is live — process cwd is global state and can only be
    /// mutated safely under that serialisation lock.
    struct CwdGuard(PathBuf);

    struct FakeBootstrapAdapter;

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
        reason = "the fake only exercises name/provision/synthesise_baseline_manifest/validate_adapter_manifest; every other trait method inherits its default (no-op or Unsupported)"
    )]
    impl Adapter for FakeBootstrapAdapter {
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
            _dry_run: bool,
        ) -> Result<ProvisionOutcome, String> {
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

    fn reset_fake_state() {
        SYNTH_CALLED.store(false, Ordering::SeqCst);
        VALIDATE_SAW_FILE.store(false, Ordering::SeqCst);
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
        let _cwd = CwdGuard::set(temp.path()).expect("chdir into tempdir");

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: PathBuf::from("edgezero.toml"),
        })
        .expect_err("must reach the (true, true) dispatch stub");
        // Positive assertion: the (true, true) arm's stub error
        // proves the manifest loaded AND path-safety passed. Without
        // this, a manifest-load failure would silently satisfy the
        // negative assertions below and give false-positive coverage.
        assert!(
            err.contains("local dry-run staging lands in Task 10/11"),
            "must reach dispatch matrix, not fail on manifest load: {err}"
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

        let err = run_provision(&ProvisionArgs {
            adapter: "axum".to_owned(),
            dry_run: true,
            local: true,
            manifest: manifest_path.clone(),
        })
        .expect_err("must reach the (true, true) dispatch stub");
        assert!(
            err.contains("local dry-run staging lands in Task 10/11"),
            "must reach dispatch matrix, not fail on manifest load: {err}"
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
}
