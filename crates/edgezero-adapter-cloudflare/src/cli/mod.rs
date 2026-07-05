#![expect(
    clippy::mod_module_files,
    reason = "Workspace lint policy denies BOTH `self_named_module_files` (wants `cli/mod.rs`) and `mod_module_files` (wants `cli.rs`) -- they contradict, so any file with submodules must opt out of one. This crate's cli directory uses the `cli/mod.rs` form; allow accordingly."
)]
#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "submodule declarations sit between the `use` block and the rest of the file's items by Rust convention; the strict-ordering lint disagrees but no human convention puts `mod` blocks AFTER trait impls"
)]

use std::path::{Path, PathBuf};

use ctor::ctor;
use edgezero_adapter::cli_support::run_native_cli;
use edgezero_adapter::env_file::{append_lines_dedup_with_header, EDGEZERO_PROVISION_HEADER};
use edgezero_adapter::registry::{
    register_adapter, Adapter, AdapterAction, AdapterDeployedState, AdapterPushContext,
    ProvisionMode, ProvisionOutcome, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
    TypedSecretEntry,
};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};

mod provision_cloud;
mod provision_local;
mod push_cloud;
mod run;

static CLOUDFLARE_ADAPTER: CloudflareCliAdapter = CloudflareCliAdapter;

static CLOUDFLARE_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "cloudflare",
    display_name: "Cloudflare Workers",
    crate_suffix: "adapter-cloudflare",
    dependency_crate: "edgezero-adapter-cloudflare",
    dependency_repo_path: "crates/edgezero-adapter-cloudflare",
    template_registrations: CLOUDFLARE_TEMPLATE_REGISTRATIONS,
    files: CLOUDFLARE_FILE_SPECS,
    extra_dirs: &["src", ".cargo"],
    dependencies: CLOUDFLARE_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "wrangler.toml",
        build_target: "wasm32-unknown-unknown",
        build_profile: "release",
        build_features: &["cloudflare"],
    },
    commands: CommandTemplates {
        build: "wrangler build --cwd {crate_dir}",
        deploy: "wrangler deploy --cwd {crate_dir}",
        serve: "wrangler dev --cwd {crate_dir}",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`edgezero serve --adapter cloudflare`"],
    },
    run_module: "edgezero_adapter_cloudflare",
};

static CLOUDFLARE_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_cloudflare",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_cloudflare",
        repo_crate: "crates/edgezero-adapter-cloudflare",
        fallback:
            "edgezero-adapter-cloudflare = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-cloudflare\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_cloudflare_wasm",
        repo_crate: "crates/edgezero-adapter-cloudflare",
        fallback:
            "edgezero-adapter-cloudflare = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-cloudflare\", default-features = false, features = [\"cloudflare\"] }",
        features: &["cloudflare"],
    },
];

static CLOUDFLARE_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "cf_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "cf_src_lib_rs",
        output: "src/lib.rs",
    },
    AdapterFileSpec {
        template: "cf_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "cf_cargo_config_toml",
        output: ".cargo/config.toml",
    },
    AdapterFileSpec {
        template: "cf_wrangler_toml",
        output: "wrangler.toml",
    },
];

static CLOUDFLARE_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "cf_Cargo_toml",
        contents: include_str!("../templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_lib_rs",
        contents: include_str!("../templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_main_rs",
        contents: include_str!("../templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_cargo_config_toml",
        contents: include_str!("../templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_wrangler_toml",
        contents: include_str!("../templates/wrangler.toml.hbs"),
    },
];

pub(super) const TARGET_TRIPLE: &str = "wasm32-unknown-unknown";

pub(super) const WRANGLER_INSTALL_HINT: &str =
    "install the Cloudflare CLI (`npm install -g wrangler`) and try again";

struct CloudflareCliAdapter;

#[expect(
    clippy::missing_trait_methods,
    reason = "cloudflare has no validate_app_config_keys / validate_adapter_manifest / validate_typed_secrets requirements; those three trait defaults are intentionally inherited. `read_config_entry` and `read_config_entry_local` are both overridden below (wrangler kv key get --remote / --local). `single_store_kinds` IS overridden below (returns `&[\"secrets\"]`). `synthesise_baseline_manifest` IS overridden below (emits a baseline `wrangler.toml` for the Task 8b clean-clone bootstrap). `provision_typed` IS overridden below (appends `<key_value>=\"\"` secret placeholders to `.dev.vars` in Local mode; Cloud is a no-op — `wrangler secret put` is the remote path)."
)]
impl Adapter for CloudflareCliAdapter {
    fn deployed_fields(&self) -> &'static [&'static str] {
        &["kv_namespaces", "preview_kv_namespaces"]
    }

    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `wrangler` is the native sign-in surface for Cloudflare
            // Workers. EdgeZero stores no credentials — this is a thin
            // shell-out.
            AdapterAction::AuthLogin => {
                run_native_cli("wrangler", &["login"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("wrangler", &["logout"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("wrangler", &["whoami"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::Build => run::build(args).map(|artifact| {
                log::info!(
                    "[edgezero] Cloudflare build artifact -> {}",
                    artifact.display()
                );
            }),
            AdapterAction::Deploy => run::deploy(args),
            AdapterAction::Serve => run::serve(args),
            other => Err(format!("cloudflare adapter does not support {other:?}")),
        }
    }

    fn merged_id_kinds(&self) -> &'static [&'static str] {
        // Both KV and Config back to Worker KV namespaces via the
        // same `[[kv_namespaces]] binding = <platform-name>`
        // wrangler.toml entry. Declaring the same logical id under
        // both kinds (e.g. `[stores.kv].ids = ["x"]` AND
        // `[stores.config].ids = ["x"]`) resolves to a SINGLE
        // underlying KV namespace at runtime — KV writes from the
        // app silently clobber config-shaped entries (and vice
        // versa). Provision compounds the hazard: the second
        // binding would already be present from the first kind's
        // `upsert_kv_namespace` and get reported as "already
        // provisioned" instead of failing the collision.
        //
        // CLI `config validate` rejects this collision before any
        // wrangler shell-out happens.
        &["kv", "config"]
    }

    fn name(&self) -> &'static str {
        "cloudflare"
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
            ProvisionMode::Cloud => {
                provision_cloud::provision(manifest_root, adapter_manifest_path, stores, dry_run)
            }
            // ProvisionMode is #[non_exhaustive]; a future mode variant
            // gets an explicit error so we don't accidentally dispatch
            // via one of the two known arms.
            other => Err(format!(
                "cloudflare adapter does not implement provision mode {other:?}"
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
        // Cloud is a no-op: `wrangler secret put` is the tool for
        // remote secret upload. `provision_typed` handles ONLY the
        // local preview writeback — a `<key_value>=""` placeholder
        // per typed field, appended to the SAME `.dev.vars` file
        // `provision_local` seeds with `EDGEZERO__STORES__…__NAME` /
        // `__KEY` overlays.
        if !matches!(mode, ProvisionMode::Local) {
            return Ok(ProvisionOutcome::default());
        }
        // Anchor `.dev.vars` on the RESOLVED wrangler.toml path so
        // nested layouts (e.g. `adapter_manifest_path =
        // "crates/app-demo-adapter-cloudflare/wrangler.toml"`) land
        // the file in the same crate dir wrangler dev reads from,
        // NOT at `manifest_root/.dev.vars`. Mirrors the placement
        // `provision_local` uses for the __NAME / __KEY lines.
        let wrangler_rel = adapter_manifest_path.unwrap_or("wrangler.toml");
        let wrangler_path = manifest_root.join(wrangler_rel);
        let dev_vars_path = wrangler_path
            .parent()
            .unwrap_or(manifest_root)
            .join(".dev.vars");
        let lines: Vec<String> = typed_secrets
            .iter()
            .map(|entry| format!(r#"{}="""#, entry.key_value))
            .collect();
        append_lines_dedup_with_header(
            &dev_vars_path,
            Some(EDGEZERO_PROVISION_HEADER),
            &lines,
            dry_run,
        )
        .map_err(|err| format!("write {}: {err}", dev_vars_path.display()))?;
        let status_lines = vec![format!(
            "cloudflare: wrote {} secret placeholders to {}",
            typed_secrets.len(),
            dev_vars_path.display()
        )];
        Ok(ProvisionOutcome::from_status_lines(status_lines))
    }

    fn push_config_entries(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        push_cloud::write_entries(
            manifest_root,
            adapter_manifest_path,
            store,
            entries,
            dry_run,
        )
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
        push_cloud::write_entries_local(
            manifest_root,
            adapter_manifest_path,
            store,
            entries,
            dry_run,
        )
    }

    fn read_config_entry(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        push_cloud::read_wrangler_kv_key(
            manifest_root,
            adapter_manifest_path,
            store,
            key,
            "--remote",
        )
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
        push_cloud::read_wrangler_kv_key(
            manifest_root,
            adapter_manifest_path,
            store,
            key,
            "--local",
        )
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: cloudflare is Multi for KV (KV namespaces) and
        // Config (KV namespaces), Single for Secrets (Worker
        // Secrets is a single flat bag).
        &["secrets"]
    }

    fn synthesise_baseline_manifest(
        &self,
        _manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        app_name: &str,
        _deployed: Option<&AdapterDeployedState>,
    ) -> Result<Vec<(PathBuf, String)>, String> {
        let rel =
            adapter_manifest_path.map_or_else(|| PathBuf::from("wrangler.toml"), PathBuf::from);
        Ok(vec![(rel, run::synthesise_wrangler_toml(app_name))])
    }
}

#[inline]
pub fn register() {
    register_adapter(&CLOUDFLARE_ADAPTER);
    register_adapter_blueprint(&CLOUDFLARE_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

// Shared process-wide mutex serialising PATH-mutating tests across every
// submodule test suite in this crate. Tests in `provision_local`, `provision_cloud`,
// and `push_cloud` all install shell shims via `PathPrepend` and would otherwise
// race on the environment variable.
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
    use std::fs;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` per call site) keeps the
    // setup-vs-assertion pair in sync -- a typo in one place no
    // longer silently divorces from the other, because both reference
    // the same const. Also names the intent: these are the LOGICAL
    // store ids the cloudflare adapter operates on, not arbitrary
    // strings.
    const TEST_SECRET_ID: &str = "default";

    // ---------- provision_typed (Local mode) — secret placeholders ----------

    #[test]
    fn cloudflare_provision_typed_appends_secret_placeholders_to_dev_vars() {
        // Fixture: nested wrangler.toml layout matching app-demo.
        // provision_typed writes `<key_value>=""` per entry into the
        // `.dev.vars` NEXT TO the wrangler manifest (append_lines_dedup
        // creates parent dirs, so no pre-seed of the wrangler.toml is
        // required for this test).
        let dir = tempdir().expect("tempdir");
        let entries = [TypedSecretEntry::new(
            TEST_SECRET_ID,
            "api_token",
            "demo_api_token",
        )];
        let outcome = CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let dev_vars_path = dir.path().join("crates/cf/.dev.vars");
        assert!(
            dev_vars_path.exists(),
            ".dev.vars exists at nested path: {}",
            dev_vars_path.display()
        );
        let dev_vars = fs::read_to_string(&dev_vars_path).expect("read .dev.vars");
        assert!(
            dev_vars.contains(r#"demo_api_token="""#),
            "placeholder line present: {dev_vars}"
        );
        assert!(
            outcome
                .status_lines
                .iter()
                .any(|line| line.contains(&dev_vars_path.display().to_string())),
            "status line names the .dev.vars path: {:?}",
            outcome.status_lines
        );
        assert!(
            outcome.deployed.is_none(),
            "local provision_typed returns no deployed state"
        );
    }

    #[test]
    fn cloudflare_provision_typed_dev_vars_lands_next_to_wrangler_toml() {
        // Locks the `wrangler_path.parent().join(".dev.vars")`
        // anchor against drift: with `adapter_manifest_path =
        // "crates/cf/wrangler.toml"`, `.dev.vars` MUST land at
        // `temp/crates/cf/.dev.vars` and NOT at `temp/.dev.vars`.
        let dir = tempdir().expect("tempdir");
        let entries = [TypedSecretEntry::new(
            TEST_SECRET_ID,
            "api_token",
            "demo_api_token",
        )];
        CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        assert!(
            dir.path().join("crates/cf/.dev.vars").exists(),
            ".dev.vars anchored on wrangler.toml parent"
        );
        assert!(
            !dir.path().join(".dev.vars").exists(),
            "root-level .dev.vars must NOT be written"
        );
    }

    #[test]
    fn cloudflare_provision_typed_cloud_mode_is_a_no_op() {
        // Cloud is a no-op: `wrangler secret put` is the remote
        // path. Empty outcome, no `.dev.vars` written anywhere.
        let dir = tempdir().expect("tempdir");
        let entries = [TypedSecretEntry::new(
            TEST_SECRET_ID,
            "api_token",
            "demo_api_token",
        )];
        let outcome = CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Cloud,
                false,
            )
            .expect("provision_typed Cloud succeeds");
        assert!(
            outcome.status_lines.is_empty(),
            "cloud mode emits no status lines: {:?}",
            outcome.status_lines
        );
        assert!(
            outcome.deployed.is_none(),
            "cloud mode returns no deployed state"
        );
        assert!(
            !dir.path().join("crates/cf/.dev.vars").exists(),
            "cloud mode must NOT touch .dev.vars"
        );
        assert!(
            !dir.path().join(".dev.vars").exists(),
            "cloud mode must NOT touch .dev.vars at manifest_root either"
        );
    }

    #[test]
    fn cloudflare_provision_typed_deduplicates_against_existing_dev_vars() {
        // Operator has already filled in the real value. Re-running
        // provision_typed must NOT clobber it with the empty
        // placeholder — append_lines_dedup collapses keys.
        let dir = tempdir().expect("tempdir");
        let dev_vars_dir = dir.path().join("crates/cf");
        fs::create_dir_all(&dev_vars_dir).expect("mkdir nested");
        let dev_vars_path = dev_vars_dir.join(".dev.vars");
        fs::write(&dev_vars_path, "demo_api_token=\"already_set\"\n").expect("seed .dev.vars");
        let entries = [TypedSecretEntry::new(
            TEST_SECRET_ID,
            "api_token",
            "demo_api_token",
        )];
        CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("crates/cf/wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed succeeds");
        let dev_vars = fs::read_to_string(&dev_vars_path).expect("read .dev.vars");
        assert!(
            dev_vars.contains(r#"demo_api_token="already_set""#),
            "operator's real value survives: {dev_vars}"
        );
        assert!(
            !dev_vars.contains(r#"demo_api_token="""#),
            "empty-value placeholder must NOT be appended: {dev_vars}"
        );
        let token_lines = dev_vars
            .lines()
            .filter(|line| {
                let after_hash = line.trim_start().strip_prefix('#').unwrap_or(line);
                after_hash.trim_start().starts_with("demo_api_token=")
            })
            .count();
        assert_eq!(
            token_lines, 1,
            "exactly one demo_api_token line remains: {dev_vars}"
        );
    }

    #[test]
    fn provision_local_push_after_provision_preserves_dev_vars_secret_value() {
        // First run seeds `SECRET_KEY=""` (empty placeholder) into
        // `.dev.vars`. The operator hand-edits the file to
        // `SECRET_KEY="real_value_operator_set"`. A subsequent
        // `provision_typed` MUST NOT overwrite the operator's value
        // with the empty placeholder — append_lines_dedup collapses
        // commented + uncommented forms by normalised key, so the
        // uncommented real value survives byte-for-byte.
        let dir = tempdir().expect("tempdir");
        let entries = [TypedSecretEntry::new(
            TEST_SECRET_ID,
            "api_token",
            "SECRET_KEY",
        )];
        CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("first provision_typed writes empty placeholder");
        let dev_vars_path = dir.path().join(".dev.vars");
        let first = fs::read_to_string(&dev_vars_path).expect("read .dev.vars (first run)");
        assert!(
            first.contains(r#"SECRET_KEY="""#),
            "empty placeholder present after first run: {first}"
        );
        // Simulate the operator's hand-edit. Rewrite just the
        // SECRET_KEY line; everything else stays as provision wrote it.
        let edited = first.replace(
            r#"SECRET_KEY="""#,
            r#"SECRET_KEY="real_value_operator_set""#,
        );
        assert_ne!(edited, first, "operator edit actually mutated the file");
        fs::write(&dev_vars_path, &edited).expect("operator hand-edit");
        CloudflareCliAdapter
            .provision_typed(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .expect("re-run provision_typed after operator edit");
        let after = fs::read_to_string(&dev_vars_path).expect("read .dev.vars (second run)");
        assert!(
            after.contains(r#"SECRET_KEY="real_value_operator_set""#),
            "operator's value survives byte-for-byte: {after}"
        );
        assert!(
            !after.contains(r#"SECRET_KEY="""#),
            "empty placeholder must NOT be re-appended: {after}"
        );
        // Exactly one SECRET_KEY line remains after dedup.
        let key_lines = after
            .lines()
            .filter(|line| {
                let after_hash = line.trim_start().strip_prefix('#').unwrap_or(line);
                after_hash.trim_start().starts_with("SECRET_KEY=")
            })
            .count();
        assert_eq!(
            key_lines, 1,
            "exactly one SECRET_KEY line remains after dedup: {after}"
        );
    }
}
