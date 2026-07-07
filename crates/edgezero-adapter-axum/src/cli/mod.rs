use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ctor::ctor;
use edgezero_adapter::cli_support;
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

mod provision_local;
mod run;

// `axum.toml` is intentionally absent from the scaffold registration.
// It is written by the scaffold-time provision loop that runs
// immediately after file emission (see `generator.rs`
// `provision_all_selected_adapters` -> `Adapter::synthesise_baseline_manifest`
// -> `run::synthesise_axum_toml`). Registering a scaffold template
// here would cause `axum.toml` to be created before provision runs;
// provision's `write_baseline_to_disk` skips files that already
// exist (spec § "Adapter manifests are gitignored"), so the two
// baselines would diverge — the scaffold template would win at
// `edgezero new`, but the synthesiser would win on a clean clone.
// Keep this single-source: only the synthesiser writes `axum.toml`.
static AXUM_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "axum_Cargo_toml",
        contents: include_str!("../templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "axum_src_main_rs",
        contents: include_str!("../templates/src/main.rs.hbs"),
    },
];

static AXUM_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "axum_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "axum_src_main_rs",
        output: "src/main.rs",
    },
];

static AXUM_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_axum",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\" }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_axum",
        repo_crate: "crates/edgezero-adapter-axum",
        fallback:
            "edgezero-adapter-axum = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-axum\", default-features = false }",
        features: &["axum"],
    },
];

static AXUM_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "axum",
    display_name: "Axum",
    crate_suffix: "adapter-axum",
    dependency_crate: "edgezero-adapter-axum",
    dependency_repo_path: "crates/edgezero-adapter-axum",
    template_registrations: AXUM_TEMPLATE_REGISTRATIONS,
    files: AXUM_FILE_SPECS,
    extra_dirs: &["src"],
    dependencies: AXUM_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "axum.toml",
        build_target: "native",
        build_profile: "dev",
        build_features: &[],
    },
    commands: CommandTemplates {
        build: "cargo build -p {crate}",
        serve: "cargo run -p {crate}",
        deploy: "# configure deployment for Axum",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: Some(true),
    },
    readme: ReadmeInfo {
        description: "{display} adapter entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &[
            "`cd {crate_dir}`",
            "`cargo run` or `edgezero serve --adapter axum`",
        ],
    },
    run_module: "edgezero_adapter_axum",
};

static AXUM_ADAPTER: AxumCliAdapter = AxumCliAdapter;

struct AxumCliAdapter;

impl Adapter for AxumCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // The axum adapter is the in-process native dev server —
            // there is no remote auth provider to sign in/out of.
            // Per spec this is an explicit no-op.
            AdapterAction::AuthLogin | AdapterAction::AuthLogout | AdapterAction::AuthStatus => {
                log::info!(
                    "[edgezero] axum has no remote auth surface; `auth` is a no-op for this adapter"
                );
                Ok(())
            }
            AdapterAction::Build => run::build(args),
            AdapterAction::Deploy => run::deploy(args),
            AdapterAction::Serve => run::serve(args),
            other => Err(format!("axum adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "axum"
    }

    // Axum has no cloud identifiers to persist across provisions.
    #[inline]
    fn deployed_fields(&self) -> &'static [&'static str] {
        &[]
    }

    // Axum's KV / config / secrets each live in their own file — no
    // logical-id merging across store kinds.
    #[inline]
    fn merged_id_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    // Axum has no per-platform adapter manifest to validate — axum.toml
    // is the runtime's own file, checked at load time by the axum
    // adapter, not by the CLI. No-op mirrors the trait default.
    #[inline]
    fn validate_adapter_manifest(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    // Axum has no adapter-specific key naming constraint on
    // app-config keys. Trait default no-op.
    #[inline]
    fn validate_app_config_keys(&self, _keys: &[&str]) -> Result<(), String> {
        Ok(())
    }

    // Axum has no adapter-specific canonicalisation rule on typed
    // secret store bindings. Trait default no-op.
    #[inline]
    fn validate_typed_secrets(&self, _entries: &[TypedSecretEntry<'_>]) -> Result<(), String> {
        Ok(())
    }

    fn provision(
        &self,
        manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        _deployed: Option<&AdapterDeployedState>,
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        match mode {
            ProvisionMode::Cloud => {}
            ProvisionMode::Local => {
                return provision_local::provision(manifest_root, stores, dry_run)
            }
            // ProvisionMode is #[non_exhaustive]; explicit error so a
            // future mode variant doesn't quietly fall through.
            other => {
                return Err(format!(
                    "axum adapter does not implement provision mode {other:?}"
                ))
            }
        }
        //: axum has no remote resources. Print one note per
        // declared store id so the operator sees the CLI heard
        // them — same shape `dry_run` would have, since there is
        // nothing to actually perform.
        let mut out = Vec::with_capacity(
            stores
                .kv
                .len()
                .saturating_add(stores.config.len())
                .saturating_add(stores.secrets.len()),
        );
        for store in stores.kv {
            let logical = store.logical.as_str();
            out.push(format!(
                "axum KV store `{logical}` is in-memory; nothing to provision"
            ));
        }
        for store in stores.config {
            // Axum reads `.edgezero/local-config-<logical>.json`.
            // The platform name is informational here -- the env
            // overlay isn't used for local file paths because the
            // path encoding is the spec's canonical form.
            let logical = store.logical.as_str();
            out.push(format!(
                "axum config store `{logical}` reads `.edgezero/local-config-{logical}.json`; nothing to provision"
            ));
        }
        for store in stores.secrets {
            let logical = store.logical.as_str();
            out.push(format!(
                "axum secret store `{logical}` reads env vars; nothing to provision"
            ));
        }
        if out.is_empty() {
            out.push("axum has no declared stores to provision".to_owned());
        }
        Ok(ProvisionOutcome::from_status_lines(out))
    }

    fn provision_typed(
        &self,
        manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        typed_secrets: &[TypedSecretEntry<'_>],
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        // Axum has no cloud secret store: cloud is a documented no-op.
        // Local mode appends `<key_value>=` lines to `.edgezero/.env`
        // (unquoted empty value — the loosest `.env` form). The
        // operator fills in the actual secret by editing the file.
        // `append_lines_dedup` handles parent-dir creation so
        // `.edgezero/` gets auto-created on the first-run case.
        if !matches!(mode, ProvisionMode::Local) {
            return Ok(ProvisionOutcome::default());
        }
        let env_path = manifest_root.join(".edgezero").join(".env");
        let lines: Vec<String> = typed_secrets
            .iter()
            .map(|entry| format!("{}=", entry.key_value))
            .collect();
        append_lines_dedup_with_header(&env_path, Some(EDGEZERO_PROVISION_HEADER), &lines, dry_run)
            .map_err(|err| format!("write {}: {err}", env_path.display()))?;
        let status_lines = vec![format!(
            "axum: wrote {} secret placeholders to {}",
            typed_secrets.len(),
            env_path.display()
        )];
        Ok(ProvisionOutcome::from_status_lines(status_lines))
    }

    fn push_config_entries(
        &self,
        manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        _push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: axum is local-only. Push writes the same flat
        // `string -> string` JSON object `AxumConfigStore` reads
        // back from `.edgezero/local-config-<id>.json`. The path
        // is keyed on the LOGICAL id, not the env-resolved
        // platform name -- the local file flow is the spec's
        // canonical form and isn't subject to the per-store env
        // overlay (which targets platform store names, not local
        // file paths).
        let logical = store.logical.as_str();
        let local_dir = manifest_root.join(".edgezero");
        let target = local_dir.join(format!("local-config-{logical}.json"));
        if dry_run {
            return Ok(vec![format!(
                "would write {} entries to {}",
                entries.len(),
                target.display()
            )]);
        }
        fs::create_dir_all(&local_dir)
            .map_err(|err| format!("failed to create {}: {err}", local_dir.display()))?;
        // Upsert into any existing map so a `config push --key
        // app_config_staging` doesn't wipe a previously-pushed
        // `app_config` blob (spec 12.7 requires default + staging
        // to coexist for the `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY`
        // override to switch between them). The map is owned (rather
        // than borrowed) so we can merge old + new without lifetime
        // surgery on the slice.
        let mut map: BTreeMap<String, String> = match fs::read_to_string(&target) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|err| {
                format!(
                    "failed to parse existing {}: {err} (expected a JSON object of key->envelope)",
                    target.display()
                )
            })?,
            _ => BTreeMap::new(),
        };
        for (key, value) in entries {
            map.insert(key.clone(), value.clone());
        }
        let json = serde_json::to_string_pretty(&map)
            .map_err(|err| format!("failed to serialize config to JSON: {err}"))?;
        fs::write(&target, json)
            .map_err(|err| format!("failed to write {}: {err}", target.display()))?;
        Ok(vec![format!(
            "wrote {} entries to {} ({} total keys after upsert)",
            entries.len(),
            target.display(),
            map.len(),
        )])
    }

    fn push_config_entries_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        store: &ResolvedStoreId,
        entries: &[(String, String)],
        push_ctx: &AdapterPushContext<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Axum is local-only: the default push already writes
        // `.edgezero/local-config-<id>.json`, which is what the
        // running dev server reads. `--local` is therefore the
        // same as the default; we delegate and prepend a notice
        // so the operator who typed `--local` for parity with
        // fastly/cloudflare knows there was nothing extra to do.
        let mut lines = self.push_config_entries(
            manifest_root,
            adapter_manifest_path,
            component_selector,
            store,
            entries,
            push_ctx,
            dry_run,
        )?;
        let notice =
            "axum push is always local: `--local` has no separate effect (writes the same `.edgezero/local-config-<id>.json` either way)".to_owned();
        lines.insert(0, notice);
        Ok(lines)
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
        // Axum has no "remote" — delegate to the local impl.
        // The local JSON file IS the live state for the running dev server.
        self.read_config_entry_local(
            manifest_root,
            adapter_manifest_path,
            component_selector,
            store,
            key,
            push_ctx,
        )
    }

    fn read_config_entry_local(
        &self,
        manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        // Axum reads `.edgezero/local-config-<logical>.json`.
        // The path is keyed on the LOGICAL id (matching
        // `push_config_entries`), not the env-resolved platform name.
        let path = manifest_root
            .join(".edgezero")
            .join(format!("local-config-{}.json", store.logical));
        match fs::read_to_string(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ReadConfigEntry::MissingStore),
            Err(err) => Err(format!("failed to read {}: {err}", path.display())),
            Ok(raw) => {
                let map: BTreeMap<String, String> = serde_json::from_str(&raw)
                    .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
                match map.get(key) {
                    Some(value) => Ok(ReadConfigEntry::Present(value.clone())),
                    None => Ok(ReadConfigEntry::MissingKey),
                }
            }
        }
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: axum is Multi for KV (local file dirs) and Config
        // (local JSON files), Single for Secrets (env vars).
        &["secrets"]
    }

    fn synthesise_baseline_manifest(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        app_name: &str,
        _deployed: Option<&AdapterDeployedState>,
    ) -> Result<Vec<(PathBuf, String)>, String> {
        // Axum's manifest is pure operator-facing dev-server config
        // (host, port, crate name, crate dir). There are no cloud
        // identifiers to weave in, and provision's merge path is a
        // no-op on the file. The baseline is emitted so a fresh
        // clone's `provision --local` gets a runnable `axum.toml`
        // without needing the operator to hand-author one -- same
        // model as Cloudflare / Fastly / Spin.
        let rel = adapter_manifest_path.map_or_else(|| PathBuf::from("axum.toml"), PathBuf::from);
        // Prefer the ACTUAL adapter crate name from the
        // `Cargo.toml` next to the manifest -- honours the
        // operator-tracked `[adapters.axum.adapter].crate` path
        // when it points at a rename like `crates/server`. Fall
        // back to the scaffold convention only when the Cargo.toml
        // is absent or unreadable (typically a first-run scaffold
        // where the file gets written moments later).
        let crate_name = cli_support::read_adapter_crate_name(manifest_root, adapter_manifest_path)
            .unwrap_or_else(|| {
                if app_name.is_empty() {
                    "app-adapter-axum".to_owned()
                } else {
                    format!("{app_name}-adapter-axum")
                }
            });
        Ok(vec![(rel, run::synthesise_axum_toml(&crate_name))])
    }
}

#[inline]
pub fn register() {
    register_adapter(&AXUM_ADAPTER);
    register_adapter_blueprint(&AXUM_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn adapter_name_is_axum() {
        assert_eq!(AXUM_ADAPTER.name(), "axum");
    }

    #[test]
    fn blueprint_has_correct_id() {
        assert_eq!(AXUM_BLUEPRINT.id, "axum");
        assert_eq!(AXUM_BLUEPRINT.display_name, "Axum");
    }

    // ---------- push_config_entries ----------

    #[test]
    fn push_writes_flat_json_to_local_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        let lines = AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("app_config"),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("wrote 2 entries"),
            "status line names count: {lines:?}"
        );
        let json_path = dir.path().join(".edgezero/local-config-app_config.json");
        let raw = fs::read_to_string(&json_path).expect("read written file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed["greeting"], "hello");
        assert_eq!(parsed["service.timeout_ms"], "1500");
    }

    #[test]
    fn push_dry_run_does_not_create_local_dir_or_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let lines = AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("app_config"),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        assert!(
            lines[0].contains("would write 1 entries"),
            "dry-run line: {lines:?}"
        );
        assert!(
            !dir.path().join(".edgezero").exists(),
            ".edgezero must not exist after dry-run"
        );
    }

    #[test]
    fn push_creates_dot_edgezero_directory_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![("key".to_owned(), "value".to_owned())];
        AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("x"),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");
        assert!(dir.path().join(".edgezero").is_dir(), ".edgezero created");
    }

    #[test]
    fn push_with_empty_entries_writes_empty_json_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("empty"),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds even with no entries");
        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-empty.json"))
            .expect("read written file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed, serde_json::json!({}));
    }

    // ---------- read_config_entry / read_config_entry_local ----------

    #[test]
    fn read_config_entry_local_returns_missing_store_when_file_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = AxumCliAdapter
            .read_config_entry_local(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("app_config"),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("infallible on missing file");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "missing file => MissingStore"
        );
    }

    #[test]
    fn read_config_entry_local_returns_missing_key_when_key_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write a JSON file with one key so the store exists, but the
        // requested key is not in it.
        let local_dir = dir.path().join(".edgezero");
        fs::create_dir_all(&local_dir).expect("create dir");
        fs::write(
            local_dir.join("local-config-app_config.json"),
            r#"{"other_key": "value"}"#,
        )
        .expect("write");
        let result = AxumCliAdapter
            .read_config_entry_local(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("app_config"),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("infallible on missing key");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "key absent => MissingKey"
        );
    }

    #[test]
    fn read_config_entry_local_returns_present_when_key_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local_dir = dir.path().join(".edgezero");
        fs::create_dir_all(&local_dir).expect("create dir");
        fs::write(
            local_dir.join("local-config-app_config.json"),
            r#"{"greeting": "hello-axum"}"#,
        )
        .expect("write");
        let result = AxumCliAdapter
            .read_config_entry_local(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical("app_config"),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("key present");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, "hello-axum", "value matches");
    }

    #[test]
    fn read_config_entry_delegates_to_local() {
        // Axum has no remote: read_config_entry and read_config_entry_local
        // must return the same result for the same inputs.
        let dir = tempfile::tempdir().expect("tempdir");
        let local_dir = dir.path().join(".edgezero");
        fs::create_dir_all(&local_dir).expect("create dir");
        fs::write(
            local_dir.join("local-config-app_config.json"),
            r#"{"greeting": "hello-axum"}"#,
        )
        .expect("write");
        let store = ResolvedStoreId::from_logical("app_config");
        let ctx = AdapterPushContext::new();
        let via_local = AxumCliAdapter
            .read_config_entry_local(dir.path(), None, None, &store, "greeting", &ctx)
            .expect("local ok");
        let via_remote = AxumCliAdapter
            .read_config_entry(dir.path(), None, None, &store, "greeting", &ctx)
            .expect("remote ok");
        let ReadConfigEntry::Present(local_val) = via_local else {
            panic!("expected Present from local");
        };
        let ReadConfigEntry::Present(remote_val) = via_remote else {
            panic!("expected Present from remote");
        };
        assert_eq!(local_val, remote_val, "local and remote agree");
    }

    #[test]
    fn read_config_entry_local_errors_on_malformed_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local_dir = dir.path().join(".edgezero");
        fs::create_dir_all(&local_dir).expect("create dir");
        fs::write(
            local_dir.join("local-config-app_config.json"),
            "not valid json {{{",
        )
        .expect("write");
        let result = AxumCliAdapter.read_config_entry_local(
            dir.path(),
            None,
            None,
            &ResolvedStoreId::from_logical("app_config"),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("failed to parse"),
                "error names the failure: {err}"
            ),
            Ok(_) => panic!("expected Err for malformed JSON"),
        }
    }

    /// Spec 12.7: pushing two blobs under different keys (e.g.
    /// `app_config` + `app_config_staging`) must leave both keys
    /// readable so the runtime
    /// `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY` override can
    /// switch between them. Prior to the upsert fix the second push
    /// wiped the first by wholesale-rewriting the JSON map.
    #[test]
    fn push_config_entries_preserves_sibling_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ResolvedStoreId::from_logical("app_config");
        let ctx = AdapterPushContext::new();

        AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &store,
                &[("app_config".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &ctx,
                false,
            )
            .expect("first push");
        AxumCliAdapter
            .push_config_entries(
                dir.path(),
                None,
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

        let raw = fs::read_to_string(dir.path().join(".edgezero/local-config-app_config.json"))
            .expect("read");
        let map: BTreeMap<String, String> = serde_json::from_str(&raw).expect("parse map");
        assert_eq!(
            map.get("app_config").map(String::as_str),
            Some("{\"envelope\":\"A\"}"),
            "default key must survive sibling push: {raw}"
        );
        assert_eq!(
            map.get("app_config_staging").map(String::as_str),
            Some("{\"envelope\":\"B\"}"),
            "staging key must be present: {raw}"
        );
    }

    // ---------- provision_typed (Local mode) — secret placeholders ----------

    #[test]
    fn axum_provision_typed_appends_secret_placeholders_to_edgezero_env() {
        // Fixture: no `.edgezero/` pre-existing (append_lines_dedup
        // creates it via parent-dir handling). provision_typed writes
        // `<key_value>=` per entry — unquoted empty value.
        let dir = tempdir().unwrap();
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        let outcome = AxumCliAdapter
            .provision_typed(
                dir.path(),
                None,
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env_path = dir.path().join(".edgezero/.env");
        assert!(env_path.exists(), ".env exists: {}", env_path.display());
        let env = fs::read_to_string(&env_path).unwrap();
        assert!(
            env.lines().any(|line| line == "demo_api_token="),
            "unquoted empty-value placeholder present: {env}"
        );
        assert!(
            outcome
                .status_lines
                .iter()
                .any(|line| line.contains(&env_path.display().to_string())),
            "status line names the .env path: {:?}",
            outcome.status_lines
        );
        assert!(
            outcome.deployed.is_none(),
            "local provision_typed returns no deployed state"
        );
    }

    #[test]
    fn axum_provision_typed_creates_dot_edgezero_if_missing() {
        // No `.edgezero/` pre-existing. append_lines_dedup (Task 16c)
        // creates parent dirs, so the first-run case works without an
        // explicit `create_dir_all` in provision_typed.
        let dir = tempdir().unwrap();
        assert!(
            !dir.path().join(".edgezero").exists(),
            "sanity: .edgezero/ must NOT pre-exist"
        );
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        AxumCliAdapter
            .provision_typed(
                dir.path(),
                None,
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        assert!(
            dir.path().join(".edgezero").is_dir(),
            ".edgezero/ auto-created via append_lines_dedup parent-dir handling"
        );
        assert!(
            dir.path().join(".edgezero/.env").exists(),
            ".env landed inside auto-created .edgezero/"
        );
    }

    #[test]
    fn axum_provision_typed_cloud_mode_is_a_no_op() {
        // Cloud is a no-op: axum has no cloud secret store. The load-
        // bearing negative assertion is that Cloud mode must NOT
        // create `.edgezero/` or `.env`.
        let dir = tempdir().unwrap();
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        let outcome = AxumCliAdapter
            .provision_typed(
                dir.path(),
                None,
                None,
                &entries,
                ProvisionMode::Cloud,
                false,
            )
            .unwrap();
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
            !dir.path().join(".edgezero").exists(),
            "cloud mode must NOT auto-create .edgezero/"
        );
    }

    #[test]
    fn axum_provision_typed_deduplicates_matching_key() {
        // Operator has already filled in the real value. Re-running
        // provision_typed must NOT clobber it with the empty
        // placeholder — append_lines_dedup collapses keys.
        let dir = tempdir().unwrap();
        let dot_edgezero = dir.path().join(".edgezero");
        fs::create_dir_all(&dot_edgezero).unwrap();
        let env_path = dot_edgezero.join(".env");
        fs::write(&env_path, "demo_api_token=operator_value\n").unwrap();
        let entries = [TypedSecretEntry::new(
            "default",
            "api_token",
            "demo_api_token",
        )];
        AxumCliAdapter
            .provision_typed(
                dir.path(),
                None,
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(&env_path).unwrap();
        assert!(
            env.contains("demo_api_token=operator_value"),
            "operator's real value survives: {env}"
        );
        let token_lines = env
            .lines()
            .filter(|line| {
                let after_hash = line.trim_start().strip_prefix('#').unwrap_or(line);
                after_hash.trim_start().starts_with("demo_api_token=")
            })
            .count();
        assert_eq!(
            token_lines, 1,
            "exactly one demo_api_token line remains: {env}"
        );
    }

    #[test]
    fn axum_provision_typed_handles_multiple_entries() {
        // Multiple TypedSecretEntry values across different store_ids.
        // Every key_value must land as a `<key_value>=` line, exactly
        // once each.
        let dir = tempdir().unwrap();
        let entries = [
            TypedSecretEntry::new("default", "api_token", "demo_api_token"),
            TypedSecretEntry::new("default", "hmac_key", "demo_hmac_key"),
            TypedSecretEntry::new("audit", "audit_token", "audit_secret"),
        ];
        AxumCliAdapter
            .provision_typed(
                dir.path(),
                None,
                None,
                &entries,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        for expected in ["demo_api_token=", "demo_hmac_key=", "audit_secret="] {
            let count = env.lines().filter(|line| *line == expected).count();
            assert_eq!(
                count, 1,
                "expected exactly one line `{expected}` in .env: {env}"
            );
        }
    }
}
