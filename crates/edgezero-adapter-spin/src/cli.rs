#![expect(
    clippy::self_named_module_files,
    reason = "Workspace lint policy denies BOTH `self_named_module_files` (wants `cli/mod.rs`) and `mod_module_files` (wants `cli.rs`) -- they contradict, so any file with submodules must opt out of one. The repo convention is the self-named form (`cli.rs` with submodules under `cli/`); allow accordingly."
)]
#![expect(
    clippy::arbitrary_source_item_ordering,
    reason = "submodule declarations sit between the `use` block and the rest of the file's items by Rust convention; the strict-ordering lint disagrees but no human convention puts `mod` blocks AFTER trait impls"
)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    Adapter, AdapterAction, AdapterPushContext, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
    TypedSecretEntry, register_adapter,
};
use edgezero_adapter::scaffold::{
    AdapterBlueprint, AdapterFileSpec, CommandTemplates, DependencySpec, LoggingDefaults,
    ManifestSpec, ReadmeInfo, TemplateRegistration, register_adapter_blueprint,
};
use walkdir::WalkDir;

mod push_cloud;
mod push_sqlite;
mod runtime_config;

static SPIN_ADAPTER: SpinCliAdapter = SpinCliAdapter;

static SPIN_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "spin",
    display_name: "Spin (Fermyon)",
    crate_suffix: "adapter-spin",
    dependency_crate: "edgezero-adapter-spin",
    dependency_repo_path: "crates/edgezero-adapter-spin",
    template_registrations: SPIN_TEMPLATE_REGISTRATIONS,
    files: SPIN_FILE_SPECS,
    extra_dirs: &["src"],
    dependencies: SPIN_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "spin.toml",
        build_target: "wasm32-wasip2",
        build_profile: "release",
        build_features: &["spin"],
    },
    commands: CommandTemplates {
        build: "cargo build --target wasm32-wasip2 --release -p {crate}",
        deploy: "spin deploy --from {crate_dir}",
        serve: "spin up --from {crate_dir} --runtime-config-file {crate_dir}/runtime-config.toml",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`edgezero serve --adapter spin`"],
    },
    run_module: "edgezero_adapter_spin",
};

static SPIN_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_spin",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback: "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin_wasm",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback: "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false, features = [\"spin\"] }",
        features: &["spin"],
    },
];

static SPIN_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "spin_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "spin_runtime_config_toml",
        output: "runtime-config.toml",
    },
    AdapterFileSpec {
        template: "spin_src_lib_rs",
        output: "src/lib.rs",
    },
    AdapterFileSpec {
        template: "spin_spin_toml",
        output: "spin.toml",
    },
];

static SPIN_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "spin_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_runtime_config_toml",
        contents: include_str!("templates/runtime-config.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_src_lib_rs",
        contents: include_str!("templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "spin_spin_toml",
        contents: include_str!("templates/spin.toml.hbs"),
    },
];

const TARGET_TRIPLE: &str = "wasm32-wasip2";

const SPIN_INSTALL_HINT: &str = "install the Spin CLI (https://spinframework.dev/) and try again";

struct SpinCliAdapter;

#[expect(
    clippy::missing_trait_methods,
    reason = "Stage 6: KV-backed config dropped Spin's `^[a-z][a-z0-9_]*$` key rule and the config-vs-secret collision check, so `validate_app_config_keys` falls back to the trait default `Ok(())`. `validate_typed_secrets` IS overridden below (secret-value canonicalisation + within-secrets uniqueness still apply). `validate_adapter_manifest` IS overridden below (Spin's multi-component disambiguation). `read_config_entry` and `read_config_entry_local` are both overridden below (four-branch SQLite-direct / Fermyon Cloud / non-Spin-backend dispatch)."
)]
impl Adapter for SpinCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `spin cloud {login|logout|info}` is the native sign-in
            // surface for Fermyon Cloud. EdgeZero stores no
            // credentials — this is a thin shell-out.
            AdapterAction::AuthLogin => {
                run_native_cli("spin", &["cloud", "login"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("spin", &["cloud", "logout"], SPIN_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("spin", &["cloud", "info"], SPIN_INSTALL_HINT)
            }
            AdapterAction::Build => {
                let artifact = build(args)?;
                log::info!("[edgezero] Spin build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("spin adapter does not support {other:?}")),
        }
    }

    fn merged_id_kinds(&self) -> &'static [&'static str] {
        // Both KV and Config back to `spin_sdk::key_value::Store` via
        // the same `provision` path; declaring the same logical id
        // under both kinds resolves to one underlying store with
        // silent write-collisions. CLI validate rejects.
        &["kv", "config"]
    }

    fn name(&self) -> &'static str {
        "spin"
    }

    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: spin provision is pure spin.toml editing — no
        // shell-out (Spin KV stores are provisioned by the Spin
        // runtime / Fermyon at deploy). For each declared KV id
        // AND each declared CONFIG id (KV-backed since Stage 5
        // of the spin-kv-config plan), append the env-resolved
        // platform label to the component's `key_value_stores`
        // array. Secret variables are manually declared by the
        // developer in spin.toml -- secrets stay on Spin
        // variables for the platform's `secret = true` flagging.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for provision".to_owned(),
            );
        };
        let spin_path = manifest_root.join(rel);

        let mut out = Vec::new();
        // Resolve the component once if either KV or config has
        // anything to provision.
        let needs_component = !stores.kv.is_empty() || !stores.config.is_empty();
        if needs_component {
            let component_id = resolve_spin_component(&spin_path, component_selector)?;
            for (kind, store) in stores
                .kv
                .iter()
                .map(|store| ("KV", store))
                .chain(stores.config.iter().map(|store| ("config", store)))
            {
                let logical = store.logical.as_str();
                // The label the runtime opens is what
                // `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`
                // resolves to (default = the logical id). Provision
                // writes the PLATFORM label into
                // `[component.X].key_value_stores` so that both the
                // KV runtime lookup AND the KV-backed config
                // runtime lookup match.
                let label = store.platform.as_str();
                if dry_run {
                    out.push(format!(
                        "would ensure {kind} label `{label}` (logical id `{logical}`) is in [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                    continue;
                }
                let added = ensure_kv_label_in_component(&spin_path, &component_id, label)?;
                if added {
                    out.push(format!(
                        "added {kind} label `{label}` (logical id `{logical}`) to [component.{component_id}].key_value_stores in {}",
                        spin_path.display()
                    ));
                } else {
                    out.push(format!(
                        "{kind} label `{label}` (logical id `{logical}`) already present in [component.{component_id}].key_value_stores in {}; skipping",
                        spin_path.display()
                    ));
                }
            }
        }
        for store in stores.secrets {
            let logical = store.logical.as_str();
            let platform = store.platform.as_str();
            out.push(format!(
                "spin secret id `{logical}` (platform name `{platform}`) requires manual `[variables].* secret = true` + `[component.*.variables].*` declarations in spin.toml; nothing to do here"
            ));
        }
        if out.is_empty() {
            out.push("spin has no declared stores to provision".to_owned());
        }
        Ok(out)
    }

    fn push_config_entries(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        dispatch_push(
            manifest_root,
            adapter_manifest_path,
            store,
            entries,
            push_ctx,
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
        push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // `--local` lives in `push_ctx.local`. `dispatch_push` honours
        // it by suppressing the Fermyon Cloud auto-detect so the
        // operator can force a SQLite-direct write even when the
        // manifest's deploy command shells to `spin deploy`.
        dispatch_push(
            manifest_root,
            adapter_manifest_path,
            store,
            entries,
            push_ctx,
            dry_run,
        )
    }

    fn read_config_entry(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        // Four-branch dispatch mirroring `dispatch_push`:
        //
        // 1. `push_ctx.local` → delegate to `read_config_entry_local`
        //    (SQLite-direct, same as the `--local` write path).
        // 2. Deploy command targets Fermyon Cloud → `Unsupported`.
        //    Fermyon Cloud's `spin cloud key-value list` enumerates
        //    STORES, not keys; there is no stable per-key get CLI in
        //    v1 (8.3 / 9.4 of the spec). NO shell-out.
        // 3. `runtime-config.toml` declares a non-`spin` backend
        //    (Redis / AzureCosmos / Unknown) → error naming the backend
        //    and pointing at its native CLI, matching the writer at
        //    `cli.rs:639-650`.
        // 4. Default / `type = "spin"` → SQLite-direct read.
        //
        // Errors from `runtime_config::read` and from
        // `verify_label_declared` are PROPAGATED (not swallowed with
        // `.ok()`). Silently falling through on a malformed
        // runtime-config would let `config diff` report a result on a
        // tree where the writer would have errored hard.
        if push_ctx.local {
            return self.read_config_entry_local(
                manifest_root,
                adapter_manifest_path,
                component_selector,
                store,
                key,
                push_ctx,
            );
        }

        let spin_manifest_path = adapter_manifest_path
            .map(|rel| manifest_root.join(rel))
            .ok_or_else(|| {
                "[adapters.spin.adapter].manifest must point at spin.toml for config diff"
                    .to_owned()
            })?;
        let spin_manifest_dir = spin_manifest_path.parent().unwrap_or(manifest_root);
        let runtime_config_path = push_ctx.runtime_config_path.map_or_else(
            || spin_manifest_dir.join("runtime-config.toml"),
            Path::to_path_buf,
        );
        let runtime_config_dir = runtime_config_path.parent().unwrap_or(spin_manifest_dir);
        let platform = store.platform.as_str();

        // Branch 2: Fermyon Cloud auto-detect.
        if push_cloud::deploy_command_targets_fermyon_cloud(push_ctx.manifest_adapter_deploy_cmd) {
            return Ok(ReadConfigEntry::Unsupported(
                "Spin Cloud key-value CLI exposes no `get`; remote read-back unsupported in v1",
            ));
        }

        // Branches 3 + 4: parse runtime-config, propagating parse errors,
        // then dispatch on backend type.
        let parsed = runtime_config::read(&runtime_config_path)?;
        verify_label_declared(platform, &parsed, &runtime_config_path)?;
        let backend = parsed.key_value_stores.get(platform);
        match backend {
            Some(runtime_config::KeyValueBackend::Redis { url }) => Err(format!(
                "store `{platform}` is backed by `type = \"redis\"` (url: `{url}`) in {}; \
                 use `redis-cli -u {url} GET <key>` to read entries from this store. \
                 edgezero does not read from redis backends.",
                runtime_config_path.display()
            )),
            Some(runtime_config::KeyValueBackend::AzureCosmos) => Err(format!(
                "store `{platform}` is backed by `type = \"azure_cosmos\"` in {}; \
                 use the Azure CosmosDB CLI to read this store. \
                 edgezero does not read from azure_cosmos backends.",
                runtime_config_path.display()
            )),
            Some(runtime_config::KeyValueBackend::Unknown { type_name }) => Err(format!(
                "store `{platform}` is backed by an unrecognised type `{type_name}` in {}; \
                 edgezero only reads from `type = \"spin\"` (SQLite) backends.",
                runtime_config_path.display()
            )),
            // Branch 4: `type = "spin"` or missing stanza (default).
            Some(runtime_config::KeyValueBackend::Spin { path }) => {
                let db_path = push_sqlite::resolve_sqlite_path(
                    spin_manifest_dir,
                    runtime_config_dir,
                    path.as_deref(),
                );
                read_sqlite_entry(&db_path, platform, key)
            }
            None => {
                let db_path =
                    push_sqlite::resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, None);
                read_sqlite_entry(&db_path, platform, key)
            }
        }
    }

    fn read_config_entry_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        // Branch 1: `--local` forces SQLite-direct regardless of the
        // runtime-config backend type or the Fermyon Cloud auto-detect.
        // Mirrors the write path at `dispatch_push` branch 1 (cli.rs:572).
        //
        // We still enforce that any non-`default` label is declared in
        // `runtime-config.toml` (same invariant as the writer) so the
        // read path can't silently succeed on a tree where `spin up`
        // would error with "unknown key_value_stores label X".
        //
        // An explicit `--runtime-config <path>` is honoured for path
        // resolution; the backend `type` is ignored (SQLite is always
        // the target for `--local`).
        let spin_manifest_path = adapter_manifest_path
            .map(|rel| manifest_root.join(rel))
            .ok_or_else(|| {
                "[adapters.spin.adapter].manifest must point at spin.toml for config diff --local"
                    .to_owned()
            })?;
        let spin_manifest_dir = spin_manifest_path.parent().unwrap_or(manifest_root);
        let runtime_config_path = push_ctx.runtime_config_path.map_or_else(
            || spin_manifest_dir.join("runtime-config.toml"),
            Path::to_path_buf,
        );
        let runtime_config_dir = runtime_config_path.parent().unwrap_or(spin_manifest_dir);
        let platform = store.platform.as_str();

        // Parse runtime-config (propagating errors).
        let parsed = runtime_config::read(&runtime_config_path)?;
        verify_label_declared(platform, &parsed, &runtime_config_path)?;

        // Resolve the SQLite path: honour any explicit `path` in a
        // `type = "spin"` stanza; fall back to Spin's default otherwise
        // (matches the write path at dispatch_push branch 1).
        let explicit_path = match parsed.key_value_stores.get(platform) {
            Some(runtime_config::KeyValueBackend::Spin { path }) => path.as_deref(),
            _ => None,
        };
        let db_path =
            push_sqlite::resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, explicit_path);
        read_sqlite_entry(&db_path, platform, key)
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: Multi for KV AND Config (both label-backed via the
        // Spin KV API since Stage 5 of the spin-kv-config plan).
        // Single for Secrets (still flat-variable namespace).
        &["secrets"]
    }

    fn validate_adapter_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
    ) -> Result<(), String> {
        // check 3: spin.toml must exist and either declare
        // exactly one `[component.*]` or carry an explicit selector
        // that matches one of the declared ids.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.spin.adapter].manifest must point at spin.toml for Spin component discovery".to_owned()
            );
        };
        let spin_path = manifest_root.join(rel);
        let raw = fs::read_to_string(&spin_path).map_err(|err| {
            format!(
                "failed to read spin manifest at {}: {err}",
                spin_path.display()
            )
        })?;
        let parsed: toml::Value = toml::from_str(&raw)
            .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
        let component_ids = collect_spin_component_ids(&parsed);

        if component_ids.is_empty() {
            return Err(format!(
                "{}: no [component.*] declarations found",
                spin_path.display()
            ));
        }

        if let Some(selector) = component_selector {
            if component_ids.iter().any(|id| id == selector) {
                return Ok(());
            }
            return Err(format!(
                "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
                selector,
                spin_path.display(),
                component_ids.join(", ")
            ));
        }

        if component_ids.len() == 1 {
            return Ok(());
        }
        Err(format!(
            "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
            spin_path.display(),
            component_ids.len(),
            component_ids.join(", ")
        ))
    }

    fn validate_typed_secrets(&self, entries: &[TypedSecretEntry<'_>]) -> Result<(), String> {
        use std::collections::HashMap;
        // Stage 5+: KV-backed config no longer shares Spin's flat
        // variable namespace, so config keys are NOT considered here
        // (and the trait dropped the parameter in Stage 6+) — config
        // can use arbitrary UTF-8 keys without colliding with
        // `#[secret]` values. Secrets still resolve through
        // `spin_sdk::variables`, so two checks remain:
        //   1. each `#[secret]` value canonicalises (lowercase, no
        //      `.→__` — secrets don't get translated at runtime)
        //      to a valid Spin variable name, so invalid chars
        //      (dashes, digit-first) fail validation rather than
        //      at runtime with an opaque `InvalidName`;
        //   2. no two `#[secret]` values collapse to the same
        //      lowercased Spin variable, since Spin's flat
        //      namespace cannot disambiguate them.
        // Map lowercased-Spin-variable → original field name. When a
        // second entry collapses to the same name, the existing entry
        // tells us which field already claimed it.
        let mut seen: HashMap<String, &str> = HashMap::with_capacity(entries.len());
        for entry in entries {
            let spin_var = entry.key_value.to_ascii_lowercase();
            if !is_valid_spin_key(&spin_var) {
                let reason = spin_key_rule_violation(&spin_var);
                return Err(format!(
                    "`#[secret]` field `{field}` value `{value}` translates to Spin variable `{spin_var}`, which is not a valid Spin variable name. {reason}. Pick a `#[secret]` value that conforms.",
                    field = entry.field_name,
                    value = entry.key_value,
                ));
            }
            if let Some(prev_field) = seen.insert(spin_var.clone(), entry.field_name.as_str()) {
                return Err(format!(
                    "Spin variable `{spin_var}` would receive values from BOTH `#[secret]` field `{prev_field}` AND `#[secret]` field `{this_field}`; Spin's flat variable namespace cannot disambiguate them. Pick distinct `#[secret]` values whose lowercased forms differ.",
                    this_field = entry.field_name,
                ));
            }
        }
        Ok(())
    }
}

fn is_valid_spin_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Return a per-failure-mode diagnostic for a key that failed
/// `is_valid_spin_key`. Spin's variable-name rule
/// (`^[a-z][a-z0-9_]*$`) is one regex but the operator usually
/// wants to know WHICH bit they broke: digit-leading, uppercase,
/// or stray punctuation. Returns a short phrase to splice into
/// the caller's full error.
fn spin_key_rule_violation(key: &str) -> &'static str {
    // Callers only invoke this AFTER `is_valid_spin_key` returned
    // false; in production the per-char branches below exhaust the
    // failure modes and the catch-all at the bottom is unreachable.
    // It's kept defensively so a future regex tweak (e.g. allowing
    // a new char class) doesn't crash the diagnostic helper with
    // an unreachable!() before the caller can produce its error.
    //
    // Reachability notes for the per-mode branches:
    // - `push_config_entries` translates keys via
    //   `translate_key_for_spin` (which lowercases) BEFORE this
    //   call, so the uppercase-first branch is unreachable from
    //   that site. It IS reachable from `validate_app_config_keys`
    //   and `validate_typed_secrets`, which check raw user input.
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return "Spin variable names must not be empty";
    };
    if first.is_ascii_digit() {
        return "Spin variable names must start with a lowercase letter, not a digit";
    }
    if first.is_ascii_uppercase() {
        return "Spin variable names must be lowercase (uppercase letters are not allowed)";
    }
    if !first.is_ascii_lowercase() {
        return "Spin variable names must start with a lowercase ASCII letter";
    }
    for ch in chars {
        if ch.is_ascii_uppercase() {
            return "Spin variable names must be lowercase (uppercase letters are not allowed)";
        }
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
            return "Spin variable names may only contain lowercase letters, digits, and underscores";
        }
    }
    debug_assert!(
        false,
        "spin_key_rule_violation called with key `{key}` that satisfies the regex; check is_valid_spin_key + caller agreement"
    );
    "Spin variable names must match `^[a-z][a-z0-9_]*$`"
}

fn collect_spin_component_ids(parsed: &toml::Value) -> Vec<String> {
    parsed
        .as_table()
        .and_then(|root| root.get("component"))
        .and_then(toml::Value::as_table)
        .map(|components| components.keys().cloned().collect())
        .unwrap_or_default()
}

/// Read `[application].name` from `spin.toml`. Required by the
/// Fermyon Cloud writer to address KV stores via the app-scoped
/// label model (`--app <app> --label <label>`).
///
/// # Errors
/// Returns a human-readable error string if the file can't be
/// read, isn't valid TOML, or omits `[application].name`.
fn read_spin_application_name(spin_path: &Path) -> Result<String, String> {
    let raw = fs::read_to_string(spin_path).map_err(|err| {
        format!(
            "failed to read spin manifest at {}: {err}",
            spin_path.display()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
    parsed
        .as_table()
        .and_then(|root| root.get("application"))
        .and_then(toml::Value::as_table)
        .and_then(|app| app.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "spin manifest at {} is missing `[application].name`. Fermyon Cloud push needs the app name to address KV stores via `--app <name> --label <label>`. Add `[application]\\nname = \"<your-app>\"` to spin.toml.",
                spin_path.display()
            )
        })
}

/// Dispatch `config push --adapter spin` to the right per-backend
/// writer based on `runtime-config.toml` + the adapter's
/// `[adapters.spin.commands].deploy` command (auto-detect for Fermyon
/// Cloud).
///
/// Decision order:
/// 1. **`--local` is set**: force SQLite-direct against the local
///    `.spin/sqlite_key_value.db`. Fermyon Cloud auto-detect cannot
///    fire — even when the manifest's deploy command would otherwise
///    trip it. This lets the operator push into a local KV without
///    needing to authenticate with Fermyon Cloud first.
/// 2. **Deploy command auto-detects Fermyon Cloud** (`spin deploy` /
///    `spin cloud deploy`): shell out to `spin cloud key-value set`.
/// 3. **`runtime-config.toml` exists and declares this label's
///    backend**: dispatch on `type` — `spin` → SQLite-direct, `redis`
///    / `azure_cosmos` / `Unknown` → clear error pointing at the
///    backend's native CLI.
/// 4. **Default**: SQLite-direct at Spin's default path.
fn dispatch_push(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    push_ctx: &AdapterPushContext<'_>,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let platform = store.platform.as_str();
    let logical = store.logical.as_str();

    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to spin store `{platform}` (logical id `{logical}`)"
        )]);
    }

    let spin_manifest_path = adapter_manifest_path
        .map(|rel| manifest_root.join(rel))
        .ok_or_else(|| {
            "[adapters.spin.adapter].manifest must point at spin.toml for config push".to_owned()
        })?;
    let spin_manifest_dir = spin_manifest_path.parent().unwrap_or(manifest_root);

    // --runtime-config wins; otherwise default to
    // `runtime-config.toml` next to the spin manifest. Path math
    // only — the actual `runtime_config::read` is deferred until a
    // branch that NEEDS the parsed file, so a malformed/unreadable
    // `runtime-config.toml` doesn't block the cloud branch (which
    // only needs `spin.toml`'s `[application].name`).
    let runtime_config_path = push_ctx.runtime_config_path.map_or_else(
        || spin_manifest_dir.join("runtime-config.toml"),
        Path::to_path_buf,
    );
    let runtime_config_dir = runtime_config_path.parent().unwrap_or(spin_manifest_dir);

    // 1. `--local` forces SQLite-direct EVERY time. We skip both the
    //    Fermyon Cloud auto-detect AND the runtime-config backend
    //    dispatch — even if the operator's `runtime-config.toml`
    //    declares `type = "redis"` / `azure_cosmos` / unknown for
    //    this label. The intent of `--local` is "I want to seed my
    //    local dev loop, regardless of what the deployed app's
    //    backend selection looks like". An explicit `--runtime-config
    //    <path>` is still honoured for resolving the SQLite path,
    //    but the backend `type` is ignored.
    //
    //    We still enforce the Spin runtime invariant that any
    //    non-`default` label MUST be declared in `runtime-config.toml`
    //    — without it, `spin up` errors with "unknown
    //    key_value_stores label X" and the SQLite file we wrote is
    //    unreadable from the running app. See `verify_label_declared`.
    if push_ctx.local {
        let parsed = runtime_config::read(&runtime_config_path)?;
        verify_label_declared(platform, &parsed, &runtime_config_path)?;
        return write_sqlite(
            spin_manifest_dir,
            runtime_config_dir,
            // If the operator DID declare `type = "spin"` with an
            // explicit `path`, honour that path; otherwise fall
            // through to Spin's default `.spin/sqlite_key_value.db`.
            // Other backend types are silently treated as "no
            // explicit path" so SQLite-direct still happens.
            match parsed.key_value_stores.get(platform) {
                Some(runtime_config::KeyValueBackend::Spin { path }) => path.as_deref(),
                _ => None,
            },
            platform,
            logical,
            entries,
            dry_run,
        );
    }

    // 2. Else, if the manifest deploy command shells to `spin deploy`,
    //    treat as Fermyon Cloud. Cloud's `set` subcommand addresses
    //    the cloud KV store via Fermyon's app-scoped label model
    //    (`--app <app> --label <label>`), so we need the spin app
    //    name from spin.toml. We DO NOT read `runtime-config.toml`
    //    here — the cloud writer doesn't consult any local backend
    //    declaration, and parsing the file would gratuitously block
    //    cloud pushes (including `--dry-run` previews) on syntax
    //    errors in a file that doesn't influence the cloud path.
    if push_cloud::deploy_command_targets_fermyon_cloud(push_ctx.manifest_adapter_deploy_cmd) {
        let app_name = read_spin_application_name(&spin_manifest_path)?;
        if dry_run {
            // Run the same validation the real push runs: a `=` in
            // a key would be silently split by `spin`'s upstream
            // `KEY=VALUE` parser, and any single entry / cumulative
            // argv chunk over the safe-argv cap would fail the
            // shellout. Surfacing these in dry-run means a "clean"
            // preview is a real predictor of push success — without
            // it, the operator gets a green dry-run followed by a
            // hard failure on the actual push.
            let chunks = push_cloud::chunk_entries(entries)?;
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would shell `spin cloud key-value set --app {app_name} --label {platform} KEY=VALUE [...]` for {} entries across {} invocation(s) (logical id `{logical}`):",
                entries.len(),
                chunks.len()
            ));
            for (key, _) in entries {
                out.push(format!("  would set `{key}`"));
            }
            return Ok(out);
        }
        push_cloud::write_batch(&app_name, platform, entries)?;
        return Ok(vec![format!(
            "pushed {} entries to Fermyon Cloud KV store linked to app `{app_name}` label `{platform}` (logical id `{logical}`) via `spin cloud key-value set`",
            entries.len()
        )]);
    }

    // 3 / 4: SQLite-direct dispatch. Look up the backend explicitly
    // declared for this label, falling back to Spin's default
    // `(type = "spin", path = None)` if the label has no stanza.
    let parsed = runtime_config::read(&runtime_config_path)?;
    let backend = parsed.key_value_stores.get(platform);
    match backend {
        Some(runtime_config::KeyValueBackend::Redis { url }) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by `type = \"redis\"` (url: `{url}`) in {}; `config push --adapter spin` does not yet support the redis backend in this version. Use `redis-cli -u {url} SET <key> <value>` directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::AzureCosmos) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by `type = \"azure_cosmos\"` in {}; `config push --adapter spin` does not yet support the Azure backend in this version. Use Azure's CosmosDB SDK / `az cosmosdb` CLI directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Unknown { type_name }) => Err(format!(
            "store `{platform}` (logical id `{logical}`) is backed by an unrecognised type `{type_name}` in {}; `config push --adapter spin` only supports `type = \"spin\"` for now. Use the backend's native CLI to seed entries directly.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Spin { path }) => write_sqlite(
            spin_manifest_dir,
            runtime_config_dir,
            path.as_deref(),
            platform,
            logical,
            entries,
            dry_run,
        ),
        None => {
            // Spin's runtime auto-provides ONLY the `default` label;
            // any other label must have a `[key_value_store.<label>]`
            // stanza or `spin up` errors. We fall through to SQLite-
            // direct only for `default`; non-default un-declared
            // labels error early so the operator doesn't write a
            // file the running app can't open.
            verify_label_declared(platform, &parsed, &runtime_config_path)?;
            write_sqlite(
                spin_manifest_dir,
                runtime_config_dir,
                None,
                platform,
                logical,
                entries,
                dry_run,
            )
        }
    }
}

/// Spin's runtime auto-provides ONLY the `default` KV label. Any
/// other label must be declared in `runtime-config.toml`; without
/// it `spin up` errors with `unknown key_value_stores label X` and
/// the `SQLite` file our writer just created is unreadable from
/// the running app. This helper enforces the same invariant at push
/// time so a silent "push succeeds, runtime can't open store"
/// divergence can't happen.
fn verify_label_declared(
    platform: &str,
    parsed: &runtime_config::ParsedRuntimeConfig,
    runtime_config_path: &Path,
) -> Result<(), String> {
    if platform == "default" || parsed.key_value_stores.contains_key(platform) {
        return Ok(());
    }
    Err(format!(
        "label `{platform}` has no `[key_value_store.{platform}]` stanza in {}. Spin auto-provides ONLY the `default` label; any other label must be declared in runtime-config.toml or `spin up` errors with `unknown key_value_stores label {platform}`. Add `[key_value_store.{platform}]\\ntype = \"spin\"` (or the backend you want) to {} and retry.",
        runtime_config_path.display(),
        runtime_config_path.display(),
    ))
}

/// `SQLite`-direct write helper: resolves the `SQLite` path (honouring
/// any explicit `path` from `runtime-config.toml`), then either prints
/// a dry-run preview or actually writes the batch through
/// `push_sqlite::write_batch`.
fn write_sqlite(
    spin_manifest_dir: &Path,
    runtime_config_dir: &Path,
    explicit_path: Option<&Path>,
    platform: &str,
    logical: &str,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let db_path =
        push_sqlite::resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, explicit_path);
    if dry_run {
        let mut out = Vec::with_capacity(entries.len().saturating_add(1));
        out.push(format!(
            "would write {} entries to SQLite-backed Spin KV at `{}` for store `{platform}` (logical id `{logical}`):",
            entries.len(),
            db_path.display()
        ));
        for (key, _) in entries {
            out.push(format!("  would set `{key}`"));
        }
        return Ok(out);
    }
    push_sqlite::write_batch(&db_path, platform, entries)?;
    Ok(vec![format!(
        "pushed {} entries to Spin SQLite KV at `{}` for store `{platform}` (logical id `{logical}`)",
        entries.len(),
        db_path.display()
    )])
}

/// `SQLite`-direct read helper: opens the Spin KV database at `db_path`
/// and queries `SELECT value FROM spin_key_value WHERE store=$1 AND key=$2`.
///
/// Returns:
/// - `MissingStore` if the database file does not exist (same semantic
///   as the write path creating it on first write).
/// - `MissingKey` if the row is absent.
/// - `Present(value)` on a hit (value decoded from UTF-8 BLOB).
fn read_sqlite_entry(db_path: &Path, store: &str, key: &str) -> Result<ReadConfigEntry, String> {
    use rusqlite::{Connection, OptionalExtension as _, params};

    if !db_path.exists() {
        return Ok(ReadConfigEntry::MissingStore);
    }
    let connection = Connection::open(db_path)
        .map_err(|err| format!("failed to open `{}`: {err}", db_path.display()))?;
    // Ensure the schema exists so opening a fresh (empty) file doesn't error.
    connection
        .execute(push_sqlite::SPIN_KV_CREATE_TABLE, [])
        .map_err(|err| format!("failed to verify schema in `{}`: {err}", db_path.display()))?;
    let raw: Option<Vec<u8>> = connection
        .query_row(
            "SELECT value FROM spin_key_value WHERE store=$1 AND key=$2",
            params![store, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| {
            format!(
                "failed to query `{}` for store `{store}` key `{key}`: {err}",
                db_path.display()
            )
        })?;
    match raw {
        None => Ok(ReadConfigEntry::MissingKey),
        Some(bytes) => {
            let value = String::from_utf8(bytes).map_err(|err| {
                format!(
                    "value for store `{store}` key `{key}` in `{}` is not valid UTF-8: {err}",
                    db_path.display()
                )
            })?;
            Ok(ReadConfigEntry::Present(value))
        }
    }
}

/// Resolve which `[component.<id>]` table `provision` should
/// write into. Mirrors the rule used by `validate_adapter_manifest`:
/// single-component spin.toml resolves implicitly,
/// multi-component requires an explicit `component = "..."` in
/// `[adapters.spin.adapter]`, and a selector that doesn't match
/// any declared id is an error.
fn resolve_spin_component(
    spin_path: &Path,
    component_selector: Option<&str>,
) -> Result<String, String> {
    let raw = fs::read_to_string(spin_path).map_err(|err| {
        format!(
            "failed to read spin manifest at {}: {err}",
            spin_path.display()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|err| format!("failed to parse {} as TOML: {err}", spin_path.display()))?;
    let component_ids = collect_spin_component_ids(&parsed);

    if component_ids.is_empty() {
        return Err(format!(
            "{}: no [component.*] declarations found",
            spin_path.display()
        ));
    }
    if let Some(selector) = component_selector {
        if component_ids.iter().any(|id| id == selector) {
            return Ok(selector.to_owned());
        }
        return Err(format!(
            "[adapters.spin.adapter].component = {:?} is not declared in {} (available: {})",
            selector,
            spin_path.display(),
            component_ids.join(", ")
        ));
    }
    if component_ids.len() == 1 {
        return Ok(component_ids.into_iter().next().unwrap_or_default());
    }
    Err(format!(
        "{} declares {} components ({}) but [adapters.spin.adapter].component is unset; set one explicitly",
        spin_path.display(),
        component_ids.len(),
        component_ids.join(", ")
    ))
}

/// Ensure `label` appears in `[component.<component_id>]`'s
/// `key_value_stores = [...]` array. Creates the array if absent.
/// Returns `Ok(true)` if the label was newly added, `Ok(false)` if
/// it was already there (idempotent across re-runs). Preserves the
/// rest of the spin manifest, including formatting and comments.
fn ensure_kv_label_in_component(
    spin_path: &Path,
    component_id: &str,
    label: &str,
) -> Result<bool, String> {
    use toml_edit::{Array, DocumentMut, Value, value};

    let raw = fs::read_to_string(spin_path)
        .map_err(|err| format!("failed to read {}: {err}", spin_path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", spin_path.display()))?;

    let component_root = doc.get_mut("component").ok_or_else(|| {
        format!(
            "{}: [component.*] tables expected but `component` key missing",
            spin_path.display()
        )
    })?;
    let component_tbl = component_root
        .as_table_mut()
        .ok_or_else(|| format!("{}: `component` is not a table", spin_path.display()))?;
    let target = component_tbl.get_mut(component_id).ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not declared",
            spin_path.display()
        )
    })?;
    let target_tbl = target.as_table_mut().ok_or_else(|| {
        format!(
            "{}: [component.{component_id}] is not a table",
            spin_path.display()
        )
    })?;

    let entry = target_tbl
        .entry("key_value_stores")
        .or_insert_with(|| value(Array::new()));
    let arr = entry
        .as_value_mut()
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            format!(
                "{}: [component.{component_id}].key_value_stores is not an array",
                spin_path.display()
            )
        })?;

    if arr.iter().any(|item| item.as_str() == Some(label)) {
        return Ok(false);
    }
    arr.push(label);

    fs::write(spin_path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", spin_path.display()))?;
    Ok(true)
}

/// # Errors
/// Returns an error if the Spin CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            TARGET_TRIPLE,
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
/// Returns an error if the Spin CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin deploy failed with status {status}"));
    }

    Ok(())
}

fn find_spin_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "spin.toml") {
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
            path.file_name().is_some_and(|n| n == "spin.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate spin.toml".to_owned());
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
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(TARGET_TRIPLE)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(TARGET_TRIPLE)
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
    register_adapter(&SPIN_ADAPTER);
    register_adapter_blueprint(&SPIN_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Spin CLI up command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["up"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` / `"demo"` per call site)
    // keeps the setup-vs-assertion pair in sync -- a typo in one
    // place no longer silently divorces from the other, because both
    // reference the same const. Also names the intent: these are the
    // LOGICAL store ids + spin component id the adapter operates on,
    // not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_KV_ID_ALT: &str = "cache";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";
    const TEST_COMPONENT_ID: &str = "demo";

    #[test]
    fn is_valid_spin_key_accepts_lowercase_with_digits_and_underscores() {
        assert!(is_valid_spin_key("foo"));
        assert!(is_valid_spin_key("foo_bar"));
        assert!(is_valid_spin_key("foo__bar"));
        assert!(is_valid_spin_key("a1b2"));
    }

    #[test]
    fn is_valid_spin_key_rejects_bad_starts_and_chars() {
        assert!(!is_valid_spin_key(""));
        assert!(!is_valid_spin_key("FOO"));
        assert!(!is_valid_spin_key("1foo"));
        assert!(!is_valid_spin_key("foo-bar"));
        assert!(!is_valid_spin_key("_foo"));
    }

    #[test]
    fn spin_key_rule_violation_picks_the_right_diagnostic_per_mode() {
        // Pin the exact diagnostic string per failure mode so a
        // future branch reorder can't pass these assertions by
        // accident (e.g. "lowercase" appears in two distinct return
        // values, so a substring-only check was too lax).
        assert_eq!(
            spin_key_rule_violation(""),
            "Spin variable names must not be empty"
        );
        assert_eq!(
            spin_key_rule_violation("1foo"),
            "Spin variable names must start with a lowercase letter, not a digit"
        );
        assert_eq!(
            spin_key_rule_violation("Foo"),
            "Spin variable names must be lowercase (uppercase letters are not allowed)"
        );
        assert_eq!(
            spin_key_rule_violation("foo-bar"),
            "Spin variable names may only contain lowercase letters, digits, and underscores"
        );
        assert_eq!(
            spin_key_rule_violation("fooBar"),
            "Spin variable names must be lowercase (uppercase letters are not allowed)"
        );
        // `_foo` starts with `_` -- not digit, not uppercase, not
        // lowercase ASCII letter. Falls through to the "must start
        // with a lowercase ASCII letter" branch.
        assert_eq!(
            spin_key_rule_violation("_foo"),
            "Spin variable names must start with a lowercase ASCII letter"
        );
    }

    #[test]
    fn validate_typed_secrets_passes_with_no_collision() {
        SpinCliAdapter
            .validate_typed_secrets(&[TypedSecretEntry::new(
                "default",
                "api_token",
                "demo_api_token",
            )])
            .expect("non-colliding inputs must pass");
    }

    /// `#[secret]` values must also be valid Spin variable names
    /// after canonicalisation. A dashed value like `"api-token"`
    /// reaches Spin at runtime and gets rejected with an opaque
    /// `InvalidName` — the validator should catch it earlier.
    #[test]
    fn validate_typed_secrets_rejects_invalid_spin_variable_in_secret_value() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&[TypedSecretEntry::new("default", "api_token", "api-token")])
            .expect_err("dashed secret value must error");
        assert!(
            // The error must name BOTH the field name (`api_token`,
            // underscore) and the offending value (`api-token`,
            // dash), plus mark it as a Spin variable issue. The prior
            // assertion double-checked the value and silently missed
            // the field-name half.
            err.contains("api_token") && err.contains("api-token") && err.contains("Spin variable"),
            "error names the field, the bad value, and the Spin-variable bucket: {err}"
        );
    }

    /// Negative case: a lowercased secret value that happens to
    /// coincide with another lowercased value MUST collide
    /// (sanity check that the `seen` map still works post-fix).
    #[test]
    fn validate_typed_secrets_detects_collision_between_two_lowercased_secret_values() {
        let err = SpinCliAdapter
            .validate_typed_secrets(&[
                TypedSecretEntry::new("default", "first", "SHARED_NAME"),
                TypedSecretEntry::new("default", "second", "shared_name"),
            ])
            .expect_err("two values lowercasing to the same name must collide");
        assert!(
            err.contains("shared_name") && (err.contains("first") || err.contains("second")),
            "error names the shared canonical name and at least one field: {err}"
        );
    }

    // 12.16 — named-store secret adapter validation

    #[test]
    fn collision_error_names_both_field_names_and_lowercased_variable() {
        // 12.16 case (b): KeyInDefault and KeyInNamedStore that
        // collide on the lowercased Spin variable.
        let entries = [
            TypedSecretEntry::new("default", "one", "Demo_Token"),
            TypedSecretEntry::new("vault", "two", "demo_token"),
        ];
        let err = SpinCliAdapter.validate_typed_secrets(&entries).unwrap_err();
        assert!(err.contains("`one`"), "{err}");
        assert!(err.contains("`two`"), "{err}");
        assert!(err.contains("demo_token"), "{err}");
    }

    #[test]
    fn rejects_invalid_spin_variable_name_with_hyphen() {
        // 12.16 case (a): KeyInNamedStore value contains a hyphen,
        // which is not a valid Spin variable name.
        let entries = [TypedSecretEntry::new("vault", "api_token", "demo-token")];
        let err = SpinCliAdapter.validate_typed_secrets(&entries).unwrap_err();
        assert!(err.contains("`api_token`"), "{err}");
        assert!(err.contains("demo-token"), "{err}");
        assert!(
            err.to_lowercase().contains("hyphen") || err.contains("not a valid"),
            "{err}"
        );
    }

    #[test]
    fn non_spin_adapter_is_exempt_from_collision_check() {
        // 12.16 case (c): same collision fixture against a manifest
        // declaring only [adapters.axum] — covered by run_adapter_
        // typed_checks NOT calling SpinCliAdapter at all. This is more
        // naturally a CLI-level integration test, but the adapter
        // unit test asserts that a non-Spin adapter's trait-default
        // `validate_typed_secrets` returns Ok(()) on the same input.
        struct StubAdapter;
        #[expect(
            clippy::missing_trait_methods,
            reason = "StubAdapter exercises only the trait default for validate_typed_secrets"
        )]
        impl Adapter for StubAdapter {
            fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
                Ok(())
            }
            fn name(&self) -> &'static str {
                "stub"
            }
        }
        let entries = [
            TypedSecretEntry::new("default", "one", "Demo_Token"),
            TypedSecretEntry::new("vault", "two", "demo_token"),
        ];
        StubAdapter
            .validate_typed_secrets(&entries)
            .expect("non-Spin adapter trait default must return Ok(()) for any entries");
    }

    #[test]
    fn validate_adapter_manifest_errors_on_zero_components() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), None)
            .expect_err("no [component.*] must error");
        assert!(
            err.contains("no [component.*]"),
            "error explains the absence: {err}"
        );
    }

    #[test]
    fn validate_adapter_manifest_rejects_bad_selector_against_single_component() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.actual]\nsource = \"a.wasm\"\n",
        )
        .unwrap();
        let err = SpinCliAdapter
            .validate_adapter_manifest(dir.path(), Some("spin.toml"), Some("typo"))
            .expect_err("typo selector must error");
        assert!(
            err.contains("typo") && err.contains("actual"),
            "error names both the bad selector and the available id: {err}"
        );
    }

    #[test]
    fn single_store_kinds_is_secrets_only() {
        // Stage 5: config moved to KV (provisioned via `key_value_stores`,
        // entries pushed via the seed handler). Secrets remain Spin
        // `[variables]` until we ship native secret support.
        assert_eq!(SpinCliAdapter.single_store_kinds(), &["secrets"]);
    }

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let found = find_spin_manifest(&second).unwrap();
        assert_eq!(found, second.join("spin.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let manifest = find_spin_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("spin.toml"));
    }

    #[test]
    fn locate_artifact_considers_workspace_target() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(manifest_dir.join("target/wasm32-wasip2/release")).unwrap();
        let artifact = workspace.join("target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, TEST_COMPONENT_ID).unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn locate_artifact_converts_hyphens_to_underscores() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("crates/my-cool-crate");
        fs::create_dir_all(&manifest_dir).unwrap();

        // Cargo emits underscored filenames for hyphenated crate names.
        let artifact = workspace.join("target/wasm32-wasip2/release/my_cool_crate.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "my-cool-crate").unwrap();
        assert_eq!(located, artifact);
    }

    // ---------- resolve_spin_component ----------

    fn write_spin(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(&path, contents).expect("write spin.toml");
        path
    }

    #[test]
    fn resolve_spin_component_picks_single_component_implicitly() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.only]\nsource = \"a.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, None).expect("resolve");
        assert_eq!(resolved, "only");
    }

    #[test]
    fn resolve_spin_component_uses_selector_when_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let resolved = resolve_spin_component(&path, Some("second")).expect("resolve");
        assert_eq!(resolved, "second");
    }

    #[test]
    fn resolve_spin_component_errors_on_multi_without_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.first]\nsource = \"a.wasm\"\n[component.second]\nsource = \"b.wasm\"\n",
        );
        let err = resolve_spin_component(&path, None).expect_err("ambiguous must error");
        assert!(
            err.contains("first") && err.contains("second"),
            "error lists candidates: {err}"
        );
    }

    #[test]
    fn resolve_spin_component_errors_on_bad_selector() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.real]\nsource = \"a.wasm\"\n",
        );
        let err = resolve_spin_component(&path, Some("typo")).expect_err("bad selector must error");
        assert!(
            err.contains("typo") && err.contains("real"),
            "error names bad selector and available id: {err}"
        );
    }

    // ---------- ensure_kv_label_in_component ----------

    #[test]
    fn ensure_kv_label_adds_to_component_without_key_value_stores() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(added, "newly added label should return true");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("key_value_stores = [\"sessions\"]")
                || after.contains("key_value_stores = ['sessions']"),
            "added KV label: {after}"
        );
    }

    #[test]
    fn ensure_kv_label_appends_to_existing_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"cache\"]\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(added);
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("\"cache\""), "kept existing label: {after}");
        assert!(after.contains("\"sessions\""), "added new label: {after}");
    }

    #[test]
    fn ensure_kv_label_is_idempotent_when_already_present() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nkey_value_stores = [\"sessions\"]\n",
        );
        let added =
            ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        assert!(!added, "duplicate label should return false");
    }

    #[test]
    fn ensure_kv_label_errors_when_component_missing() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let err = ensure_kv_label_in_component(&path, "missing", TEST_KV_ID)
            .expect_err("missing component must error");
        assert!(
            err.contains("missing"),
            "error names the missing component id: {err}"
        );
    }

    #[test]
    fn ensure_kv_label_preserves_top_comments_and_other_fields() {
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "# keep me\nspin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\nallowed_outbound_hosts = []\n",
        );
        ensure_kv_label_in_component(&path, TEST_COMPONENT_ID, TEST_KV_ID).expect("ensure");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("# keep me"), "preserved comment: {after}");
        assert!(
            after.contains("allowed_outbound_hosts = []"),
            "preserved sibling field: {after}"
        );
    }

    // ---------- provision (dry-run + error path + idempotent skip) ----------

    #[test]
    fn provision_dry_run_does_not_edit_spin_toml() {
        let dir = tempdir().expect("tempdir");
        let original = "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n";
        let path = write_spin(dir.path(), original);
        let kv_ids: Vec<ResolvedStoreId> =
            ResolvedStoreId::from_logicals(&[TEST_KV_ID, TEST_KV_ID_ALT]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("would ensure KV label `sessions`"));
        assert!(out[1].contains("would ensure KV label `cache`"));
        let after = fs::read_to_string(&path).expect("read back");
        assert_eq!(after, original, "dry-run mutated spin.toml");
    }

    #[test]
    fn provision_writes_resolved_platform_label_into_kv_array() {
        // Regression: spin provision used to receive only logical
        // ids and add them verbatim to
        // `[component.X].key_value_stores`. With the platform-name
        // flow, an operator who sets
        // `EDGEZERO__STORES__KV__SESSIONS__NAME=prod_sessions` now
        // sees `prod_sessions` land as the KV label (matching what
        // the runtime opens), with the logical id preserved for
        // human-facing wording.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids = vec![ResolvedStoreId::new(TEST_KV_ID, "prod_sessions")];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real-run succeeds");
        assert!(
            out[0].contains("`prod_sessions`") && out[0].contains("`sessions`"),
            "status line names BOTH the platform label and the logical id: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains("\"prod_sessions\""),
            "platform label written into spin.toml KV array: {after}"
        );
        assert!(
            !after.contains("\"sessions\""),
            "logical id is NOT written (would shadow the platform binding): {after}"
        );
    }

    #[test]
    fn provision_writes_kv_labels_into_resolved_component() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("real run succeeds");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("added KV label `sessions`"), "got: {out:?}");
        let after = fs::read_to_string(dir.path().join("spin.toml")).expect("read back");
        assert!(
            after.contains("\"sessions\""),
            "label landed in spin.toml: {after}"
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
        let err = SpinCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("spin.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_writes_config_labels_into_kv_array_and_leaves_secrets_manual() {
        // Stage 5: config now lives in Spin KV. Provision writes each
        // `[stores.config].id` into `[component.X].key_value_stores`
        // (same machinery as `[stores.kv]`). Secrets stay manual until
        // we ship native secret support.
        let dir = tempdir().expect("tempdir");
        let path = write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &secret_ids,
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("config + secrets provision succeeds");
        assert_eq!(out.len(), 2);
        assert!(
            out[0].contains("config label") && out[0].contains("key_value_stores"),
            "config row reports KV-array write: {out:?}"
        );
        assert!(
            out[1].contains("manual"),
            "secret row still flags manual declaration: {out:?}"
        );

        let after = fs::read_to_string(&path).expect("read spin.toml");
        assert!(
            after.contains(&format!("\"{TEST_CONFIG_ID}\"")),
            "config label landed in spin.toml: {after}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        write_spin(
            dir.path(),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        );
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = SpinCliAdapter
            .provision(dir.path(), Some("spin.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["spin has no declared stores to provision"]);
    }

    // ---------- dispatch_push matrix ----------
    //
    // These tests exercise `dispatch_push` directly with fixture
    // `AdapterPushContext` / `runtime-config.toml` shapes. The
    // function is where all the business logic of the per-backend
    // redesign lives, so the matrix has to be tight: each branch
    // (--local override, Fermyon Cloud auto-detect, redis-error,
    // azure-error, unknown-error, default-SQLite, explicit-Spin-with-
    // path, empty entries) gets a named test.

    fn write_minimal_spin_toml(dir: &Path) -> PathBuf {
        let path = dir.join("spin.toml");
        fs::write(
            &path,
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");
        path
    }

    fn entries_two() -> Vec<(String, String)> {
        vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("svc.timeout".to_owned(), "1500".to_owned()),
        ]
    }

    fn store(logical: &str, platform: &str) -> ResolvedStoreId {
        ResolvedStoreId::new(logical.to_owned(), platform.to_owned())
    }

    #[test]
    fn dispatch_push_empty_entries_returns_noop_message() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &[],
            &push_ctx,
            false,
        )
        .expect("dispatch ok");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "no-op message: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_runtime_config_declares_redis() {
        // F1 (blocker): `--local` MUST bypass runtime-config backend
        // dispatch. Without this test, the code that says "Redis: error
        // out" would silently fire even under --local.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let entries = entries_two();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries,
            &push_ctx,
            true, // dry-run so the test doesn't actually touch disk
        )
        .expect("--local + redis must dispatch to SQLite, not error");
        assert!(
            out[0].contains("SQLite-backed Spin KV"),
            "--local must force the SQLite writer: {out:?}"
        );
        assert!(
            !out.iter().any(|line| line.contains("redis-cli")),
            "--local must NOT emit the redis-cli error: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_runtime_config_declares_azure() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("--local + azure must dispatch to SQLite");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    #[test]
    fn dispatch_push_local_forces_sqlite_even_when_deploy_targets_fermyon_cloud() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Non-default labels require a runtime-config stanza so
        // `spin up` can open them; with the stanza in place, --local
        // dispatches to SQLite regardless of the Fermyon Cloud deploy
        // command.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new()
            .with_local(true)
            .with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("--local must beat Fermyon Cloud auto-detect");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
        assert!(
            !out.iter()
                .any(|line| line.contains("spin cloud key-value set")),
            "Fermyon Cloud writer must NOT fire under --local: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_local_honours_explicit_spin_path() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\npath = \"custom/kv.db\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new().with_local(true);
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("dispatch ok");
        let expected_path = dir.path().join("custom/kv.db");
        assert!(
            out.iter()
                .any(|line| line.contains(&expected_path.display().to_string())),
            "explicit path under --local: expected {} in {out:?}",
            expected_path.display()
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_auto_detects_from_spin_deploy_and_uses_app_label_shape() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path()); // [application].name = "x"

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("dispatch ok");
        // The preview MUST show Fermyon's app-scoped label shape
        // (`--app <APP> --label <LABEL> KEY=VALUE`), not the
        // pre-fix `--store <STORE>` shape.
        assert!(
            out[0].contains("spin cloud key-value set"),
            "cloud writer dry-run preview: {out:?}"
        );
        assert!(
            out[0].contains("--app x") && out[0].contains("--label app_config"),
            "must use --app <spin app name> + --label <platform label> per Fermyon's app-scoped label model: {out:?}"
        );
        assert!(
            !out[0].contains("--store"),
            "must NOT use --store (conflates spin label with cloud KV resource name): {out:?}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_dry_run_ignores_malformed_runtime_config() {
        // Cloud push (set / link) consults only spin.toml's
        // `[application].name` + the env-resolved label — it never
        // reads `runtime-config.toml`. So a malformed local runtime
        // config (someone mid-edit, a stale file from another
        // project, etc.) MUST NOT block cloud `--dry-run` previews.
        // Before this fix `dispatch_push` parsed runtime-config
        // unconditionally at the top, so any TOML syntax error
        // surfaced before the cloud branch even ran.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path()); // [application].name = "x"
        fs::write(
            dir.path().join("runtime-config.toml"),
            "this is not [valid toml at all\n",
        )
        .expect("write malformed runtime-config");

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("cloud dry-run must succeed despite malformed runtime-config");
        assert!(
            out[0].contains("spin cloud key-value set")
                && out[0].contains("--app x")
                && out[0].contains("--label app_config"),
            "cloud preview should render the app-scoped shape: {out:?}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_dry_run_rejects_equals_in_key() {
        // The real push errors on a `=` in a key (would be silently
        // truncated by Spin's upstream `KEY=VALUE` parser). The
        // dry-run preview MUST surface the same error — otherwise an
        // operator gets a green dry-run followed by a hard failure on
        // the actual push.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let bad = vec![("svc=timeout".to_owned(), "1500".to_owned())];

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &bad,
            &push_ctx,
            true,
        )
        .expect_err("dry-run must reject `=` in keys");
        assert!(
            err.contains("contains `=`"),
            "dry-run error must surface the same KEY=VALUE diagnostic the real push gives: {err}"
        );
    }

    #[test]
    fn dispatch_push_fermyon_cloud_errors_when_spin_application_name_missing() {
        // The cloud writer needs `[application].name` for `--app`.
        // A spin.toml without it must error with an actionable
        // message, not silently shell `spin cloud key-value set
        // --app  --label …` (which would fail upstream with an
        // unhelpful clap error).
        let dir = tempdir().expect("tempdir");
        // Note: NO `[application]` section.
        let spin_path = dir.path().join("spin.toml");
        fs::write(
            &spin_path,
            "spin_manifest_version = 2\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");

        let push_ctx =
            AdapterPushContext::new().with_manifest_adapter_deploy_cmd("spin deploy --from ./");
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("missing [application].name must error");
        assert!(
            err.contains("[application].name") && err.contains("spin.toml"),
            "error must name the missing field + the file: {err}"
        );
    }

    #[test]
    fn dispatch_push_redis_backend_errors_with_redis_cli_hint() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost:6379\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("redis backend without --local must error");
        assert!(
            err.contains("redis-cli") && err.contains("redis://localhost:6379"),
            "redis error must name the cli + url: {err}"
        );
    }

    #[test]
    fn dispatch_push_azure_backend_errors_with_azure_cli_hint() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("azure backend without --local must error");
        assert!(
            err.contains("az cosmosdb"),
            "azure error must name the cli: {err}"
        );
    }

    #[test]
    fn dispatch_push_unknown_backend_errors_with_type_name() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"someones-new-backend\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("unknown backend must error");
        assert!(
            err.contains("someones-new-backend"),
            "unknown-type error must name the type: {err}"
        );
    }

    #[test]
    fn dispatch_push_default_label_with_no_runtime_config_dispatches_to_sqlite() {
        // Spin auto-provides ONLY the `default` label. With no
        // runtime-config.toml present and the platform label set to
        // `default`, we fall through to the SQLite writer.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("default", "default"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("`default` label -> SQLite");
        let expected = dir.path().join(".spin/sqlite_key_value.db");
        assert!(
            out.iter()
                .any(|line| line.contains(&expected.display().to_string())),
            "default SQLite path: expected {} in {out:?}",
            expected.display()
        );
    }

    #[test]
    fn dispatch_push_non_default_label_without_runtime_config_stanza_errors() {
        // H2: Spin's runtime can't open a custom label without a
        // `[key_value_store.<label>]` entry. Silently writing SQLite
        // for `app_config` when the operator hasn't declared it is
        // worse than erroring -- the push "succeeds" but the running
        // app fails at `Store::open("app_config")`. Catch this at
        // push time so the operator gets an actionable hint.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());

        let push_ctx = AdapterPushContext::new();
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("non-default label without runtime-config stanza must error");
        assert!(
            err.contains("`[key_value_store.app_config]`")
                && err.contains("unknown key_value_stores label app_config")
                && err.contains("type = \"spin\""),
            "error must name the stanza, the runtime symptom, AND the fix: {err}"
        );
    }

    #[test]
    fn dispatch_push_non_default_label_with_runtime_config_stanza_dispatches_to_sqlite() {
        // Counterpart to the test above: with the stanza in place,
        // the same `app_config` dispatch succeeds.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("non-default label WITH stanza must dispatch");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    #[test]
    fn dispatch_push_custom_runtime_config_path_is_honoured() {
        // H3 from the test-coverage review: prove --runtime-config
        // <path> is actually read from the non-default location.
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let custom = dir.path().join("alternate-runtime.toml");
        fs::write(
            &custom,
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://elsewhere\"\n",
        )
        .expect("write");

        let push_ctx = AdapterPushContext::new().with_runtime_config_path(&custom);
        let err = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect_err("custom runtime-config's redis declaration must fire");
        assert!(
            err.contains("redis://elsewhere"),
            "custom runtime-config was read: {err}"
        );
    }

    #[test]
    fn dispatch_push_unrelated_label_in_runtime_config_does_not_affect_dispatch() {
        // Sanity: only the matching label's stanza is consulted. A
        // [key_value_store.other_label] redis entry must NOT prevent
        // a SQLite-direct push to app_config (which has its own
        // declared stanza).
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n\n\
             [key_value_store.other_label]\ntype = \"redis\"\nurl = \"redis://nowhere\"\n",
        )
        .expect("write runtime-config");

        let push_ctx = AdapterPushContext::new();
        let out = dispatch_push(
            dir.path(),
            Some("spin.toml"),
            &store("app_config", "app_config"),
            &entries_two(),
            &push_ctx,
            true,
        )
        .expect("unrelated label must not block dispatch");
        assert!(out[0].contains("SQLite-backed Spin KV"), "{out:?}");
    }

    // ---------- read_config_entry / read_config_entry_local ----------

    // Helper: seed a key into the SQLite DB at `db_path` for `store_label`.
    fn write_kv_entry(db_path: &Path, store_label: &str, key: &str, value: &str) {
        push_sqlite::write_batch(db_path, store_label, &[(key.to_owned(), value.to_owned())])
            .expect("seed entry");
    }

    // Branch 2: Fermyon Cloud auto-detect → Unsupported.
    #[test]
    fn read_config_entry_returns_unsupported_for_fermyon_cloud_deploy_cmd() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let mut ctx = AdapterPushContext::new();
        ctx.manifest_adapter_deploy_cmd = Some("spin deploy");
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "greeting",
                &ctx,
            )
            .expect("cloud branch returns Ok(Unsupported)");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "Fermyon Cloud must return Unsupported"
        );
    }

    // Branch 3a: redis backend → error naming the backend.
    #[test]
    fn read_config_entry_errors_for_redis_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"redis\"\nurl = \"redis://localhost:6379\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("redis") && err.contains("app_config"),
                "error names the backend and store: {err}"
            ),
            Ok(_) => panic!("expected Err for redis backend"),
        }
    }

    // Branch 3b: azure_cosmos backend → error naming the backend.
    #[test]
    fn read_config_entry_errors_for_azure_cosmos_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"azure_cosmos\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("azure_cosmos") && err.contains("app_config"),
                "error names the backend and store: {err}"
            ),
            Ok(_) => panic!("expected Err for azure_cosmos backend"),
        }
    }

    // Branch 3c: unknown backend → error naming the type.
    #[test]
    fn read_config_entry_errors_for_unknown_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"future-backend\"\n",
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("future-backend"),
                "error names the unrecognised type: {err}"
            ),
            Ok(_) => panic!("expected Err for unknown backend"),
        }
    }

    // Branch 4: default `type = "spin"` → SQLite-direct, Present.
    #[test]
    fn read_config_entry_returns_present_for_spin_backend() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "greeting", "hello-spin");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "greeting",
                &ctx,
            )
            .expect("spin backend read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "hello-spin");
    }

    // Branch 4: default label (no runtime-config stanza) → MissingStore
    // when the database file doesn't exist.
    #[test]
    fn read_config_entry_returns_missing_store_when_db_absent() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // No runtime-config.toml → default label rules apply.
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "greeting",
                &ctx,
            )
            .expect("missing db is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "absent SQLite file must yield MissingStore"
        );
    }

    // Branch 4: key absent in an existing DB → MissingKey.
    #[test]
    fn read_config_entry_returns_missing_key_when_key_absent() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "default", "other_key", "v");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "greeting",
                &ctx,
            )
            .expect("missing key is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "absent key must yield MissingKey"
        );
    }

    // Malformed runtime-config.toml propagates as an error
    // (not silently swallowed with .ok()).
    #[test]
    fn read_config_entry_propagates_malformed_runtime_config_error() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config\ntype = \"spin\"\n", // missing closing `]`
        )
        .expect("write bad runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter.read_config_entry(
            dir.path(),
            Some("spin.toml"),
            None,
            &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
            "greeting",
            &ctx,
        );
        match result {
            Err(err) => assert!(
                err.contains("failed to parse") || err.contains("runtime-config"),
                "error names the parse failure: {err}"
            ),
            Ok(_) => panic!("expected Err for malformed runtime-config"),
        }
    }

    // Branch 1: --local forces SQLite-direct regardless of backend type.
    #[test]
    fn read_config_entry_local_reads_sqlite_ignoring_backend_type() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Declare a spin backend so label is declared for verify_label_declared.
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "mode", "local");
        let mut ctx = AdapterPushContext::new();
        ctx.local = true;
        let result = SpinCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "mode",
                &ctx,
            )
            .expect("local read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "local");
    }

    // Branch 1 via read_config_entry: push_ctx.local=true delegates to local impl.
    #[test]
    fn read_config_entry_with_local_flag_delegates_to_local_impl() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        fs::write(
            dir.path().join("runtime-config.toml"),
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        let db_path = dir.path().join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "flag", "set");
        let mut ctx = AdapterPushContext::new();
        ctx.local = true;
        // Even though Fermyon Cloud auto-detect would fire via deploy_cmd,
        // local flag must win.
        ctx.manifest_adapter_deploy_cmd = Some("spin deploy");
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "flag",
                &ctx,
            )
            .expect("local flag wins over cloud detect");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "local flag + cloud cmd must yield Present (SQLite wins)"
        );
    }

    // SQLite path anchors against runtime-config dir, not
    // spin manifest dir, for relative explicit paths.
    #[test]
    fn read_config_entry_sqlite_path_anchors_against_runtime_config_dir() {
        let dir = tempdir().expect("tempdir");
        // spin.toml at <tmp>/spin/spin.toml
        let spin_dir = dir.path().join("spin");
        fs::create_dir_all(&spin_dir).expect("create spin dir");
        fs::write(
            spin_dir.join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        // runtime-config.toml at <tmp>/cfg/runtime-config.toml with
        // path = "session.db" (relative).
        let cfg_dir = dir.path().join("cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg dir");
        let runtime_config_path = cfg_dir.join("runtime-config.toml");
        fs::write(
            &runtime_config_path,
            "[key_value_store.app_config]\ntype = \"spin\"\npath = \"session.db\"\n",
        )
        .expect("write runtime-config");
        // Seed the SQLite file at <tmp>/cfg/session.db (NOT spin/session.db).
        let db_path = cfg_dir.join("session.db");
        write_kv_entry(&db_path, "app_config", "key1", "val1");
        let mut ctx = AdapterPushContext::new();
        ctx.runtime_config_path = Some(&runtime_config_path);
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin/spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "key1",
                &ctx,
            )
            .expect("cfg-dir-anchored path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "val1", "value from cfg-dir-anchored db");
    }

    // Default path (no explicit path) falls back to spin manifest dir.
    #[test]
    fn read_config_entry_sqlite_default_path_anchors_against_spin_manifest_dir() {
        let dir = tempdir().expect("tempdir");
        let spin_dir = dir.path().join("spin");
        fs::create_dir_all(&spin_dir).expect("create spin dir");
        fs::write(
            spin_dir.join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\n[component.demo]\nsource = \"demo.wasm\"\n",
        )
        .expect("write spin.toml");
        let cfg_dir = dir.path().join("cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg dir");
        let runtime_config_path = cfg_dir.join("runtime-config.toml");
        // No `path` in the stanza → default .spin/sqlite_key_value.db.
        fs::write(
            &runtime_config_path,
            "[key_value_store.app_config]\ntype = \"spin\"\n",
        )
        .expect("write runtime-config");
        // Seed at <tmp>/spin/.spin/sqlite_key_value.db.
        let db_path = spin_dir.join(".spin/sqlite_key_value.db");
        write_kv_entry(&db_path, "app_config", "key2", "val2");
        let mut ctx = AdapterPushContext::new();
        ctx.runtime_config_path = Some(&runtime_config_path);
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin/spin.toml"),
                None,
                &store(TEST_CONFIG_ID, TEST_CONFIG_ID),
                "key2",
                &ctx,
            )
            .expect("default path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "val2", "value from spin-manifest-dir default db");
    }

    // Absolute path is honoured verbatim.
    #[test]
    fn read_config_entry_sqlite_absolute_path_honoured() {
        let dir = tempdir().expect("tempdir");
        write_minimal_spin_toml(dir.path());
        // Use a tempfile as the target database with an absolute path.
        let db_file = tempfile::NamedTempFile::new().expect("tempfile");
        let db_path = db_file.path().to_path_buf();
        write_kv_entry(&db_path, "default", "abs_key", "abs_val");
        let abs_path_str = db_path.to_str().expect("abs path utf8").to_owned();
        // Write runtime-config with the absolute path.
        fs::write(
            dir.path().join("runtime-config.toml"),
            format!("[key_value_store.default]\ntype = \"spin\"\npath = \"{abs_path_str}\"\n"),
        )
        .expect("write runtime-config");
        let ctx = AdapterPushContext::new();
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &store("default", "default"),
                "abs_key",
                &ctx,
            )
            .expect("absolute path read ok");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "abs_val");
    }
}
