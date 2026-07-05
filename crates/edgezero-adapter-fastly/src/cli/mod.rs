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
mod push_local;
mod run;

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
        fallback:
            "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly_wasm",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback:
            "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false, features = [\"fastly\"] }",
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
    TemplateRegistration {
        name: "fastly_fastly_toml",
        contents: include_str!("../templates/fastly.toml.hbs"),
    },
];

pub(super) const FASTLY_INSTALL_HINT: &str =
    "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

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
    reason = "see the explanatory block comment immediately above; fastly's no-op defaults for the three validate_* hooks are intentional and documented. `read_config_entry` and `read_config_entry_local` are both overridden below. `single_store_kinds` IS overridden below (returns `&[]`). `synthesise_baseline_manifest` IS overridden below (emits a baseline `fastly.toml` for the Task 8b clean-clone bootstrap, threading `[adapters.fastly.deployed].service_id` through when present). `provision_typed` IS overridden below (Local mode appends `[[local_server.secret_stores.<store_id>]]` entries in `fastly.toml`; Cloud is a no-op)."
)]
impl Adapter for FastlyCliAdapter {
    fn deployed_fields(&self) -> &'static [&'static str] {
        &["service_id"]
    }

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
                let artifact = run::build(args)?;
                log::info!("[edgezero] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => run::deploy(args),
            AdapterAction::Serve => run::serve(args),
            other => Err(format!("fastly adapter does not support {other:?}")),
        }
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
        _manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        app_name: &str,
        deployed: Option<&AdapterDeployedState>,
    ) -> Result<Vec<(PathBuf, String)>, String> {
        // The CLI's `deployed_state_for` translator (Task 8b) copies
        // `[adapters.fastly.deployed].service_id` into
        // `deployed.fields["service_id"]` before calling this override,
        // so the adapter reads the flat field bag and never links to
        // `edgezero-core`.
        let deployed_service_id = deployed
            .and_then(|state| state.fields.get("service_id"))
            .map(String::as_str);
        let rel = adapter_manifest_path.map_or_else(|| PathBuf::from("fastly.toml"), PathBuf::from);
        Ok(vec![(
            rel,
            run::synthesise_fastly_toml(app_name, deployed_service_id),
        )])
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
