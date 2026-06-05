//! `provision` command.
//!
//! Thin delegate to the adapter registry. The CLI loads the manifest,
//! resolves the named adapter, hands it the declared store ids per
//! kind, and prints the human-readable status lines the adapter
//! returns. All the platform-specific work — `wrangler kv namespace
//! create`, `fastly *-store create`, `spin.toml` editing — lives in
//! each `edgezero-adapter-*` crate's `Adapter::provision` impl, not
//! here.

use std::path::Path;

use crate::args::ProvisionArgs;
use crate::config::{
    enforce_single_store_capability, reject_merged_id_collisions, strict_handler_paths,
};
use crate::ensure_adapter_defined;
use edgezero_adapter::registry::{self as adapter_registry, ProvisionStores, ResolvedStoreId};
use edgezero_core::env_config::EnvConfig;
use edgezero_core::manifest::{ManifestLoader, StoreDeclaration};

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
    let adapter_cfg = manifest.adapters.get(&args.adapter).ok_or_else(|| {
        format!(
            "adapter `{}` is not declared in {}",
            args.adapter,
            args.manifest.display()
        )
    })?;

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
    adapter.validate_adapter_manifest(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
    )?;

    // Resolve each logical store id to its platform name via the
    // same `EDGEZERO__STORES__<KIND>__<ID>__NAME` env overlay the
    // runtime reads. Provision writes the PLATFORM name into the
    // per-platform manifest (wrangler.toml, spin.toml,
    // fastly.toml); the logical id stays available for status-line
    // wording so operators see what they declared even when the
    // env override redirected the create.
    let env_config = EnvConfig::from_env();

    // Same env-resolved merged-id collision check `config validate`
    // runs. Without it, `provision --adapter spin --dry-run` would
    // happily ack a manifest where (e.g.) [stores.kv].sessions and
    // [stores.config].app_config both resolve to platform label
    // `shared` via the env overlay -- both writes would silently
    // land on the same Spin KV store at runtime. Catches BOTH
    // logical-id collisions and env-resolved platform-label
    // collisions across merged kinds.
    reject_merged_id_collisions(&args.adapter, adapter, manifest, &env_config)?;

    let config_ids = resolve_kind(manifest.stores.config.as_ref(), &env_config, "config");
    let kv_ids = resolve_kind(manifest.stores.kv.as_ref(), &env_config, "kv");
    let secret_ids = resolve_kind(manifest.stores.secrets.as_ref(), &env_config, "secrets");
    let stores = ProvisionStores {
        config: &config_ids,
        kv: &kv_ids,
        secrets: &secret_ids,
    };

    let lines = adapter.provision(
        manifest_root,
        adapter_cfg.adapter.manifest.as_deref(),
        adapter_cfg.adapter.component.as_deref(),
        &stores,
        args.dry_run,
    )?;

    if args.dry_run {
        log::info!("[edgezero] provision --dry-run for `{}`:", args.adapter);
    }
    for line in lines {
        log::info!("{line}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ProvisionArgs;
    use crate::test_support::{manifest_guard, EnvOverride, PROVISION_MANIFEST};
    use std::fs;
    use tempfile::TempDir;

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
            manifest: manifest_path.clone(),
        })
        .expect("fastly dry-run dispatches cleanly");
    }
}
