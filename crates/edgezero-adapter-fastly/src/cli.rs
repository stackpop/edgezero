use std::env;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

use crate::chunked_config::{prepare_fastly_config_entries, resolve_fastly_config_value};
use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    register_adapter, Adapter, AdapterAction, AdapterPushContext, ProvisionStores, ReadConfigEntry,
    ResolvedStoreId,
};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use walkdir::WalkDir;

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
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "fastly_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "fastly_fastly_toml",
        contents: include_str!("templates/fastly.toml.hbs"),
    },
];

const FASTLY_INSTALL_HINT: &str =
    "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

struct FastlyCliAdapter;

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
enum ConfigStoreLookup {
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
    reason = "see the explanatory block comment immediately above; fastly's no-op defaults for the three validate_* hooks are intentional and documented. `read_config_entry` and `read_config_entry_local` are both overridden below. `single_store_kinds` IS overridden below (returns `&[]`)."
)]
impl Adapter for FastlyCliAdapter {
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
                let artifact = build(args)?;
                log::info!("[edgezero] Fastly build complete -> {}", artifact.display());
                Ok(())
            }
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
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
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        // Fastly is Multi for every store kind. Each id maps 1:1
        // to a Fastly resource (kv-store / config-store /
        // secret-store) created via the Fastly CLI; the manifest
        // writeback declares the resource link for `fastly
        // compute deploy` and the local viceroy server.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for provision"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);

        let mut out = Vec::new();
        for (kind, ids) in [
            ("kv", stores.kv),
            ("config", stores.config),
            ("secret", stores.secrets),
        ] {
            for store in ids {
                // Fastly setup tables key on the resource name the
                // CLI creates. The runtime resolves that same name
                // via `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`,
                // so provision must use the env-resolved PLATFORM
                // name -- the logical id stays in status lines for
                // human-facing wording.
                let logical = store.logical.as_str();
                let name = store.platform.as_str();
                if dry_run {
                    out.push(format!(
                        "would run `fastly {kind}-store create --name={name}` and append [setup.{kind}_stores.{name}] to {} (logical id `{logical}`)",
                        fastly_path.display()
                    ));
                    continue;
                }
                if setup_block_present(&fastly_path, kind, name)? {
                    out.push(format!(
                        "fastly {kind}-store `{name}` (logical id `{logical}`) already declared in {}; skipping. To force a fresh remote: delete the [setup.{kind}_stores.{name}] block AND run `fastly {kind}-store delete --name={name}` (the old remote store lingers otherwise), then re-run provision.",
                        fastly_path.display()
                    ));
                    continue;
                }
                create_fastly_store(kind, name)?;
                // If the platform store was created but the
                // writeback fails, remote state and the local
                // manifest are out of sync. Re-running `provision`
                // would attempt to create the platform store again
                // and fail with "already exists". Surface the
                // recovery path explicitly so the operator isn't
                // stuck.
                append_fastly_setup(&fastly_path, kind, name).map_err(|err| {
                    format!(
                        "fastly {kind}-store `{name}` (logical id `{logical}`) was created remotely, but writeback to {path} failed: {err}\n  To recover, either:\n    1. Manually append `[setup.{kind}_stores.{name}]` to {path} and re-run, or\n    2. Delete the orphan remote store via `fastly {kind}-store delete --name={name}` and re-run `edgezero provision --adapter fastly`.",
                        path = fastly_path.display()
                    )
                })?;
                // Fastly's `[setup.<kind>_stores.<name>]` table is
                // consumed ONLY when `fastly compute deploy` is
                // creating a NEW service. If `service_id` is
                // already present in fastly.toml, the service has
                // been deployed at least once and subsequent
                // deploys skip `[setup]` entirely — so the store
                // exists in the account but has no resource link
                // tying it to a service version, and the running
                // Compute service can't open it.
                //
                // Detect that case and EMIT the exact one-shot
                // command the operator should run to link the
                // store. We deliberately don't auto-run it: the
                // link cones the active version (`--autoclone`),
                // and silently mutating an already-deployed
                // service is surprising. The instruction names
                // both the store-id lookup AND the link command so
                // the operator can audit before committing.
                let post_create_note = read_fastly_service_id(&fastly_path)?.map(|svc_id| {
                    format!(
                        "  fastly.toml declares `service_id = \"{svc_id}\"`, so this service is already deployed — `[setup]` will NOT be re-run on the next `fastly compute deploy`. The store exists in the account but is NOT yet linked to the service. To finish provisioning, look up the store id with `fastly {kind}-store list --json` (match by name=`{name}`), then run:\n    fastly resource-link create --service-id={svc_id} --resource-id=<STORE-ID> --version=latest --autoclone --name={name}\n  (the link clones the active version so existing traffic is not affected until you `fastly service-version activate`)."
                    )
                });
                let mut line = format!(
                    "created fastly {kind}-store `{name}` (logical id `{logical}`); appended setup tables to {}",
                    fastly_path.display()
                );
                if let Some(note) = post_create_note {
                    line.push('\n');
                    line.push_str(&note);
                }
                out.push(line);
            }
        }
        if out.is_empty() {
            out.push("fastly has no declared stores to provision".to_owned());
        }
        Ok(out)
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
        // Resolve the platform config-store id on demand via
        // `fastly config-store list --json` (matched by name =
        // `store.platform`), then `fastly config-store-entry update
        // --store-id=<id> --key=<k> --upsert --stdin` per physical
        // entry. Entries are logical blob-envelope entries from
        // the CLI (one (key, envelope_json) per push); oversized
        // Fastly values are expanded below into chunk entries plus
        // a root pointer by `chunked_config::prepare_fastly_config_entries`.
        let logical = store.logical.as_str();
        let name = store.platform.as_str();
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to fastly config-store `{name}` (logical id `{logical}`)"
            )]);
        }
        // Expand each logical (key, envelope_json) into physical entries
        // via the chunk-pointer helper. Entries ≤ 8 000 chars go through
        // as a single direct entry; larger envelopes are split into
        // content-addressed chunks with a root pointer written LAST.
        // Collect all physical entries before any writes so pointer-too-
        // large errors surface before touching the remote store.
        let mut physical_entries: Vec<(String, String)> = Vec::new();
        for (key, body) in entries {
            let expanded = prepare_fastly_config_entries(key, body)?;
            physical_entries.extend(expanded);
        }
        if dry_run {
            // Report intent without shelling out. One line per logical key
            // noting whether it would be direct or chunked, plus chunk count.
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would resolve fastly config-store `{name}` (logical id `{logical}`) via `fastly config-store list --json` and push entries:"
            ));
            for (key, body) in entries {
                let expanded = prepare_fastly_config_entries(key, body)
                    .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
                if expanded.len() == 1 {
                    out.push(format!(
                        "  would push `{key}` as direct entry ({}B)",
                        body.len()
                    ));
                } else {
                    let chunk_count = expanded.len().saturating_sub(1);
                    out.push(format!(
                        "  would push `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                        body.len()
                    ));
                }
            }
            return Ok(out);
        }
        let resolved_id = resolve_remote_config_store_id(name)?;
        push_entries_with_committer(&physical_entries, |key, value| {
            create_config_store_entry(&resolved_id, key, value)
        })?;
        Ok(vec![format!(
            "pushed {} physical entries ({} logical) to fastly config-store `{name}` (logical id `{logical}`, id={resolved_id})",
            physical_entries.len(),
            entries.len()
        )])
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
        // Local-emulator path: edit
        // `[local_server.config_stores.<platform>.contents]` in
        // `fastly.toml`. Viceroy reads it on startup, so a
        // subsequent `fastly compute serve` exposes the new values
        // to the wasm component. No shell-out to the production
        // Fastly CLI -- the operator may not be authenticated and
        // wouldn't want a local push to touch production anyway.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for config push --local"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);
        let logical = store.logical.as_str();
        let name = store.platform.as_str();
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to `[local_server.config_stores.{name}]` in {} (logical id `{logical}`)",
                fastly_path.display()
            )]);
        }
        // Expand logical entries into physical entries (chunks + pointer).
        let mut physical_entries: Vec<(String, String)> = Vec::new();
        for (key, body) in entries {
            let expanded = prepare_fastly_config_entries(key, body)?;
            physical_entries.extend(expanded);
        }
        if dry_run {
            let mut out = Vec::with_capacity(entries.len().saturating_add(1));
            out.push(format!(
                "would edit `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`) with entries:",
                fastly_path.display(),
            ));
            for (key, body) in entries {
                let expanded = prepare_fastly_config_entries(key, body)
                    .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
                if expanded.len() == 1 {
                    out.push(format!(
                        "  would set `{key}` as direct entry ({}B)",
                        body.len()
                    ));
                } else {
                    let chunk_count = expanded.len().saturating_sub(1);
                    out.push(format!(
                        "  would set `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                        body.len()
                    ));
                }
            }
            return Ok(out);
        }
        write_fastly_local_config_store(&fastly_path, name, &physical_entries)?;
        Ok(vec![format!(
            "wrote {} physical entries ({} logical) to `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`); restart `fastly compute serve` to pick up changes",
            physical_entries.len(),
            entries.len(),
            fastly_path.display()
        )])
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
        // Shell out to `fastly config-store-entry describe
        // --store-id=<id> --key=<key> --json`, resolve the store id on
        // demand via `fastly config-store list --json`, then parse the
        // JSON response.
        let name = store.platform.as_str();
        let store_id = match resolve_remote_config_store_id(name) {
            Ok(id) => id,
            Err(err) => {
                // "not found" from resolve means the store doesn't exist.
                let lower = err.to_ascii_lowercase();
                if lower.contains("not found")
                    || lower.contains("did you run")
                    || lower.contains("no fastly config-store matches")
                {
                    return Ok(ReadConfigEntry::MissingStore);
                }
                return Err(err);
            }
        };
        let store_arg = format!("--store-id={store_id}");
        let key_arg = format!("--key={key}");
        let output = Command::new("fastly")
            .args([
                "config-store-entry",
                "describe",
                store_arg.as_str(),
                key_arg.as_str(),
                "--json",
            ])
            .output()
            .map_err(|err| {
                if err.kind() == ErrorKind::NotFound {
                    format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
                } else {
                    format!("failed to spawn `fastly`: {err}")
                }
            })?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse the JSON and extract the `item_value` field.
            let parsed: serde_json::Value =
                serde_json::from_str(&stdout).map_err(|err| {
                    format!(
                        "failed to parse `fastly config-store-entry describe` JSON: {err}\nraw stdout: {stdout}"
                    )
                })?;
            let value = parsed
                .get("item_value")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "`fastly config-store-entry describe` JSON has no string `item_value` field; \
                         fastly CLI may have changed its output schema. Raw stdout: {stdout}"
                    )
                })?;
            // Resolve chunk pointers: if `value` is a direct BlobEnvelope it
            // passes through unchanged; if it is a chunk pointer the chunks
            // are fetched from the same store and reconstructed.
            let resolved = resolve_fastly_config_value(key, value.to_owned(), |chunk_key| {
                fetch_remote_config_store_entry(&store_id, chunk_key)
            })?;
            return Ok(ReadConfigEntry::Present(resolved));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("not found") || lower.contains("does not exist") || lower.contains("404")
        {
            return Ok(ReadConfigEntry::MissingKey);
        }
        Err(format!(
            "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` exited with status {}\nstderr: {}",
            output.status,
            stderr.trim()
        ))
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
        // Read from `[local_server.config_stores.<platform_name>.contents]`
        // in fastly.toml — the same section `push_config_entries_local` writes.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.fastly.adapter].manifest must point at fastly.toml for config diff --local"
                    .to_owned(),
            );
        };
        let fastly_path = manifest_root.join(rel);
        let name = store.platform.as_str();
        let raw = match fs::read_to_string(&fastly_path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(ReadConfigEntry::MissingStore)
            }
            Err(err) => {
                return Err(format!("failed to read {}: {err}", fastly_path.display()));
            }
        };
        let doc: toml_edit::DocumentMut = raw
            .parse()
            .map_err(|err| format!("failed to parse {}: {err}", fastly_path.display()))?;
        // Probe `[local_server.config_stores.<name>]` — if absent, the store
        // has not been seeded locally yet.
        let Some(contents) = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(name))
            .and_then(|store_tbl| store_tbl.get("contents"))
        else {
            return Ok(ReadConfigEntry::MissingStore);
        };
        // The contents table is `key = "value"` pairs.
        match contents.get(key) {
            Some(item) => {
                let value = item.as_str().ok_or_else(|| {
                    format!(
                        "`[local_server.config_stores.{name}.contents].{key}` in {} is not a string",
                        fastly_path.display()
                    )
                })?;
                // Resolve chunk pointers using the same toml contents table.
                let resolved =
                    resolve_fastly_config_value(key, value.to_owned(), |chunk_key| match contents
                        .get(chunk_key)
                    {
                        Some(chunk_item) => {
                            let chunk_val = chunk_item.as_str().ok_or_else(|| {
                                format!(
                                    "chunk key `{chunk_key}` in {} is not a string",
                                    fastly_path.display()
                                )
                            })?;
                            Ok(Some(chunk_val.to_owned()))
                        }
                        None => Ok(None),
                    })?;
                Ok(ReadConfigEntry::Present(resolved))
            }
            None => Ok(ReadConfigEntry::MissingKey),
        }
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        // Explicit `&[]` rather than inheriting the trait default,
        // so the "Multi for every store kind" intent is documented
        // at the call site. Fastly KV / Config / Secrets all
        // support multiple distinct platform resources per kind,
        // unlike spin's flat-namespace single-store model.
        &[]
    }
}

/// Fetch a single entry value from a remote Fastly Config Store entry by
/// key, using `fastly config-store-entry describe --store-id=<id> --key=<k>
/// --json`. Used by the chunk-pointer resolver to fan out to chunk entries.
///
/// Returns:
/// - `Ok(Some(value))` when the entry exists.
/// - `Ok(None)` when the entry is absent (not-found / 404 / does not exist).
/// - `Err(...)` on adapter or parse errors.
///
/// # Errors
/// Returns an error if `fastly` isn't on `PATH`, spawning fails, the JSON
/// cannot be parsed, or the CLI exits with an unexpected non-zero status.
fn fetch_remote_config_store_entry(store_id: &str, key: &str) -> Result<Option<String>, String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "describe",
            store_arg.as_str(),
            key_arg.as_str(),
            "--json",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|err| {
            format!(
                "failed to parse `fastly config-store-entry describe` JSON for chunk \
                     key `{key}`: {err}\nraw stdout: {stdout}"
            )
        })?;
        let value = parsed
            .get("item_value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry describe` JSON has no string `item_value` \
                     field for chunk key `{key}`; fastly CLI may have changed its output schema. \
                     Raw stdout: {stdout}"
                )
            })?;
        return Ok(Some(value.to_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not found") || lower.contains("does not exist") || lower.contains("404") {
        return Ok(None);
    }
    Err(format!(
        "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` \
         exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

/// Shell out to `fastly <kind>-store create --name=<platform-name>`. The
/// caller resolves `<platform-name>` from `EDGEZERO__STORES__<KIND>__<ID>__NAME`
/// (falling back to the logical id), so this helper takes whatever the
/// caller hands it and does not re-translate. Returns `Ok(())` on success;
/// surfaces the CLI's stderr verbatim on failure (including the "already
/// exists" error, which is the caller's signal to fix the toml or use a
/// different name).
///
/// # Errors
/// Returns an error if `fastly` isn't on `PATH`, the child fails to
/// spawn, or the exit status is non-zero.
fn create_fastly_store(kind: &str, name: &str) -> Result<(), String> {
    let subcommand = format!("{kind}-store");
    let name_arg = format!("--name={name}");
    let output = Command::new("fastly")
        .args([subcommand.as_str(), "create", name_arg.as_str()])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    // Idempotency: the fastly CLI returns non-zero with an
    // "already exists" message when a store of this name was
    // created by a prior provision run. Treat that as success so
    // the operator's recovery path -- "either manually append the
    // setup block or delete the remote and re-run provision" --
    // doesn't get blocked. The append step is itself idempotent,
    // so re-running provision after a writeback failure is the
    // documented recovery and now actually works.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if looks_like_already_exists(&stderr, kind) {
        return Ok(());
    }
    Err(format!(
        "`fastly {subcommand} create --name={name}` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

/// Heuristic: does the stderr blob look like a "store of this
/// kind, by this name, already exists" failure from the fastly
/// CLI? Different CLI versions phrase this slightly differently
/// ("a kv-store with that name already exists",
/// `"Conflict: duplicate kv_store name"`, etc.); we require BOTH
/// a conflict-signal keyword AND a store-kind reference so an
/// unrelated 409 ("Error: 409 Conflict on /service/...") cannot
/// be misread as idempotent success. The earlier wider heuristic
/// would have swallowed any stderr containing the word
/// "conflict" and let provision march on to writeback against a
/// nonexistent store, surfacing as a confusing deploy-time error.
fn looks_like_already_exists(stderr: &str, kind: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    let conflict_signal = lower.contains("already exists")
        || (lower.contains("duplicate") && lower.contains("name"))
        || lower.contains("conflict");
    if !conflict_signal {
        return false;
    }
    // Accept the three common spellings of `<kind>-store` /
    // `<kind>_store` / `<kind> store` so a fastly CLI version
    // bump that reshuffles punctuation still hits.
    let dashed = format!("{kind}-store");
    let underscored = format!("{kind}_store");
    let spaced = format!("{kind} store");
    lower.contains(&dashed) || lower.contains(&underscored) || lower.contains(&spaced)
}

/// Read the top-level `service_id` from `fastly.toml`. Returns
/// `Ok(None)` when the file is absent (scaffold state before first
/// `fastly compute deploy`) or when `service_id` is missing /
/// empty. Used by `provision` to detect when an already-deployed
/// service needs a separate resource-link step beyond `[setup]`
/// (which `compute deploy` only consumes on the FIRST deploy).
fn read_fastly_service_id(path: &Path) -> Result<Option<String>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let svc = doc
        .get("service_id")
        .and_then(|item| item.as_str())
        .map(str::to_owned)
        .filter(|svc_id| !svc_id.is_empty());
    Ok(svc)
}

/// Probe `fastly.toml` for the existence of `[setup.<kind>_stores.<id>]`.
/// Treats a missing file as "not present" so the first provision call
/// can create it.
///
/// Why only `[setup]` (no longer `[local_server]`): an empty
/// `[local_server.<kind>_stores.<id>]` table doesn't satisfy
/// fastly's local-server schema — config-stores need
/// `format = "inline-toml"` + a contents table, kv/secret stores
/// need a JSON `file = "..."` or an array of `{key, data}` entries.
/// Writing an empty table makes `fastly compute serve` skip the
/// declared store or error at startup. `provision`'s job is the
/// remote / `[setup]` half; local-server stanzas are written by
/// `edgezero config push --adapter fastly --local`
/// (config-stores only), and kv/secret local-server seeding is
/// hand-edited until we add equivalent writers for those kinds.
fn setup_block_present(path: &Path, kind: &str, id: &str) -> Result<bool, String> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let plural = format!("{kind}_stores");
    Ok(doc
        .get("setup")
        .and_then(|root| root.get(plural.as_str()))
        .and_then(|kind_tbl| kind_tbl.get(id))
        .is_some())
}

/// Append `[setup.<kind>_stores.<id>]` to `fastly.toml`. Creates
/// the file (and the parent `[setup]` table) if absent. The block
/// is written as an empty table — that's what
/// `fastly compute deploy` consumes the first time it creates a
/// service: the resource-link declaration is enough, and the
/// account-level resource itself is already created in the
/// preceding `create_fastly_store` shellout.
///
/// We DON'T write `[local_server.<kind>_stores.<id>]` here: see
/// `setup_block_present`'s doc for the schema rationale. The local-
/// server seeding moved to `config push --local` (config-stores
/// only), so provision only owns the remote / setup half.
fn append_fastly_setup(path: &Path, kind: &str, id: &str) -> Result<(), String> {
    use toml_edit::{table, DocumentMut, Item};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let plural = format!("{kind}_stores");
    let parent_entry = doc.entry("setup").or_insert_with(table);
    let parent_tbl = parent_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `setup` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    let kind_entry = parent_tbl
        .entry(plural.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let kind_tbl = kind_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `setup.{plural}` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    if !kind_tbl.contains_key(id) {
        kind_tbl.insert(id, Item::Table(toml_edit::Table::new()));
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

/// Write the local-server config-store entries to `fastly.toml`:
/// `[local_server.config_stores.<platform_name>]` becomes
/// `format = "inline-toml"`, and `[local_server.config_stores.<platform_name>.contents]`
/// gets the flat `key = "value"` pairs (overwriting any previous
/// values). Idempotent — re-running just rewrites `contents`. Other
/// blocks in `fastly.toml` (setup, scripts, the actual `[local_server]`
/// secret stores, etc.) are preserved via `toml_edit`.
fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    use toml_edit::{table, DocumentMut, Item, Table, Value};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let local_server_entry = doc.entry("local_server").or_insert_with(table);
    let local_server_tbl = local_server_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    let config_stores_entry = local_server_tbl
        .entry("config_stores")
        .or_insert_with(|| Item::Table(Table::new()));
    let config_stores_tbl = config_stores_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;

    // Upsert into the existing per-store contents table so a
    // `config push --key app_config_staging` does NOT wipe the
    // previously-pushed `app_config` blob. Spec 12.7 requires
    // default + staging keys to coexist so the runtime
    // EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY env var can
    // switch between them. (Earlier wholesale-replace was a
    // misread of the "stale entries don't linger" property:
    // that applies WITHIN a key (old chunks for the same root
    // become unreferenced when a new chunk-set installs a new
    // pointer), NOT across sibling keys.)
    let store_entry = config_stores_tbl.entry(platform_name).or_insert_with(|| {
        let mut tbl = Table::new();
        tbl.insert("format", toml_edit::value("inline-toml"));
        tbl.insert("contents", Item::Table(Table::new()));
        Item::Table(tbl)
    });
    let store_tbl = store_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores.{platform_name}` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    // Ensure the `format` key is present even on a pre-existing
    // entry that omitted it.
    if !store_tbl.contains_key("format") {
        store_tbl.insert("format", toml_edit::value("inline-toml"));
    }
    let contents_entry = store_tbl
        .entry("contents")
        .or_insert_with(|| Item::Table(Table::new()));
    let contents_tbl = contents_entry.as_table_mut().ok_or_else(|| {
        format!(
            "{}: `local_server.config_stores.{platform_name}.contents` exists but is not a table; refusing to edit in place",
            path.display()
        )
    })?;
    for (key, value) in entries {
        contents_tbl.insert(key, Item::Value(Value::from(value.clone())));
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

// -------------------------------------------------------------------
// `config push` helpers
// -------------------------------------------------------------------

/// Shell out to `fastly config-store-entry create --store-id=<id>
/// --key=<k> --value=<v>` for a single entry. Surfaces fastly's
/// stderr verbatim on failure — including the "entry already
/// exists" error, which is the operator's signal to delete the
/// entry (or use `config-store-entry update` manually) before
/// re-running push.
/// Drive a sequential per-entry commit loop and produce the
/// partial-failure diagnostic when the committer fails mid-way.
/// Pure (no I/O) so the diagnostic shape is unit-testable without
/// the fastly CLI on PATH; production calls it with a closure that
/// shells out via `create_config_store_entry`. On success returns
/// the count of committed entries; on failure returns an error
/// string naming committed / failed / not-attempted keys so the
/// operator can resume from a known boundary.
fn push_entries_with_committer<F>(
    entries: &[(String, String)],
    mut committer: F,
) -> Result<usize, String>
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    let mut pushed: Vec<String> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if let Err(err) = committer(key, value) {
            let remaining: Vec<&str> = entries
                .iter()
                .skip(pushed.len().saturating_add(1))
                .map(|(remaining_key, _)| remaining_key.as_str())
                .collect();
            return Err(format!(
                "fastly push failed at entry `{key}` after committing {committed} of {total} entries; the remaining {remaining_count} entries were NOT pushed.\n  Committed (safe to skip on retry): {pushed:?}\n  Failed: `{key}` — {err}\n  Not attempted (re-push these): {remaining:?}",
                committed = pushed.len(),
                total = entries.len(),
                remaining_count = remaining.len()
            ));
        }
        pushed.push(key.clone());
    }
    Ok(pushed.len())
}

/// Shell `fastly config-store-entry update --upsert --stdin` with
/// the value piped through stdin instead of `--value=<value>` on
/// argv.
///
/// Two reasons for this exact invocation:
///
/// 1. `--upsert` (vs. the original `create` subcommand): the prior
///    `create` form errored on any key that already existed in the
///    config store, which made `config push` non-repeatable —
///    after the first push, every follow-up push triggered by a
///    config edit would fail at the first unchanged key.
///    `update --upsert` is documented as "insert or update", which
///    matches the convergent semantic the other config-push paths
///    already have (axum overwrites the JSON, cloudflare's
///    `wrangler kv bulk put` overwrites, spin's
///    `cloud key-value set` overwrites).
///
/// 2. `--stdin` (vs. `--value=<value>`): `--value=` exposed every
///    config entry's bytes in `ps`/`/proc/<pid>/cmdline` listings
///    AND was bounded by the host's `ARG_MAX` (4 KiB to 256 KiB
///    depending on platform — easy to trip with a JSON blob).
///    `--stdin` reads the value from stdin instead — keeps value
///    bytes out of argv and lifts the size cap to whatever the OS
///    pipe buffer + the CLI's read accept (megabytes in practice).
fn create_config_store_entry(store_id: &str, key: &str, value: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let mut child = Command::new("fastly")
        .args([
            "config-store-entry",
            "update",
            store_arg.as_str(),
            key_arg.as_str(),
            "--upsert",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    // Move stdin OUT of child via `take` so the ChildStdin drops at
    // end of scope — that closes the pipe and lets the CLI see EOF.
    // `child.wait_with_output()` then consumes child cleanly.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `fastly`".to_owned())?;
    stdin
        .write_all(value.as_bytes())
        .map_err(|err| format!("failed to write value to `fastly` stdin: {err}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `fastly`: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry update --store-id={store_id} --key={key} --upsert --stdin` exited with status {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Parse `fastly config-store list --json` output and return the
/// platform `id` of the store whose `name` matches `name`. Accepts
/// both a bare array (`[ {"id": "...", "name": "..."}, ... ]`)
/// and an `{"items": [...]}` envelope so this stays compatible
/// across fastly CLI versions.
fn find_config_store_id(stdout: &str, name: &str) -> ConfigStoreLookup {
    let parsed: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(err) => {
            return ConfigStoreLookup::SchemaDrift(format!("stdout did not parse as JSON: {err}"));
        }
    };
    let Some(array) = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
    else {
        return ConfigStoreLookup::SchemaDrift(format!(
            "expected a bare array `[...]` or an `{{\"items\": [...]}}` envelope; got JSON of shape `{}`",
            shape_summary(&parsed)
        ));
    };
    let mut any_well_formed = false;
    for entry in array {
        let entry_name = entry.get("name").and_then(serde_json::Value::as_str);
        let entry_id = entry.get("id").and_then(serde_json::Value::as_str);
        if entry_name.is_some() && entry_id.is_some() {
            any_well_formed = true;
        }
        if entry_name == Some(name) {
            return entry_id.map_or_else(
                || {
                    ConfigStoreLookup::SchemaDrift(format!(
                        "entry matched name `{name}` but is missing a string `id` field"
                    ))
                },
                |id| ConfigStoreLookup::Found(id.to_owned()),
            );
        }
    }
    if array.is_empty() || any_well_formed {
        ConfigStoreLookup::NotFound
    } else {
        ConfigStoreLookup::SchemaDrift(
            "no entry has both string `name` and `id` fields -- fastly CLI may have changed its output schema"
                .to_owned(),
        )
    }
}

/// One-line type label for a `serde_json::Value` (for diagnostic
/// error messages — not a canonical JSON-schema description).
fn shape_summary(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Resolve the platform config-store id on demand: shell out to
/// `fastly config-store list --json`, parse the JSON, match by
/// `name`. The provision flow doesn't persist this id, so push
/// has to re-fetch every time.
fn resolve_remote_config_store_id(name: &str) -> Result<String, String> {
    let output = Command::new("fastly")
        .args(["config-store", "list", "--json"])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`fastly config-store list --json` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match find_config_store_id(&stdout, name) {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => Err(format!(
            "no fastly config-store matches `{name}` (did you run `edgezero provision --adapter fastly`?)"
        )),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// # Errors
/// Returns an error if the Fastly CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip1",
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
/// Returns an error if the Fastly CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute deploy failed with status {status}"));
    }

    Ok(())
}

fn find_fastly_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "fastly.toml") {
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
            path.file_name().is_some_and(|n| n == "fastly.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate fastly.toml".to_owned());
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
    let target_triple = "wasm32-wasip1";
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(target_triple)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(target_triple)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(target_triple)
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
    register_adapter(&FASTLY_ADAPTER);
    register_adapter_blueprint(&FASTLY_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Fastly CLI serve command (Viceroy) fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())?;

    let status = Command::new("fastly")
        .args(["compute", "serve"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run fastly CLI: {err}"))?;
    if !status.success() {
        return Err(format!("fastly compute serve failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Shared fixture names. Pinning these as consts (instead of
    // inline `"sessions"` / `"app_config"` per call site) keeps the
    // setup-vs-assertion pair in sync -- a typo in one place no
    // longer silently divorces from the other, because both reference
    // the same const. Also names the intent: these are the LOGICAL
    // store ids the fastly adapter operates on, not arbitrary strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

    /// RAII guard: prepends a directory to `$PATH` and restores the original
    /// value on drop.
    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            let original = env::var_os("PATH");
            let new_path = match &original {
                Some(prev) => {
                    let mut accum = OsString::from(extra);
                    accum.push(":");
                    accum.push(prev);
                    accum
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new_path);
            Self { original }
        }
    }

    #[cfg(unix)]
    impl Drop for PathPrepend {
        fn drop(&mut self) {
            match self.original.take() {
                Some(prev) => env::set_var("PATH", prev),
                None => env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("fastly.toml"), "name=\"first\"").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("fastly.toml"), "name=\"second\"").unwrap();

        let found = find_fastly_manifest(&second).unwrap();
        assert_eq!(found, second.join("fastly.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("fastly.toml"), "name = \"demo\"").unwrap();

        let manifest = find_fastly_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("fastly.toml"));
    }

    #[test]
    fn locate_artifact_considers_workspace_target() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(manifest_dir.join("target/wasm32-wasip1/release")).unwrap();
        let artifact = workspace.join("target/wasm32-wasip1/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "demo").unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn read_package_falls_back_to_name() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "name = \"demo\"").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    #[test]
    fn read_package_prefers_package_table() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"demo\"\n").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    // ---------- push_entries_with_committer ----------

    #[test]
    fn push_entries_with_committer_returns_count_when_all_succeed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let pushed = push_entries_with_committer(&entries, |_, _| Ok(())).expect("all succeed");
        assert_eq!(pushed, 3);
    }

    #[test]
    fn push_entries_with_committer_zero_entries_is_ok() {
        let pushed = push_entries_with_committer(&[], |_, _| Ok(())).expect("empty is fine");
        assert_eq!(pushed, 0);
    }

    #[test]
    fn push_entries_with_committer_failure_surfaces_committed_failed_not_attempted() {
        // Mock committer: succeed for first 2 keys, fail at third.
        let entries = vec![
            ("k1".to_owned(), "v1".to_owned()),
            ("k2".to_owned(), "v2".to_owned()),
            ("k3".to_owned(), "v3".to_owned()),
            ("k4".to_owned(), "v4".to_owned()),
            ("k5".to_owned(), "v5".to_owned()),
        ];
        let mut calls: usize = 0;
        let err = push_entries_with_committer(&entries, |key, _| {
            calls = calls.saturating_add(1);
            if key == "k3" {
                Err("simulated fastly stderr".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("middle failure must error");
        // Committer was invoked for k1, k2, k3 and stopped.
        assert_eq!(calls, 3_usize, "no retries beyond failure point");
        // Error names all three categories.
        assert!(err.contains("k1") && err.contains("k2"), "committed: {err}");
        assert!(
            err.contains("Failed: `k3`"),
            "failed entry named exactly: {err}"
        );
        assert!(
            err.contains("k4") && err.contains("k5"),
            "not-attempted: {err}"
        );
        assert!(err.contains("simulated fastly stderr"), "inner err: {err}");
        // Counts are sane.
        assert!(
            err.contains("committing 2 of 5 entries"),
            "committed/total count: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_first_entry_failure_reports_zero_committed() {
        let entries = vec![
            ("only".to_owned(), "val".to_owned()),
            ("never".to_owned(), "tried".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |_, _| Err("nope".to_owned()))
            .expect_err("first-entry failure");
        assert!(err.contains("committing 0 of 2"), "zero committed: {err}");
        assert!(
            err.contains("Failed: `only`"),
            "first-entry failure named: {err}"
        );
        assert!(
            err.contains("never"),
            "second entry as not-attempted: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_last_entry_failure_reports_n_minus_one_committed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |key, _| {
            if key == "c" {
                Err("late failure".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("last-entry failure");
        assert!(err.contains("committing 2 of 3"), "n-1 committed: {err}");
        assert!(
            err.contains("the remaining 0 entries"),
            "zero not-attempted when last fails: {err}"
        );
    }

    // ---------- looks_like_already_exists ----------

    #[test]
    fn looks_like_already_exists_recognises_common_phrasings() {
        // Real-shaped fastly CLI error strings (paraphrased; the
        // CLI varies across versions). Each must be detected so
        // create_fastly_store can treat it as idempotent success.
        assert!(looks_like_already_exists(
            "Error: a kv-store with that name already exists",
            "kv",
        ));
        assert!(looks_like_already_exists(
            "ERROR: Conflict (409): duplicate kv_store name",
            "kv",
        ));
        assert!(looks_like_already_exists(
            "A config-store with this name already exists",
            "config",
        ));
        // Spaced form: some fastly CLI versions emit prose
        // ("kv store"); accept it alongside the punctuated forms.
        assert!(looks_like_already_exists(
            "Error: kv store conflict: name already in use",
            "kv",
        ));
    }

    #[test]
    fn looks_like_already_exists_rejects_unrelated_errors() {
        assert!(!looks_like_already_exists(
            "Error: unauthenticated; run `fastly profile create`",
            "kv",
        ));
        assert!(!looks_like_already_exists(
            "Error: network unreachable",
            "kv",
        ));
        assert!(!looks_like_already_exists("", "kv"));
    }

    #[test]
    fn looks_like_already_exists_rejects_unrelated_conflict_errors() {
        // The earlier wider heuristic swallowed ANY stderr
        // containing "conflict" or "already exists", which would
        // misread an unrelated 409 from a different fastly
        // subcommand (e.g. a service-version conflict during a
        // parallel deploy) as idempotent store-create success.
        // Now we require the kind context too, so unrelated
        // conflicts surface as failures.
        assert!(
            !looks_like_already_exists(
                "Error: 409 Conflict on /service/abc/version/42 -- already exists",
                "kv",
            ),
            "service-version conflict must NOT be misread as kv-store idempotency"
        );
        assert!(
            !looks_like_already_exists(
                "Error: invalid duplicate request; check name resolution",
                "kv",
            ),
            "unrelated `duplicate ... name` AND-match must NOT trigger"
        );
        // And the kind must match: a config-store conflict must
        // not look-like-already-exists for a kv-store create call.
        assert!(
            !looks_like_already_exists("Error: a config-store with that name already exists", "kv",),
            "wrong-kind conflict must NOT trigger"
        );
    }

    // ---------- setup_block_present ----------

    #[test]
    fn setup_block_present_true_when_table_exists() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "name = \"demo\"\n[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        assert!(setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_false_when_id_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n[setup.kv_stores.other]\n").expect("write");
        assert!(!setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_false_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert!(!setup_block_present(&path, "kv", TEST_KV_ID).expect("probe"));
    }

    #[test]
    fn setup_block_present_true_when_only_setup_exists() {
        // Post-F6 (PR #269 round 2): `setup_block_present` only
        // checks `[setup.<kind>_stores.<id>]`. The pre-fix check
        // ALSO required `[local_server.<kind>_stores.<id>]`, but
        // writing an empty `[local_server.*]` table didn't match
        // fastly's local-server schema (config-stores need
        // `format` + contents, kv/secret stores need a JSON file
        // or `{key, data}` entries). Local-server seeding moved
        // to `config push --adapter fastly --local`, so probe
        // only cares about `[setup]` now.
        let dir = tempdir().expect("tempdir");
        let only_setup = dir.path().join("only_setup.toml");
        fs::write(&only_setup, "name = \"demo\"\n[setup.kv_stores.sessions]\n").expect("write");
        assert!(
            setup_block_present(&only_setup, "kv", TEST_KV_ID).expect("probe"),
            "[setup.*] alone is now sufficient: {only_setup:?}"
        );

        let only_local = dir.path().join("only_local.toml");
        fs::write(
            &only_local,
            "name = \"demo\"\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        assert!(
            !setup_block_present(&only_local, "kv", TEST_KV_ID).expect("probe"),
            "[local_server.*] alone is NOT a provisioned-setup signal"
        );
    }

    // ---------- append_fastly_setup ----------

    #[test]
    fn append_fastly_setup_creates_setup_table_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "setup table added: {after}"
        );
        // Post-F6: no `[local_server.*]` write — that empty stanza
        // didn't satisfy fastly's local-server schema and made
        // `fastly compute serve` error or skip the store. Local-
        // server seeding is now `config push --adapter fastly
        // --local`'s job.
        assert!(
            !after.contains("[local_server.kv_stores.sessions]"),
            "[local_server.*] empty table no longer written by provision: {after}"
        );
        assert!(
            after.contains("name = \"demo\""),
            "preserved original keys: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_appends_alongside_existing_kind_tables() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup.kv_stores.cache]\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.cache]"),
            "existing entry kept: {after}"
        );
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "new entry added: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_is_idempotent_on_duplicate_id() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "[setup.kv_stores.sessions]\nfoo = \"keep\"\n").expect("write");
        append_fastly_setup(&path, "kv", TEST_KV_ID).expect("idempotent append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("foo = \"keep\""),
            "did not stomp existing key: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_creates_file_when_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Note: no fs::write — file starts absent.
        append_fastly_setup(&path, "config", TEST_CONFIG_ID).expect("create");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("[setup.config_stores.app_config]"));
        assert!(
            !after.contains("[local_server.config_stores.app_config]"),
            "[local_server.*] no longer written by provision: {after}"
        );
    }

    #[test]
    fn append_fastly_setup_preserves_top_comments() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "# managed by hand -- please keep this line\nname = \"demo\"\n",
        )
        .expect("write");
        append_fastly_setup(&path, "secret", TEST_SECRET_ID).expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("# managed by hand"),
            "preserved comment: {after}"
        );
    }

    // ---------- write_fastly_local_config_store (config push --local) ----------

    #[test]
    fn write_fastly_local_config_store_creates_inline_block_in_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        write_fastly_local_config_store(&path, TEST_CONFIG_ID, &entries).expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains(&format!("[local_server.config_stores.{TEST_CONFIG_ID}]")),
            "store table: {after}"
        );
        assert!(
            after.contains("format = \"inline-toml\""),
            "format field: {after}"
        );
        assert!(
            after.contains(&format!(
                "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
            )),
            "contents table: {after}"
        );
        assert!(after.contains("greeting = \"hello\""), "key 1: {after}");
        assert!(
            after.contains("\"service.timeout_ms\" = \"1500\""),
            "dotted key quoted: {after}"
        );
        assert!(after.contains("name = \"demo\""), "preserved: {after}");
    }

    #[test]
    fn write_fastly_local_config_store_replaces_existing_block_on_re_push() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "stale".to_owned())],
        )
        .expect("first write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "fresh".to_owned())],
        )
        .expect("second write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("greeting = \"fresh\""), "new value: {after}");
        assert!(
            !after.contains("greeting = \"stale\""),
            "stale value dropped: {after}"
        );
    }

    #[test]
    fn write_fastly_local_config_store_preserves_unrelated_blocks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        let original = "\
[setup.kv_stores.sessions]

[[local_server.kv_stores.sessions]]
key = \"__init__\"
data = \"\"

[scripts]
build = \"cargo build --release\"
";
        fs::write(&path, original).expect("write");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
        )
        .expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[setup.kv_stores.sessions]"),
            "setup KV kept: {after}"
        );
        assert!(after.contains("[scripts]"), "scripts table kept: {after}");
        assert!(
            after.contains("build = \"cargo build --release\""),
            "scripts value kept: {after}"
        );
        assert!(
            after.contains(&format!(
                "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
            )),
            "new config_stores block added: {after}"
        );
    }

    #[test]
    fn write_fastly_local_config_store_creates_file_when_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // No fs::write — file absent.
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), "hi".to_owned())],
        )
        .expect("write");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains(&format!(
            "[local_server.config_stores.{TEST_CONFIG_ID}.contents]"
        )));
        assert!(after.contains("greeting = \"hi\""));
    }

    // ---------- provision (dry-run + error path) ----------

    #[test]
    fn provision_dry_run_does_not_invoke_fastly() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        // 1 KV + 1 config + 1 secret = 3 status lines.
        assert_eq!(out.len(), 3);
        assert!(out[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(out[1].contains("would run `fastly config-store create --name=app_config`"));
        assert!(out[2].contains("would run `fastly secret-store create --name=default`"));
        // Manifest untouched.
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated fastly.toml");
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
        let err = FastlyCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("fastly.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["fastly has no declared stores to provision"]);
    }

    #[test]
    fn provision_skips_id_when_setup_block_already_present() {
        // setup_block_present's role in the flow: re-running
        // provision after the user already declared a store in
        // fastly.toml must be a no-op (no shell-out to fastly).
        // We can verify this in a real (non-dry-run) call because
        // the skip path bypasses create_fastly_store entirely.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(
            &path,
            "[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n",
        )
        .expect("write");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = FastlyCliAdapter
            .provision(dir.path(), Some("fastly.toml"), None, &stores, false)
            .expect("skip path succeeds without invoking fastly");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("already declared"), "got: {out:?}");
    }

    // ---------- find_config_store_id ----------

    #[test]
    fn find_config_store_id_matches_bare_array_by_name() {
        let stdout = format!(
            r#"[
                {{"id": "abc123", "name": "{TEST_CONFIG_ID}"}},
                {{"id": "def456", "name": "other_store"}}
            ]"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "abc123"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_tolerates_items_envelope() {
        let stdout = format!(
            r#"{{"items": [
                {{"id": "xyz789", "name": "{TEST_CONFIG_ID}"}}
            ]}}"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "xyz789"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_distinguishes_not_found_from_match_failure() {
        // JSON parses cleanly, entries are well-formed
        // (`name` + `id` strings present), but no entry matches
        // → NotFound. Operator likely needs to run `provision`.
        let stdout = r#"[{"id": "abc", "name": "other"}]"#;
        assert!(matches!(
            find_config_store_id(stdout, "missing"),
            ConfigStoreLookup::NotFound
        ));
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_on_malformed_json() {
        // Unparseable bytes are NOT a "store not found" — they're
        // a "fastly CLI output format changed" signal. Operator
        // needs different recovery (file a bug, pin CLI version)
        // than for the "store doesn't exist yet" case.
        let drift = find_config_store_id("not json", "anything");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "non-JSON stdout must be schema drift, got {drift:?}"
        );
        let empty = find_config_store_id("", "anything");
        assert!(
            matches!(empty, ConfigStoreLookup::SchemaDrift(_)),
            "empty stdout must be schema drift, got {empty:?}"
        );
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_shape_unexpected() {
        // JSON parses but the top-level is neither a bare array
        // nor an `{items: [...]}` envelope.
        let stdout = r#"{"namespace": "fastly", "list": []}"#;
        match find_config_store_id(stdout, "any") {
            ConfigStoreLookup::SchemaDrift(detail) => {
                assert!(
                    detail.contains("bare array") || detail.contains("items"),
                    "schema-drift detail names the expected shapes: {detail}"
                );
            }
            ConfigStoreLookup::Found(id) => panic!("expected SchemaDrift, got Found({id})"),
            ConfigStoreLookup::NotFound => panic!("expected SchemaDrift, got NotFound"),
        }
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_entries_lack_name_id() {
        // Array of objects but none have BOTH string `name` and
        // string `id` fields — suggests schema rename (e.g.
        // fastly renamed `name` → `title`).
        let stdout = format!(r#"[{{"title": "{TEST_CONFIG_ID}", "uid": "abc"}}]"#);
        let drift = find_config_store_id(&stdout, TEST_CONFIG_ID);
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "entries lacking name/id must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_returns_not_found_for_empty_array() {
        // Empty array IS a valid "store doesn't exist yet" signal,
        // not schema drift — fastly CLI legitimately returns `[]`
        // when no config-stores exist.
        let drift = find_config_store_id("[]", "any");
        assert!(
            matches!(drift, ConfigStoreLookup::NotFound),
            "empty array must be NotFound, got {drift:?}"
        );
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_does_not_invoke_fastly() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        // First line names the resolve+publish flow; subsequent lines preview
        // each key the push would create (so callers can eyeball the keyset
        // before running for real).
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would resolve fastly config-store `app_config`")
                && out[0].contains("push entries"),
            "dry-run header describes the would-be flow: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run lists `greeting`: {out:?}"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("`feature.new_checkout`")),
            "dry-run lists `feature.new_checkout`: {out:?}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_without_invoking_fastly() {
        let dir = tempdir().expect("tempdir");
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "status line names the no-op: {out:?}"
        );
    }

    // ---------- read_config_entry_local ----------

    #[test]
    fn read_local_returns_missing_store_when_fastly_toml_absent() {
        let dir = tempdir().expect("tempdir");
        // No fastly.toml written — file missing.
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing file is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "absent fastly.toml => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_store_when_no_local_server_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // fastly.toml exists but has no [local_server.config_stores.*] block.
        fs::write(&path, "name = \"demo\"\n[setup.config_stores.app_config]\n").expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing local_server block is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "no local_server stanza => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_key_when_key_absent_from_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Write a local_server block with a different key so the store exists
        // but the requested key is absent.
        fs::write(
            &path,
            format!(
                "name = \"demo\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}]\n\
                 format = \"inline-toml\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}.contents]\n\
                 other_key = \"other_value\"\n"
            ),
        )
        .expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing key is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "key absent from contents => MissingKey"
        );
    }

    #[test]
    fn read_local_returns_present_when_key_exists_in_contents() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write initial toml");

        // Use a valid BlobEnvelope value — the resolver requires BlobEnvelope
        // or chunk-pointer JSON; raw strings are not accepted post-chunking.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), envelope_json.clone())],
        )
        .expect("setup write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("key present");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, envelope_json, "value matches what was written");
    }

    #[test]
    fn read_local_roundtrips_with_push_local() {
        // Write via push_config_entries_local, then read via
        // read_config_entry_local — the two must agree on the value.
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");

        // push_config_entries_local passes the value through the chunk-pointer
        // helper which stores it verbatim when ≤ 8 000 chars. The reader then
        // resolves it through the same resolver that requires BlobEnvelope JSON.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "roundtrip"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entries = vec![("greeting".to_owned(), envelope_json.clone())];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("read succeeds");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present after push+read roundtrip");
        };
        assert_eq!(value, envelope_json, "roundtrip value matches");
    }

    #[test]
    fn read_local_requires_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let result = FastlyCliAdapter.read_config_entry_local(
            dir.path(),
            None, // adapter_manifest_path missing
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("[adapters.fastly.adapter].manifest"),
                "error names the missing field: {err}"
            ),
            Ok(_) => panic!("expected Err when adapter_manifest_path is None"),
        }
    }

    // ---------- read_config_entry (fake fastly, remote shell-out) ----------

    /// Build a tempdir containing a `fastly` shim script that:
    /// - Responds to `config-store list --json` with a store-list JSON containing
    ///   `TEST_CONFIG_ID` mapped to `store-abc123`.
    /// - Responds to `config-store-entry describe ...` with `stdout_body` on
    ///   stdout and `stderr_body` on stderr, exiting with `exit_code`.
    ///
    /// Payloads are written to separate sibling files so shell-active chars
    /// in the content don't get re-interpreted by the script.
    #[cfg(unix)]
    fn fake_fastly_returning(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        let stdout_file = dir.path().join("stdout_payload.txt");
        let stderr_file = dir.path().join("stderr_payload.txt");
        let list_file = dir.path().join("list_payload.txt");
        // Store-list JSON: bare array with one entry matching TEST_CONFIG_ID.
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        fs::write(&list_file, list_json).expect("write list payload");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\ncat '{}' >&2\nexit {exit_code}\n",
            list_file.display(),
            stdout_file.display(),
            stderr_file.display(),
        );
        fs::write(&script_path, script).expect("write fastly script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Build a fake `fastly` that logs each argv token (one per line) to
    /// `out_path`, handles the list call correctly, and exits 0 for both calls.
    #[cfg(unix)]
    fn fake_fastly_argv_log(out_path: &Path) -> tempfile::TempDir {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        let list_file = dir.path().join("list_payload.txt");
        let entry_file = dir.path().join("entry_payload.txt");
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        // item_value must be a valid BlobEnvelope JSON so the resolver accepts it.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"v": "logged"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entry_json = format!(
            r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
            serde_json::to_string(&envelope_json).expect("escape")
        );
        fs::write(&list_file, list_json).expect("write list payload");
        fs::write(&entry_file, &entry_json).expect("write entry payload");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\nexit 0\n",
            out_path.display(),
            list_file.display(),
            entry_file.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Process-wide mutex serialising PATH-mutating tests so parallel
    /// test threads don't race on the environment variable.
    #[cfg(unix)]
    fn path_mutation_guard() -> &'static Mutex<()> {
        use std::sync::OnceLock;
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_present_on_success() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Fake fastly: list succeeds with app_config → store-abc123;
        // describe returns valid JSON with item_value that is a BlobEnvelope.
        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entry_json = format!(
            r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
            serde_json::to_string(&envelope).expect("escape")
        );
        let fake = fake_fastly_returning(&entry_json, "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("fake fastly exit-0 must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, envelope);
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_not_found_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // describe exits non-zero with "not found" in stderr → MissingKey.
        let fake = fake_fastly_returning("", "Error: item not found", 1);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("not-found maps to MissingKey (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "not-found stderr => MissingKey"
        );
    }

    /// The Fastly impl distinguishes store-not-found from key-not-found via
    /// `resolve_remote_config_store_id`: when the list call exits non-zero and
    /// the error string contains "not found", `read_config_entry` returns
    /// `MissingStore` without ever calling the describe subcommand.
    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_on_appropriate_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Script that exits non-zero for the list call so resolve fails with
        // a "not found" error, causing read_config_entry to return MissingStore.
        let fake_dir = tempdir().expect("tempdir");
        let stderr_file = fake_dir.path().join("stderr_payload.txt");
        fs::write(&stderr_file, "Error: config store not found for service").expect("write stderr");
        let script_path = fake_dir.path().join("fastly");
        let script = format!(
            "#!/bin/sh\ncat '{stderr}' >&2\nexit 1\n",
            stderr = stderr_file.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        let _path = PathPrepend::new(fake_dir.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("list failure with not-found maps to MissingStore (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "list not-found => MissingStore"
        );
    }

    /// Verify that `read_config_entry` invokes
    /// `fastly config-store-entry describe --store-id=<id> --key=<key> --json`
    /// (after the resolve step that calls `fastly config-store list --json`).
    #[cfg(unix)]
    #[test]
    fn read_remote_invokes_correct_argv() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("argv-log fake must succeed");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "expected Present from argv-log fake"
        );
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // The describe call must include these args (resolve call args
        // are also captured but we only assert the describe shape here).
        assert!(
            captured.contains("config-store-entry"),
            "must invoke config-store-entry; got:\n{captured}"
        );
        assert!(
            captured.contains("describe"),
            "must pass describe subcommand; got:\n{captured}"
        );
        assert!(
            captured.contains("--store-id=store-abc123"),
            "must pass resolved store id; got:\n{captured}"
        );
        assert!(
            captured.contains("--key=greeting"),
            "must pass --key=<key>; got:\n{captured}"
        );
        assert!(
            captured.contains("--json"),
            "must pass --json flag; got:\n{captured}"
        );
    }

    // ---------- chunked push integration tests ----------

    /// Build a valid `BlobEnvelope` JSON string of approximately `target_len` bytes.
    #[cfg(unix)]
    fn make_test_envelope(target_len: usize) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let pad = "x".repeat(target_len.saturating_add(64));
        let data = json!({ "pad": pad });
        let raw =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
        if raw.len() >= target_len {
            let overhead = raw.len().saturating_sub(pad.len());
            let adjusted = "x".repeat(target_len.saturating_sub(overhead));
            let data2 = json!({ "pad": adjusted });
            serde_json::to_string(&BlobEnvelope::new(data2, "2026-06-22T00:00:00Z".into())).unwrap()
        } else {
            raw
        }
    }

    /// Build a fake `fastly` script whose describe response depends on
    /// the `--key=<k>` argument: `key_responses` maps key names to JSON
    /// item-value responses. Falls back to exit 1 "not found" for unknown keys.
    #[cfg(unix)]
    fn fake_fastly_with_key_dispatch(
        _dir: &Path,
        key_responses: &[(String, String)],
    ) -> tempfile::TempDir {
        use std::fmt::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let fake_dir = tempdir().expect("tempdir");
        let list_file = fake_dir.path().join("list.json");
        let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
        fs::write(&list_file, list_json).expect("write list");
        // Write each key response to a named file.
        let mut dispatch_lines = String::new();
        for (key, response) in key_responses {
            let resp_file = fake_dir.path().join(format!("resp_{key}.json"));
            fs::write(&resp_file, response).expect("write resp");
            // Use exact-match: iterate argv and compare each token literally
            // so that a root key like "app_config" does NOT match a chunk key
            // like "app_config.__edgezero_chunks.abc.0".
            writeln!(
                dispatch_lines,
                "  for arg in \"$@\"; do if [ \"$arg\" = \"--key={key}\" ]; then cat '{}'; exit 0; fi; done",
                resp_file.display()
            )
            .expect("write to String is infallible");
        }
        // Fallback outputs "not found" so fetch_remote_config_store_entry
        // maps it to Ok(None) rather than Err.
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\n{dispatch_lines}echo 'Error: item not found' >&2\nexit 1\n",
            list_file.display()
        );
        let script_path = fake_dir.path().join("fastly");
        fs::write(&script_path, &script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        fake_dir
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_direct_entry_at_exactly_8000_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        assert_eq!(envelope.len(), FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        // One physical entry written (direct).
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "must write root key directly: {captured}"
        );
        assert!(
            out[0].contains("1 physical entries (1 logical)"),
            "summary reports 1 physical entry: {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_chunks_and_root_pointer_for_8001_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        assert!(envelope.len() > FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // At least one chunk key must appear before the root key.
        assert!(
            captured.contains(".__edgezero_chunks."),
            "chunk keys must be written: {captured}"
        );
        // Root pointer must also be written.
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "root pointer must be written: {captured}"
        );
        // Root key must be LAST in the log (chunk lines come before it).
        let root_pos = captured.rfind(&format!("--key={TEST_CONFIG_ID}")).unwrap();
        let chunk_pos = captured.find(".__edgezero_chunks.").unwrap();
        assert!(
            chunk_pos < root_pos,
            "chunk writes must precede root pointer write: chunk_pos={chunk_pos} root_pos={root_pos}"
        );
        assert!(out[0].contains("logical"), "summary line present: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_dry_run_reports_direct_vs_chunked() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");

        let direct_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let chunked_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));

        let entries = vec![
            ("cfg_direct".to_owned(), direct_envelope),
            ("cfg_chunked".to_owned(), chunked_envelope),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not error");

        // No shellout happens; output must describe intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would push `cfg_direct` as direct entry"),
            "must report direct: {combined}"
        );
        assert!(
            combined.contains("would push `cfg_chunked` as chunked"),
            "must report chunked: {combined}"
        );
    }

    /// Spec 12.7: pushing two blobs under different root keys
    /// (e.g. `app_config` + `app_config_staging`) must leave both
    /// keys readable from the local fastly.toml so the runtime
    /// `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY` override can
    /// switch between them. Prior to the upsert fix the second
    /// push wholesale-replaced the per-store contents table.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_preserves_sibling_keys() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");
        let store = ResolvedStoreId::from_logical(TEST_CONFIG_ID);
        let ctx = AdapterPushContext::new();

        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &store,
                &[("app_config".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &ctx,
                false,
            )
            .expect("first push");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
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

        let raw = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = raw.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents after sibling push");
        let app_config = contents
            .get("app_config")
            .and_then(toml_edit::Item::as_str)
            .expect("default key must survive sibling push");
        assert_eq!(
            app_config, "{\"envelope\":\"A\"}",
            "default key value: {raw}"
        );
        let staging = contents
            .get("app_config_staging")
            .and_then(toml_edit::Item::as_str)
            .expect("staging key must be present");
        assert_eq!(staging, "{\"envelope\":\"B\"}", "staging key value: {raw}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_writes_literal_dotted_chunk_keys() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("local push must succeed");

        let after = fs::read_to_string(&fastly_toml).expect("read back");
        // Chunk keys contain '.' and must appear as quoted string keys,
        // not as TOML nested tables (which would look like [table.sub]).
        assert!(
            after.contains(".__edgezero_chunks."),
            "chunk keys written to fastly.toml: {after}"
        );
        // Parse with toml_edit and confirm chunk keys are string-keyed entries.
        let doc: toml_edit::DocumentMut = after.parse().expect("must parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .expect("contents table must exist");
        // At least one chunk key must be present as a string value (not a table).
        let has_chunk_string = contents.as_table().is_some_and(|tbl| {
            tbl.iter()
                .any(|(key, val)| key.contains(".__edgezero_chunks.") && val.as_value().is_some())
        });
        assert!(
            has_chunk_string,
            "chunk keys must be literal string-valued entries, not nested tables: {after}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_reports_chunking_and_does_not_edit_fastly_toml() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let original = "name = \"demo\"\n";
        fs::write(&fastly_toml, original).expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("local dry-run must not error");

        // File must be untouched.
        let after = fs::read_to_string(&fastly_toml).expect("read back");
        assert_eq!(after, original, "dry-run must not edit fastly.toml");

        // Output must describe chunking intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would set") && combined.contains("chunked"),
            "must report chunked intent: {combined}"
        );
    }

    // ---------- chunked read integration tests ----------

    #[cfg(unix)]
    #[test]
    fn read_config_entry_resolves_direct_value_unchanged() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = BlobEnvelope::new(json!({"hello": "world"}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&json_str).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Build a key→response map for every physical entry.
        let mut key_responses: Vec<(String, String)> = Vec::new();
        for (pk, pv) in &physical {
            let resp = format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap());
            key_responses.push((pk.clone(), resp));
        }
        // The root key should return the pointer.
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        key_responses.push((TEST_CONFIG_ID.to_owned(), ptr_resp));

        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_missing_chunk() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Only provide the root pointer; omit chunk responses so chunk fetch returns not-found.
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        let key_responses = vec![(TEST_CONFIG_ID.to_owned(), ptr_resp)];
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("missing chunk must error")
        };
        assert!(
            err.contains("missing chunk"),
            "error must mention missing chunk: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_corrupt_chunk_hash() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        let mut key_responses: Vec<(String, String)> = Vec::new();
        // Corrupt first chunk's content.
        let (first_chunk_key, first_chunk_val) = &physical[0];
        let corrupted: String = first_chunk_val.chars().map(|_| 'Z').collect();
        let corrupt_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&corrupted).unwrap()
        );
        key_responses.push((first_chunk_key.clone(), corrupt_resp));
        // Remaining chunks as normal.
        for (pk, pv) in physical
            .iter()
            .take(physical.len().saturating_sub(1))
            .skip(1)
        {
            key_responses.push((
                pk.clone(),
                format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap()),
            ));
        }
        key_responses.push((
            TEST_CONFIG_ID.to_owned(),
            format!(
                r#"{{"item_value":{}}}"#,
                serde_json::to_string(pointer_json).unwrap()
            ),
        ));
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("corrupt chunk must error")
        };
        assert!(
            err.contains("SHA mismatch") || err.contains("mismatch"),
            "error must mention hash mismatch: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_errors_on_malformed_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Root value is JSON but neither a BlobEnvelope nor a valid pointer.
        let bad_json = r#"{"some_field":"not a pointer or envelope"}"#;
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(bad_json).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("malformed pointer must error")
        };
        assert!(
            err.contains("neither a valid BlobEnvelope") || err.contains("chunk pointer"),
            "error must describe parse failure: {err}"
        );
    }

    // ---------- local read integration tests ----------

    #[test]
    fn read_config_entry_local_resolves_direct_value() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = BlobEnvelope::new(json!({"x": 1_i32}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        // Write directly as a single entry (not via push_config_entries_local so we
        // control the exact TOML content).
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), json_str.clone())],
        )
        .expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("local read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[test]
    fn read_config_entry_local_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        // Write all physical entries (chunks + pointer) to the local store.
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &physical).expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    /// Spec 12.3 + 9.3: a second oversized push must converge the
    /// runtime on the NEW envelope — chunk keys are content-addressed
    /// by the full-envelope SHA, so push B writes a new chunk-set and
    /// installs a new root pointer.
    ///
    /// The local fastly.toml writer upserts per-key (so a sibling
    /// `--key app_config_staging` push leaves `app_config` intact per
    /// spec 12.7). Within the SAME root key, old chunks for envelope
    /// A remain in the contents table after envelope B's push — they're
    /// unreferenced (the root pointer at `app_config` now names B's
    /// chunks), matching the remote Fastly behaviour where the
    /// per-entry `update --upsert` shell-out has no atomic-delete
    /// pairing. The runtime-correctness property holds either way: a
    /// read after push B follows the active pointer and reconstructs
    /// envelope B, not A.
    #[cfg(unix)]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "linear test scenario: push A, inspect, push B, inspect, read; splitting would obscure the chunk-set comparison"
    )]
    fn second_oversized_push_converges_runtime_on_new_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // First push: envelope A. Records the chunk-key set so we can
        // confirm they survive the second push (no garbage collection
        // in v1 — spec 9.3 + Q6).
        let envelope_a = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_a.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push must succeed");

        let after_a = fs::read_to_string(&fastly_toml).expect("read");
        let doc_a: toml_edit::DocumentMut = after_a.parse().expect("parse");
        let contents_a = doc_a
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push A");
        let chunks_a: Vec<String> = contents_a
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_a.is_empty(),
            "push A must have produced chunk entries: {after_a}"
        );

        // Second push: a DIFFERENT oversized envelope B. The
        // content-addressed chunk keys must shift to B's sha; old
        // A-chunks may remain in the table (v1 doesn't GC). Build
        // envelope B with a distinct payload key so its SHA differs
        // from A's even at the same total length.
        let envelope_b = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ "alt": "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:01Z".to_owned()))
                .expect("envelope B serialises")
        };
        assert_ne!(envelope_a, envelope_b, "test fixtures must differ");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("second push must succeed");

        let after_b = fs::read_to_string(&fastly_toml).expect("read");
        let doc_b: toml_edit::DocumentMut = after_b.parse().expect("parse");
        let contents_b = doc_b
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push B");
        let chunks_b: Vec<String> = contents_b
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_b.is_empty(),
            "push B must have produced chunk entries: {after_b}"
        );

        // Chunk keys are content-addressed by envelope SHA, so the B
        // push installs a fresh chunk-set whose keys are all distinct
        // from A's. Under the upsert semantic the A-chunks remain in
        // the contents table (no GC in v1); B's chunks are simply added.
        let new_b_chunks: Vec<&String> = chunks_b
            .iter()
            .filter(|key| !chunks_a.contains(*key))
            .collect();
        assert!(
            !new_b_chunks.is_empty(),
            "push B must have added at least one new content-addressed chunk: A-set={chunks_a:?} B-set={chunks_b:?}"
        );
        // Old A-chunks remain in the table (orphan-but-present —
        // matches the remote Fastly write-only-upsert semantic).
        for chunk_key in &chunks_a {
            assert!(
                chunks_b.contains(chunk_key),
                "old A-chunk `{chunk_key}` must remain in the local table after push B (v1 has no GC); B-set={chunks_b:?}"
            );
        }

        // Runtime-correctness property: a fresh read after push B
        // reconstructs envelope B (NOT envelope A).
        let read = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local read after push B");
        let ReadConfigEntry::Present(value) = read else {
            panic!("expected Present after push B");
        };
        assert_eq!(
            value, envelope_b,
            "read after second push must reconstruct envelope B, not A"
        );
        assert_ne!(
            value, envelope_a,
            "old envelope A's chunks must be inert -- read must NOT return A"
        );
    }
}
