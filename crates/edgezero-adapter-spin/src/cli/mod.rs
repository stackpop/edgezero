use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

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

mod provision_local;
mod push_cloud;
mod push_local;
mod run;
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
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_spin_wasm",
        repo_crate: "crates/edgezero-adapter-spin",
        fallback:
            "edgezero-adapter-spin = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-spin\", default-features = false, features = [\"spin\"] }",
        features: &["spin"],
    },
];

// `spin.toml` and `runtime-config.toml` are intentionally absent
// from the scaffold registration — same rationale as Axum's
// `axum.toml`. Both are written by the scaffold-time provision
// loop (see `generator::provision_all_selected_adapters` ->
// `Adapter::synthesise_baseline_manifest` -> `run::synthesise_spin_toml`
// + `run::synthesise_runtime_config_toml`). Registering a scaffold
// template here would cause the file to exist before provision
// runs; provision's `write_baseline_to_disk` skips existing
// files (spec § "Adapter manifests are gitignored"), so the two
// baselines would diverge — the scaffold template would win at
// `edgezero new`, but the synthesiser would win on a clean clone.
// Keep this single-source: only the synthesisers write these two
// files.
static SPIN_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "spin_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "spin_src_lib_rs",
        output: "src/lib.rs",
    },
];

static SPIN_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "spin_Cargo_toml",
        contents: include_str!("../templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "spin_src_lib_rs",
        contents: include_str!("../templates/src/lib.rs.hbs"),
    },
];

const SPIN_INSTALL_HINT: &str = "install the Spin CLI (https://spinframework.dev/) and try again";

struct SpinCliAdapter;

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
                let artifact = run::build(args)?;
                log::info!("[edgezero] Spin build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => run::deploy(args),
            AdapterAction::Serve => run::serve(args),
            other => Err(format!("spin adapter does not support {other:?}")),
        }
    }

    // Spin has no cloud identifiers to persist across provisions —
    // Fermyon Cloud addresses apps by name, not by opaque id.
    #[inline]
    fn deployed_fields(&self) -> &'static [&'static str] {
        &[]
    }

    // Stage 6: KV-backed config dropped Spin's `^[a-z][a-z0-9_]*$`
    // key rule and the config-vs-secret collision check. No adapter-
    // specific check applies to raw config keys.
    #[inline]
    fn validate_app_config_keys(&self, _keys: &[&str]) -> Result<(), String> {
        Ok(())
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
        _deployed: Option<&AdapterDeployedState>,
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        match mode {
            ProvisionMode::Cloud => {}
            ProvisionMode::Local => {
                return provision_local::provision(
                    manifest_root,
                    adapter_manifest_path,
                    component_selector,
                    stores,
                    dry_run,
                );
            }
            // ProvisionMode is #[non_exhaustive]; explicit error so a
            // future mode variant doesn't quietly fall into the cloud
            // arm below.
            other => {
                return Err(format!(
                    "spin adapter does not implement provision mode {other:?}"
                ))
            }
        }
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
        Ok(ProvisionOutcome::from_status_lines(out))
    }

    fn provision_typed(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        typed_secrets: &[TypedSecretEntry<'_>],
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        // Cloud mode is a no-op: Fermyon Cloud manages secret variables
        // through its own dashboard / `spin cloud variable set` CLI, so
        // there is nothing for the CLI to write from typed metadata.
        if !matches!(mode, ProvisionMode::Local) {
            return Ok(ProvisionOutcome::default());
        }
        provision_local::provision_typed(
            manifest_root,
            adapter_manifest_path,
            component_selector,
            typed_secrets,
            dry_run,
        )
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
        push_local::dispatch_push(
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
        push_local::dispatch_push(
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
        push_local::verify_label_declared(platform, &parsed, &runtime_config_path)?;
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
                let db_path = push_local::resolve_sqlite_path(
                    spin_manifest_dir,
                    runtime_config_dir,
                    path.as_deref(),
                );
                push_local::read_sqlite_entry(&db_path, platform, key)
            }
            None => {
                let db_path =
                    push_local::resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, None);
                push_local::read_sqlite_entry(&db_path, platform, key)
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
        push_local::verify_label_declared(platform, &parsed, &runtime_config_path)?;

        // Resolve the SQLite path: honour any explicit `path` in a
        // `type = "spin"` stanza; fall back to Spin's default otherwise
        // (matches the write path at dispatch_push branch 1).
        let explicit_path = match parsed.key_value_stores.get(platform) {
            Some(runtime_config::KeyValueBackend::Spin { path }) => path.as_deref(),
            _ => None,
        };
        let db_path =
            push_local::resolve_sqlite_path(spin_manifest_dir, runtime_config_dir, explicit_path);
        push_local::read_sqlite_entry(&db_path, platform, key)
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: Multi for KV AND Config (both label-backed via the
        // Spin KV API since Stage 5 of the spin-kv-config plan).
        // Single for Secrets (still flat-variable namespace).
        &["secrets"]
    }

    fn synthesise_baseline_manifest(
        &self,
        _manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        app_name: &str,
        _deployed: Option<&AdapterDeployedState>,
    ) -> Result<Vec<(PathBuf, String)>, String> {
        let spin_rel =
            adapter_manifest_path.map_or_else(|| PathBuf::from("spin.toml"), PathBuf::from);
        // runtime-config.toml sits next to spin.toml so a nested
        // `adapter_manifest_path` (e.g. `crates/spin/spin.toml`)
        // places runtime-config.toml at
        // `crates/spin/runtime-config.toml`. When `spin_rel` has no
        // parent (bare `spin.toml`), fall back to the workspace root.
        let rc_rel = spin_rel
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(
                || PathBuf::from("runtime-config.toml"),
                |parent| parent.join("runtime-config.toml"),
            );
        Ok(vec![
            (
                spin_rel,
                run::synthesise_spin_toml(app_name, component_selector),
            ),
            (rc_rel, run::synthesise_runtime_config_toml()),
        ])
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
            if let Some(prev_field) = seen.insert(spin_var.clone(), entry.field_name) {
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
    use toml_edit::{value, Array, DocumentMut, Value};

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

#[inline]
pub fn register() {
    register_adapter(&SPIN_ADAPTER);
    register_adapter_blueprint(&SPIN_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// Process-wide mutex used by tests across `cli` submodules to
/// serialise mutations of the process environment (PATH prepend +
/// arbitrary env vars). Parallel test threads share process env, so
/// any test touching it must hold this guard. Sharing a single
/// mutex across `provision_local::tests` and `push_cloud::tests`
/// prevents PATH races between those suites.
#[cfg(test)]
pub(super) fn env_mutation_guard() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
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
}
