use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    register_adapter, Adapter, AdapterAction, AdapterDeployedState, AdapterPushContext,
    ProvisionMode, ProvisionOutcome, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use walkdir::WalkDir;

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
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_lib_rs",
        contents: include_str!("templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_wrangler_toml",
        contents: include_str!("templates/wrangler.toml.hbs"),
    },
];

const TARGET_TRIPLE: &str = "wasm32-unknown-unknown";

const WRANGLER_INSTALL_HINT: &str =
    "install the Cloudflare CLI (`npm install -g wrangler`) and try again";

struct CloudflareCliAdapter;

#[expect(
    clippy::missing_trait_methods,
    reason = "cloudflare has no validate_app_config_keys / validate_adapter_manifest / validate_typed_secrets requirements; those three trait defaults are intentionally inherited. `read_config_entry` and `read_config_entry_local` are both overridden below (wrangler kv key get --remote / --local). `single_store_kinds` IS overridden below (returns `&[\"secrets\"]`)."
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
            AdapterAction::Build => build(args).map(|artifact| {
                log::info!(
                    "[edgezero] Cloudflare build artifact -> {}",
                    artifact.display()
                );
            }),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
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
        _deployed: Option<&AdapterDeployedState>,
        mode: ProvisionMode,
        dry_run: bool,
    ) -> Result<ProvisionOutcome, String> {
        match mode {
            ProvisionMode::Cloud => {}
            ProvisionMode::Local => return Err("local mode lands in Section 5".to_owned()),
        }
        //: KV ids and config ids both back to Cloudflare KV
        // namespaces. Secrets are runtime-managed via
        // `wrangler secret put` — provision is a no-op for them.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for provision"
                    .to_owned(),
            );
        };
        let wrangler_path = manifest_root.join(rel);

        let mut out = Vec::new();
        for store in stores.kv.iter().chain(stores.config.iter()) {
            let logical = &store.logical;
            // The Cloudflare KV binding name is what the runtime
            // calls `env.kv(...)` with -- it's resolved at request
            // time from `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`
            // (default = logical id). Provision must write the
            // resolved PLATFORM name into wrangler.toml, otherwise
            // the runtime will look up a binding the CLI never
            // created.
            let binding = &store.platform;
            // Idempotency check BEFORE shelling out: if a
            // [[kv_namespaces]] entry with `binding = <platform>`
            // is already present and has a real namespace id, skip.
            // Without this guard a re-run of provision would invoke
            // `wrangler kv namespace create` again and orphan the
            // previously-created namespace -- wasting account quota.
            // A placeholder id (anything that isn't a 32-char
            // lowercase hex string, like the
            // `local-dev-placeholder` the scaffold wrangler.toml
            // writes) is treated as "not yet provisioned" so the
            // entry gets rewritten with the real id.
            //
            // We deliberately do NOT cross-check the stored id
            // against Cloudflare's API (e.g. by calling `wrangler
            // kv namespace list` to confirm the id still exists).
            // Verifying every entry on every provision run would
            // add a network round-trip per id and require parsing
            // yet another wrangler subcommand output. The skip
            // line names the existing id explicitly so the operator
            // can verify it themselves and, if the Cloudflare-side
            // namespace was deleted out-of-band, remove the stale
            // entry by hand before re-running provision.
            let existing = existing_real_namespace_id(&wrangler_path, binding)?;
            if let Some(existing_id) = existing {
                out.push(format!(
                    "binding `{binding}` (logical id `{logical}`) already provisioned (id={existing_id} in {}); skipping. To force a fresh namespace: delete the [[kv_namespaces]] entry for binding `{binding}` AND run `wrangler kv namespace delete --namespace-id={existing_id}` (the old remote namespace lingers otherwise), then re-run provision.",
                    wrangler_path.display()
                ));
                continue;
            }
            // Pre-flight the writeback shape BEFORE shelling
            // `wrangler kv namespace create`. `read_namespace_id`
            // tolerates both `[[kv_namespaces]]` (array-of-tables)
            // and `kv_namespaces = [{ binding = "...", id = "..." }]`
            // (inline-array) forms, but `upsert_kv_namespace` only
            // writes back through the array-of-tables shape. Without
            // this guard, an inline-array manifest passes the
            // "already provisioned?" probe (because no id is
            // present), the remote `create` succeeds, and then the
            // upsert errors out — leaving the freshly-created
            // namespace orphaned on Cloudflare with no local
            // writeback to track it.
            //
            // Refuse early so the operator fixes the manifest shape
            // BEFORE any account-side mutation.
            check_kv_namespaces_writeback_shape(&wrangler_path)?;
            if dry_run {
                out.push(format!(
                    "would run `wrangler kv namespace create {binding}` and append [[kv_namespaces]] binding = \"{binding}\" to {} (logical id `{logical}`)",
                    wrangler_path.display()
                ));
                continue;
            }
            let namespace_id = create_kv_namespace(binding)?;
            upsert_kv_namespace(&wrangler_path, binding, &namespace_id)?;
            out.push(format!(
                "created KV namespace `{binding}` (logical id `{logical}`, namespace id={namespace_id}); written to {}",
                wrangler_path.display()
            ));
        }
        for store in stores.secrets {
            let logical = &store.logical;
            let platform = &store.platform;
            out.push(format!(
                "cloudflare secret `{platform}` (logical id `{logical}`) is runtime-managed via `wrangler secret put`; nothing to provision"
            ));
        }
        if out.is_empty() {
            out.push("cloudflare has no declared stores to provision".to_owned());
        }
        Ok(ProvisionOutcome {
            status_lines: out,
            deployed: None,
        })
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
        // Read namespace id from wrangler.toml (matched by
        // `binding = <platform>`), then `wrangler kv bulk put
        // <tempfile.json> --namespace-id=<id> --remote`. The
        // CLI hands this writer one logical (root_key, envelope_json)
        // entry; the bulk-put still works because it's one upsert
        // per entry, and the one-entry case is degenerate.
        //
        // **--remote** is mandatory for the prod-push path:
        // wrangler v4 defaults KV bulk-put to LOCAL storage when
        // the command supports both — meaning a v4 user running
        // `wrangler kv bulk put` without `--remote` would silently
        // populate Miniflare state under `.wrangler/state` and
        // report success while leaving the live Cloudflare
        // namespace empty. Explicit `--remote` removes the
        // ambiguity.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config push"
                    .to_owned(),
            );
        };
        let wrangler_path = manifest_root.join(rel);
        let binding = store.platform.as_str();
        let logical = store.logical.as_str();
        // Dry-run is lenient about a missing/unresolved binding so
        // operators can preview the keyset BEFORE running provision.
        // Real runs still err loudly so we don't silently push to
        // a non-existent namespace.
        if dry_run {
            let header = find_namespace_id(&wrangler_path, binding).map_or_else(
                |_| format!(
                    "would run `wrangler kv bulk put <tempfile.json> --namespace-id=<unresolved> --remote` with {} entries for binding `{binding}` (logical id `{logical}`, binding not yet provisioned -- run `edgezero provision --adapter cloudflare` to resolve the namespace id)",
                    entries.len()
                ),
                |ns_id| format!(
                    "would run `wrangler kv bulk put <tempfile.json> --namespace-id={ns_id} --remote` with {} entries for binding `{binding}` (logical id `{logical}`)",
                    entries.len()
                ),
            );
            let mut out = vec![header];
            for (key, _) in entries {
                out.push(format!("  would create entry `{key}`"));
            }
            return Ok(out);
        }
        let namespace_id = find_namespace_id(&wrangler_path, binding)?;
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to KV namespace `{binding}` (logical id `{logical}`, id={namespace_id})"
            )]);
        }
        let payload = bulk_payload(entries)?;
        let temp = tempfile::Builder::new()
            .prefix("edgezero-cf-push-")
            .suffix(".json")
            .tempfile()
            .map_err(|err| {
                format!("failed to create temp file for wrangler bulk payload: {err}")
            })?;
        fs::write(temp.path(), payload.as_bytes())
            .map_err(|err| format!("failed to write {}: {err}", temp.path().display()))?;
        let temp_arg = temp
            .path()
            .to_str()
            .ok_or_else(|| format!("temp file path {} is not UTF-8", temp.path().display()))?;
        let namespace_arg = format!("--namespace-id={namespace_id}");
        // Run from the wrangler.toml's directory so wrangler picks
        // up its `account_id` / `--env` resolution + persistence
        // settings the same way `wrangler dev` / `wrangler deploy`
        // do for this project.
        let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
        let output = Command::new("wrangler")
            .current_dir(project_dir)
            .args([
                "kv",
                "bulk",
                "put",
                temp_arg,
                namespace_arg.as_str(),
                "--remote",
            ])
            .output()
            .map_err(|err| {
                if err.kind() == ErrorKind::NotFound {
                    format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
                } else {
                    format!("failed to spawn `wrangler`: {err}")
                }
            })?;
        if !output.status.success() {
            return Err(format!(
                "`wrangler kv bulk put --remote` exited with status {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(vec![format!(
            "pushed {} entries to KV namespace `{binding}` (logical id `{logical}`, id={namespace_id})",
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
        // Local push: address the binding directly via
        // `wrangler kv bulk put <file> --binding <BINDING> --local`.
        // Crucially we do NOT resolve a namespace id here — the
        // scaffold ships with `local-dev-placeholder` ids, so an
        // operator that hasn't run `edgezero provision` yet should
        // still be able to seed `.wrangler/state` from the manifest
        // (matching wrangler's own local KV docs). Wrangler stores
        // local entries keyed by binding, not namespace id, so the
        // follow-up `wrangler dev --local` / `edgezero serve
        // --adapter cloudflare` reads them back through the same
        // binding name.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config push --local"
                    .to_owned(),
            );
        };
        let wrangler_path = manifest_root.join(rel);
        let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
        let binding = store.platform.as_str();
        let logical = store.logical.as_str();
        if dry_run {
            let mut out = vec![format!(
                "would run `wrangler kv bulk put <tempfile.json> --binding {binding} --local` with {} entries for binding `{binding}` (logical id `{logical}`)",
                entries.len()
            )];
            for (key, _) in entries {
                out.push(format!("  would create local entry `{key}`"));
            }
            return Ok(out);
        }
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to local KV namespace `{binding}` (logical id `{logical}`)"
            )]);
        }
        let payload = bulk_payload(entries)?;
        let temp = tempfile::Builder::new()
            .prefix("edgezero-cf-push-local-")
            .suffix(".json")
            .tempfile()
            .map_err(|err| {
                format!("failed to create temp file for wrangler bulk payload: {err}")
            })?;
        fs::write(temp.path(), payload.as_bytes())
            .map_err(|err| format!("failed to write {}: {err}", temp.path().display()))?;
        let temp_arg = temp
            .path()
            .to_str()
            .ok_or_else(|| format!("temp file path {} is not UTF-8", temp.path().display()))?;
        let output = Command::new("wrangler")
            .current_dir(project_dir)
            .args([
                "kv",
                "bulk",
                "put",
                temp_arg,
                "--binding",
                binding,
                "--local",
            ])
            .output()
            .map_err(|err| {
                if err.kind() == ErrorKind::NotFound {
                    format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
                } else {
                    format!("failed to spawn `wrangler`: {err}")
                }
            })?;
        if !output.status.success() {
            return Err(format!(
                "`wrangler kv bulk put --binding {binding} --local` exited with status {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(vec![format!(
            "pushed {} entries to local KV namespace bound as `{binding}` (logical id `{logical}`); `.wrangler/state` updated",
            entries.len()
        )])
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
        read_wrangler_kv_key(manifest_root, adapter_manifest_path, store, key, "--remote")
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
        read_wrangler_kv_key(manifest_root, adapter_manifest_path, store, key, "--local")
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: cloudflare is Multi for KV (KV namespaces) and
        // Config (KV namespaces), Single for Secrets (Worker
        // Secrets is a single flat bag).
        &["secrets"]
    }
}

/// Shell out to `wrangler kv namespace create <binding>`, capture
/// stdout, and parse the resulting namespace id. The CLI's
/// `provision` command resolves this against the user's
/// `wrangler.toml` and writes the `[[kv_namespaces]]` entry.
///
/// # Errors
/// Returns an error if `wrangler` isn't on `PATH`, the child fails
/// to spawn, the exit status is non-zero, or stdout doesn't
/// include a parseable `id = "..."` line.
fn create_kv_namespace(binding: &str) -> Result<String, String> {
    let output = Command::new("wrangler")
        .args(["kv", "namespace", "create", binding])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
            } else {
                format!("failed to spawn `wrangler`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`wrangler kv namespace create {binding}` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    extract_namespace_id(&stdout).ok_or_else(|| {
        format!(
            "wrangler created `{binding}` but stdout did not include a parseable `id = \"...\"` line -- wrangler may have changed its output format; pin a known-compatible wrangler version or file an issue. Raw stdout:\n{stdout}"
        )
    })
}

/// Pull the namespace id out of `wrangler kv namespace create`
/// stdout. Wrangler 3+ prints (something like):
///
/// ```text
/// 🌀 Creating namespace with title "..."
/// ✨ Success!
/// Add the following to your configuration file in your kv_namespaces array:
/// [[kv_namespaces]]
/// binding = "my-kv"
/// id = "abc123..."
/// ```
///
/// We tolerate leading whitespace + surrounding decoration. To
/// avoid grabbing a stray informational line like
/// `id = "<workspace_id>"` printed somewhere else in wrangler
/// output (or a hypothetical future `id = ...` line that names a
/// non-KV resource), we anchor to the `[[kv_namespaces]]` table
/// header AND require the value to be 32-char lowercase hex
/// (Cloudflare's actual namespace-id shape). The scan walks
/// lines top-down: when we see `[[kv_namespaces]]` we set a
/// scope flag; the next `id = "<32-char-hex>"` line within that
/// scope is the result. A new top-level header resets the scope.
fn extract_namespace_id(stdout: &str) -> Option<String> {
    let mut in_kv_namespaces = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "[[kv_namespaces]]" {
            in_kv_namespaces = true;
            continue;
        }
        // Any other table header ends the scope so we don't reach
        // forward into a sibling block.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_kv_namespaces = false;
            continue;
        }
        if !in_kv_namespaces {
            continue;
        }
        let Some(after_id_kw) = trimmed.strip_prefix("id") else {
            continue;
        };
        let Some(after_eq) = after_id_kw.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(quoted) = after_eq.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some((id, _)) = quoted.split_once('"') else {
            continue;
        };
        if is_real_namespace_id(id) {
            return Some(id.to_owned());
        }
    }
    None
}

/// Heuristic: is `id` a real Cloudflare KV namespace id (32-char
/// lowercase hex), as opposed to a scaffold placeholder like
/// `local-dev-placeholder`? Cloudflare's API consistently returns
/// 32-char lowercase hex, so we use that as a tight cheap signal.
///
/// Additionally rejects hex-shape sentinels that LOOK like real
/// ids but are obviously hand-typed placeholders: anything with
/// fewer than 6 distinct hex characters (catches all-zeros,
/// all-`a`, `deadbeefdeadbeefdeadbeefdeadbeef`, etc.). A real id
/// generated by Cloudflare's API has effectively uniform random
/// hex distribution: expected distinct chars over 32 draws from
/// 16 symbols is ~14, and the dominant term P(=5 distinct) is on
/// the order of 10^-13 -- so false rejections of real ids are
/// astronomically unlikely.
fn is_real_namespace_id(id: &str) -> bool {
    if id.len() != 32 {
        return false;
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    // Distinct-byte count via a BTreeSet: 32 inserts is trivial,
    // and the set form avoids the arithmetic-side-effect /
    // silent-as / indexing-panic shapes the project's clippy
    // profile rejects.
    let distinct: BTreeSet<u8> = id.bytes().collect();
    distinct.len() >= 6
}

/// If `path` already declares a `[[kv_namespaces]]` entry with
/// `binding = binding` AND its `id` looks like a real Cloudflare
/// namespace id, return that id. Returns `Ok(None)` if the binding
/// is absent OR present with a placeholder id (so provision can
/// treat both cases as "needs (re-)create"). A failure to read /
/// parse the file is a hard error -- provision needs an authoritative
/// answer.
fn existing_real_namespace_id(path: &Path, binding: &str) -> Result<Option<String>, String> {
    let Some(existing) = read_namespace_id(path, binding)? else {
        return Ok(None);
    };
    if is_real_namespace_id(&existing) {
        Ok(Some(existing))
    } else {
        Ok(None)
    }
}

/// Internal: look up `binding`'s `id` in `wrangler.toml` without
/// the "did you run provision?" error path that `find_namespace_id`
/// adds. Missing file -> `Ok(None)`. Returns the raw id whether or
/// not it looks like a real Cloudflare id.
///
/// Errors loudly if `kv_namespaces` exists but is neither an
/// array-of-tables nor an inline-array (e.g. the operator typed
/// `kv_namespaces = "oops"`). Silently returning `None` there
/// surfaces downstream as "did you run provision?" -- misleading,
/// because the actual problem is a malformed manifest.
fn read_namespace_id(path: &Path, binding: &str) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let id = match doc.get("kv_namespaces") {
        Some(Item::ArrayOfTables(arr)) => arr.iter().find_map(|table| {
            if table.get("binding").and_then(Item::as_str) == Some(binding) {
                table.get("id").and_then(Item::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(Item::Value(Value::Array(arr))) => arr.iter().find_map(|item| {
            let table = item.as_inline_table()?;
            if table.get("binding").and_then(Value::as_str) == Some(binding) {
                table.get("id").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(other) => {
            return Err(format!(
                "{}: `kv_namespaces` exists but is neither `[[kv_namespaces]]` (array-of-tables) nor an inline array of `{{ binding, id }}` records; got TOML item of type `{}`",
                path.display(),
                item_kind(other)
            ));
        }
        None => None,
    };
    Ok(id)
}

/// Refuse to provision a new namespace when `wrangler.toml`'s
/// `kv_namespaces` exists in a form that `upsert_kv_namespace`
/// can't write back to. Today that means the inline-array form
/// (`kv_namespaces = [{ binding = "...", id = "..." }]`), which
/// `read_namespace_id` tolerates but `upsert_kv_namespace`'s
/// `as_array_of_tables_mut()` rejects. Without this guard, the
/// orphan-namespace hazard documented in `upsert_kv_namespace`
/// reappears: `wrangler kv namespace create` succeeds, then
/// upsert errors out and the new namespace lingers on
/// Cloudflare with no local writeback to track it. Missing or
/// array-of-tables forms are OK.
fn check_kv_namespaces_writeback_shape(path: &Path) -> Result<(), String> {
    use toml_edit::{DocumentMut, Item, Value};

    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    match doc.get("kv_namespaces") {
        None | Some(Item::ArrayOfTables(_)) => Ok(()),
        Some(Item::Value(Value::Array(_))) => Err(format!(
            "{}: `kv_namespaces` is declared as an inline array (`kv_namespaces = [{{ binding = \"...\", id = \"...\" }}]`); provision can only write back through the `[[kv_namespaces]]` array-of-tables form. Convert each entry to a `[[kv_namespaces]]` block BEFORE re-running provision; otherwise a successful `wrangler kv namespace create` would leave the new namespace orphaned on Cloudflare with no local entry to track it.",
            path.display()
        )),
        Some(other) => Err(format!(
            "{}: `kv_namespaces` exists but is neither `[[kv_namespaces]]` (array-of-tables) nor an inline array of `{{ binding, id }}` records; got TOML item of type `{}`. Convert it manually before re-running provision.",
            path.display(),
            item_kind(other)
        )),
    }
}

/// One-line label for a `toml_edit::Item` (for diagnostic
/// messages -- not a canonical TOML type description).
fn item_kind(item: &toml_edit::Item) -> &'static str {
    use toml_edit::{Item, Value};
    match item {
        Item::None => "none",
        Item::Value(Value::String(_)) => "string",
        Item::Value(Value::Integer(_)) => "integer",
        Item::Value(Value::Float(_)) => "float",
        Item::Value(Value::Boolean(_)) => "boolean",
        Item::Value(Value::Datetime(_)) => "datetime",
        Item::Value(Value::Array(_)) => "array",
        Item::Value(Value::InlineTable(_)) => "inline-table",
        Item::Table(_) => "table",
        Item::ArrayOfTables(_) => "array-of-tables",
    }
}

/// Insert OR update the `[[kv_namespaces]]` entry for `binding`,
/// rewriting `id` if the binding already exists (e.g. provision
/// is replacing a `local-dev-placeholder`). Used by provision so
/// re-running on a scaffolded wrangler.toml replaces the placeholder
/// with the real id instead of silently skipping.
///
/// Caveat: `toml_edit::Table::insert` replaces the value's `Item`,
/// which drops any trailing inline comment that was attached to
/// the prior `id = "..."` line (e.g. `id = "old"  # delete me`).
/// Sibling fields under the same `[[kv_namespaces]]` table are
/// preserved verbatim -- only the `id` line's decor is lost.
///
/// Concurrency: provision is NOT safe to run concurrently against
/// the same `wrangler.toml`. Two concurrent runs may both miss the
/// idempotency check, both call `wrangler kv namespace create`
/// remotely, then race the file write -- the loser's namespace
/// becomes an orphan in the Cloudflare account. `EdgeZero` does not
/// take a lockfile; operators must serialise provision themselves.
fn upsert_kv_namespace(path: &Path, binding: &str, id: &str) -> Result<(), String> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    // Treat NotFound as "start with empty document" symmetrically with
    // `read_namespace_id` so the orphan-namespace hazard goes away: if
    // wrangler.toml is missing entirely (e.g. operator deleted it
    // between scaffold and provision), the upsert that follows a
    // successful `wrangler kv namespace create` would otherwise error
    // out, leaving the remote namespace orphaned.
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    let entry = doc
        .entry("kv_namespaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let arr_of_tables = entry.as_array_of_tables_mut().ok_or_else(|| {
        format!(
            "{}: `kv_namespaces` exists but is not an array-of-tables (`[[kv_namespaces]]`); convert it manually before re-running provision",
            path.display()
        )
    })?;

    let existing_idx = arr_of_tables
        .iter()
        .position(|table| table.get("binding").and_then(Item::as_str) == Some(binding));
    if let Some(idx) = existing_idx {
        if let Some(existing) = arr_of_tables.get_mut(idx) {
            existing.insert("id", value(id));
        }
    } else {
        let mut new_table = Table::new();
        new_table.insert("binding", value(binding));
        new_table.insert("id", value(id));
        arr_of_tables.push(new_table);
    }

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

/// Render the entries as the `[{"key": "...", "value": "..."}, …]`
/// JSON wrangler expects for `kv bulk put`. Under the blob model the
/// CLI hands this writer one logical `(root_key, envelope_json)` entry;
/// Cloudflare passes the value through unchanged (the envelope is an
/// opaque string from the platform's perspective).
fn bulk_payload(entries: &[(String, String)]) -> Result<String, String> {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|(key, value)| serde_json::json!({ "key": key, "value": value }))
        .collect();
    serde_json::to_string(&payload)
        .map_err(|err| format!("failed to serialize wrangler bulk payload: {err}"))
}

/// Read a single key from a Cloudflare KV namespace by shelling out to
/// `wrangler kv key get --binding <BINDING> <KEY> <locality>`.
///
/// `locality` is either `"--remote"` (live Cloudflare KV) or `"--local"`
/// (Miniflare `.wrangler/state`). The two read methods on the adapter call
/// this shared helper with the appropriate flag.
///
/// # Mapping to `ReadConfigEntry`
/// - Success (exit 0) → `Present(stdout)`.
/// - Exit non-zero, stderr contains "not found" / "does not exist" → `MissingKey`.
/// - Exit non-zero, stderr mentions "binding" → `MissingStore` (the KV
///   namespace binding itself doesn't exist in `wrangler.toml`).
/// - Any other non-zero exit → `Err`.
fn read_wrangler_kv_key(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    locality: &str,
) -> Result<ReadConfigEntry, String> {
    let rel = adapter_manifest_path.ok_or_else(|| {
        "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config diff"
            .to_owned()
    })?;
    let wrangler_path = manifest_root.join(rel);
    let binding = store.platform.as_str();
    let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
    let output = Command::new("wrangler")
        .args(["kv", "key", "get", "--binding", binding, key, locality])
        .current_dir(project_dir)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
            } else {
                format!("failed to spawn `wrangler`: {err}")
            }
        })?;
    if output.status.success() {
        let body = String::from_utf8(output.stdout)
            .map_err(|err| format!("`wrangler kv key get` stdout is not UTF-8: {err}"))?;
        // Wrangler 4.x (verified 4.64.0) returns exit 0 + stdout
        // "Value not found" for a missing key instead of exit 1 +
        // stderr. Detect that shape and map to MissingKey -- a
        // missing key in the blob model is valid initial state
        // (first push hasn't run yet), not corrupt remote state.
        // Match the trimmed first line so trailing newlines or
        // future variants like "Value not found.\n" still match.
        let trimmed = body.trim();
        if trimmed.eq_ignore_ascii_case("value not found")
            || trimmed.eq_ignore_ascii_case("value not found.")
        {
            return Ok(ReadConfigEntry::MissingKey);
        }
        return Ok(ReadConfigEntry::Present(body));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not found") || stderr.contains("does not exist") {
        return Ok(ReadConfigEntry::MissingKey);
    }
    if stderr.contains("binding") || stderr.contains("Binding") {
        return Ok(ReadConfigEntry::MissingStore);
    }
    Err(format!(
        "`wrangler kv key get --binding {binding} {key} {locality}` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

/// # Errors
/// Returns an error if the Cloudflare wrangler build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
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
/// Returns an error if the Cloudflare wrangler deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["deploy", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler deploy failed with status {status}"));
    }

    Ok(())
}

/// Look up the namespace id wrangler.toml has bound to `binding`,
/// rejecting placeholder ids (anything that isn't a 32-char
/// lowercase hex Cloudflare API id).
///
/// Accepts both `[[kv_namespaces]]` (array-of-tables, what
/// `provision` writes and wrangler's own post-create hint prints)
/// and the inline-array form. Returns Err with a "did you run
/// provision?" hint if the binding is absent OR holds a placeholder
/// like `local-dev-placeholder` — without this check `push` would
/// shell out to `wrangler kv bulk put --namespace-id=<placeholder>`,
/// which fails at wrangler with a less actionable error.
fn find_namespace_id(wrangler_path: &Path, binding: &str) -> Result<String, String> {
    // read_namespace_id returns Ok(None) for both
    // missing-file AND binding-not-present; for `find_namespace_id`
    // the user wants a "did you run provision?" hint in both cases,
    // so collapse them into the same error message.
    let raw = read_namespace_id(wrangler_path, binding)?.ok_or_else(|| {
        format!(
            "{}: no [[kv_namespaces]] entry with binding = {binding:?} (did you run `edgezero provision --adapter cloudflare`?)",
            wrangler_path.display()
        )
    })?;
    if is_real_namespace_id(&raw) {
        Ok(raw)
    } else {
        Err(format!(
            "{}: binding {binding:?} has id {raw:?}, which doesn't look like a real Cloudflare KV namespace id (expected 32-char lowercase hex). This is usually a scaffold placeholder -- run `edgezero provision --adapter cloudflare` to create a real namespace and overwrite the entry.",
            wrangler_path.display()
        ))
    }
}

fn find_wrangler_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "wrangler.toml") {
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
            path.file_name().is_some_and(|n| n == "wrangler.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate wrangler.toml".to_owned());
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
        "compiled artifact not found for {crate_name} (looked in manifest and workspace target directories)"
    ))
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

/// # Errors
/// Returns an error if the Cloudflare wrangler dev command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["dev", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler dev failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    // store ids the cloudflare adapter operates on, not arbitrary
    // strings.
    const TEST_KV_ID: &str = "sessions";
    const TEST_KV_ID_ALT: &str = "cache";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

    /// RAII guard: prepends a directory to `$PATH` and restores the original
    /// value on drop. Mirrors the `PathPrepend` used in `push_cloud.rs`.
    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            let original = env::var_os("PATH");
            let new = match &original {
                Some(prev) => {
                    let mut accum = OsString::from(extra);
                    accum.push(":");
                    accum.push(prev);
                    accum
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new);
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

    // ---------- extract_namespace_id ----------

    #[test]
    fn extract_namespace_id_parses_wrangler_3_output() {
        // wrangler decorates these lines with unicode glyphs in real
        // output; we drop them from the fixture to keep the source
        // file ASCII-only (clippy::non_ascii_literal). The parser
        // requires both the `[[kv_namespaces]]` anchor and a
        // 32-char-lowercase-hex id.
        let stdout = r#"Creating namespace with title "my-kv"
Success!
Add the following to your configuration file in your kv_namespaces array:
[[kv_namespaces]]
binding = "my-kv"
id = "00112233445566778899aabbccddeeff"
"#;
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn extract_namespace_id_tolerates_extra_whitespace() {
        let stdout = "[[kv_namespaces]]\n   id   =   \"00112233445566778899aabbccddeeff\"   \n";
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn extract_namespace_id_returns_none_on_missing_id_line() {
        assert!(extract_namespace_id("nothing to see here").is_none());
        assert!(extract_namespace_id("").is_none());
        assert!(
            extract_namespace_id("[[kv_namespaces]]\nid = \"\"").is_none(),
            "empty value not a real id"
        );
    }

    #[test]
    fn extract_namespace_id_ignores_unrelated_lines_starting_with_id() {
        // `identifier = "..."` doesn't match -- we strip exactly the
        // prefix `id` then require `=`. Also doesn't match because
        // there's no `[[kv_namespaces]]` anchor.
        assert!(extract_namespace_id("[[kv_namespaces]]\nidentifier = \"x\"").is_none());
    }

    #[test]
    fn extract_namespace_id_requires_kv_namespaces_anchor() {
        // A bare `id = "<32-char-hex>"` line that isn't preceded by
        // `[[kv_namespaces]]` must not match -- otherwise a future
        // wrangler info line like `id = "<workspace_id>"` printed
        // somewhere else in stdout would be picked up as the
        // namespace id and silently corrupt wrangler.toml on writeback.
        let unanchored = "id = \"00112233445566778899aabbccddeeff\"\n";
        assert!(extract_namespace_id(unanchored).is_none());

        // A different table header BEFORE the `id` line scopes us
        // out of the kv-namespaces context.
        let other_block = "[[d1_databases]]\nid = \"00112233445566778899aabbccddeeff\"\n";
        assert!(extract_namespace_id(other_block).is_none());
    }

    #[test]
    fn extract_namespace_id_rejects_non_real_id_inside_kv_namespaces_anchor() {
        // Even with the anchor, the value must look like a real
        // Cloudflare id (32-char lowercase hex with the diversity
        // floor). Shorter or non-hex values are skipped, not
        // returned -- forces the operator to investigate stdout
        // drift rather than silently writing a bogus id.
        let stdout = "[[kv_namespaces]]\nbinding = \"my-kv\"\nid = \"abc123\"\n";
        assert!(extract_namespace_id(stdout).is_none());
    }

    fn write_wrangler(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("wrangler.toml");
        fs::write(&path, contents).expect("write wrangler.toml");
        path
    }

    // ---------- is_real_namespace_id ----------

    #[test]
    fn is_real_namespace_id_accepts_32_char_lowercase_hex_with_sufficient_diversity() {
        // 16-distinct-char fixture: maximum diversity.
        assert!(is_real_namespace_id("00112233445566778899aabbccddeeff"));
        // Realistic randomish fixture: 14 distinct chars.
        assert!(is_real_namespace_id("4a8f3c2b9e1d5670adef2839c4b6e1f0"));
    }

    #[test]
    fn is_real_namespace_id_rejects_placeholder_or_short_id() {
        assert!(!is_real_namespace_id("local-dev-placeholder"));
        assert!(!is_real_namespace_id("abc123"));
        assert!(!is_real_namespace_id(""));
    }

    #[test]
    fn is_real_namespace_id_rejects_uppercase_or_non_hex() {
        // Uppercase rejected: Cloudflare's API returns lowercase.
        assert!(!is_real_namespace_id("00112233445566778899AABBCCDDEEFF"));
        // Non-hex digits rejected.
        assert!(!is_real_namespace_id("z0112233445566778899aabbccddeeff"));
    }

    #[test]
    fn is_real_namespace_id_rejects_hex_shape_sentinels() {
        // 32-char lowercase hex but obvious hand-typed placeholder:
        // distinct-hex-digit count is below the diversity floor.
        // Real Cloudflare ids have effectively uniform random hex,
        // so collisions with this guard are astronomical.
        assert!(
            !is_real_namespace_id("00000000000000000000000000000000"),
            "all-zeros rejected"
        );
        assert!(
            !is_real_namespace_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "all-a rejected"
        );
        assert!(
            !is_real_namespace_id("deadbeefdeadbeefdeadbeefdeadbeef"),
            "deadbeef rejected (only 5 distinct chars: d,e,a,b,f)"
        );
        // Boundary: a real-looking id with the diversity floor or
        // more must still pass.
        assert!(
            is_real_namespace_id("00112233445566778899aabbccddeeff"),
            "16-distinct-char fixture must still pass"
        );
        // Exactly 6 distinct chars (a,b,c,d,e,f): on the boundary,
        // must pass.
        assert!(
            is_real_namespace_id("aabbccddeeffaabbccddeeffaabbccdd"),
            "6-distinct-char fixture (boundary) passes"
        );
    }

    // ---------- read_namespace_id ----------

    #[test]
    fn read_namespace_id_errors_when_kv_namespaces_is_non_array_value() {
        // `kv_namespaces = "oops"` is a malformed manifest. Silently
        // returning None there bubbles up as "did you run provision?"
        // -- a misleading error. The right surface is "manifest
        // doesn't match the expected shape".
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), "name = \"demo\"\nkv_namespaces = \"oops\"\n");
        let err = read_namespace_id(&path, TEST_CONFIG_ID)
            .expect_err("non-array kv_namespaces must error");
        assert!(
            err.contains("array-of-tables") || err.contains("inline array"),
            "error names the expected shapes: {err}"
        );
        assert!(
            err.contains("string"),
            "error names the offending kind: {err}"
        );
    }

    // ---------- extract_namespace_id (pinning behaviour) ----------

    #[test]
    fn extract_namespace_id_returns_first_real_match_inside_kv_namespaces_anchor() {
        // Pin: top-down scan, first qualifying line inside the
        // `[[kv_namespaces]]` anchor wins. Real wrangler output has
        // exactly one. A hypothetical future format with multiple
        // qualifying lines would surface the earliest, but only
        // values that look like real Cloudflare ids count.
        let stdout = "[[kv_namespaces]]\n\
                      id = \"00112233445566778899aabbccddeeff\"\n\
                      id = \"ffeeddccbbaa99887766554433221100\"\n";
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    // ---------- upsert_kv_namespace ----------

    #[test]
    fn upsert_kv_namespace_replaces_placeholder_id_for_existing_binding() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "placeholder replaced: {after}"
        );
        assert!(
            !after.contains("local-dev-placeholder"),
            "placeholder removed: {after}"
        );
        assert_eq!(
            after.matches("binding = \"sessions\"").count(),
            1,
            "no duplicate binding: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_appends_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), "name = \"demo\"\n");
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("binding = \"sessions\"")
                && after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "appended new entry: {after}"
        );
        assert!(
            after.contains("name = \"demo\""),
            "preserved original keys: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_appends_next_to_existing_entries() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"cache\"\nid = \"old\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("binding = \"cache\"") && after.contains("id = \"old\""),
            "existing entry kept: {after}"
        );
        assert!(
            after.contains("binding = \"sessions\""),
            "new entry added: {after}"
        );
        assert_eq!(
            after.matches("[[kv_namespaces]]").count(),
            2,
            "two entries: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_preserves_top_comments() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "# managed by hand -- please keep this line\nname = \"my-worker\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("# managed by hand"),
            "preserved comment: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_preserves_sibling_fields_on_existing_entry() {
        // toml_edit replaces only the `id` Item when we update it;
        // sibling fields on the same `[[kv_namespaces]]` table
        // (e.g. `preview_id`, custom annotations the user added)
        // must survive the rewrite. Pinning this so a future
        // toml_edit upgrade or a refactor can't silently drop
        // operator data.
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\npreview_id = \"local-preview\"\ndescription = \"hand-added by ops\"\n",
        );
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff").expect("upsert");
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "id rewritten: {after}"
        );
        assert!(
            after.contains("preview_id = \"local-preview\""),
            "preserved preview_id: {after}"
        );
        assert!(
            after.contains("description = \"hand-added by ops\""),
            "preserved description: {after}"
        );
    }

    #[test]
    fn upsert_kv_namespace_creates_file_when_wrangler_toml_missing() {
        // Orphan-namespace hazard: if `wrangler kv namespace create`
        // succeeds but wrangler.toml is missing at writeback time,
        // erroring here would leave the remote namespace orphaned
        // with no local reference. Symmetric with read_namespace_id's
        // NotFound -> Ok(None) behaviour: upsert treats NotFound as
        // "start with empty document" and writes the entry.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        assert!(!path.exists(), "precondition: file must not exist");
        upsert_kv_namespace(&path, TEST_KV_ID, "00112233445566778899aabbccddeeff")
            .expect("missing file is permissive");
        let after = fs::read_to_string(&path).expect("file now exists");
        assert!(
            after.contains("binding = \"sessions\""),
            "created file with new entry: {after}"
        );
        assert!(
            after.contains("id = \"00112233445566778899aabbccddeeff\""),
            "id written: {after}"
        );
    }

    // ---------- writeback shape pre-check ----------

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing.toml");
        check_kv_namespaces_writeback_shape(&path)
            .expect("missing file is permissive (upsert creates it)");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_kv_namespaces_absent() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write wrangler.toml");
        check_kv_namespaces_writeback_shape(&path).expect("no kv_namespaces => OK");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_ok_when_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(
            &path,
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        )
        .expect("write wrangler.toml");
        check_kv_namespaces_writeback_shape(&path)
            .expect("[[kv_namespaces]] is the writeback-supported shape");
    }

    #[test]
    fn check_kv_namespaces_writeback_shape_rejects_inline_array_with_actionable_message() {
        // Regression for the orphan-namespace hazard: pre-fix, a
        // `kv_namespaces = [{ binding = "sessions" }]` manifest (no
        // id present) made `read_namespace_id` return None ("not yet
        // provisioned") so provision shelled `wrangler kv namespace
        // create` successfully, then `upsert_kv_namespace`'s
        // `as_array_of_tables_mut()` returned None and the upsert
        // errored — leaving the freshly-created namespace orphaned
        // on Cloudflare. The pre-flight rejects the inline-array
        // shape BEFORE any account-side call.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("wrangler.toml");
        fs::write(
            &path,
            "name = \"demo\"\nkv_namespaces = [{ binding = \"sessions\" }]\n",
        )
        .expect("write wrangler.toml");
        let err = check_kv_namespaces_writeback_shape(&path)
            .expect_err("inline-array form must be rejected before provision shells out");
        assert!(
            err.contains("inline array")
                && err.contains("[[kv_namespaces]]")
                && err.contains("orphaned"),
            "error must name the inline-array form, the supported [[kv_namespaces]] form, AND the orphan hazard so the operator knows what's at stake: {err}"
        );
    }

    // ---------- provision (dry-run + error path) ----------

    #[test]
    fn provision_dry_run_does_not_invoke_wrangler() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let kv_ids: Vec<ResolvedStoreId> =
            ResolvedStoreId::from_logicals(&[TEST_KV_ID, TEST_KV_ID_ALT]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        // 2 KV + 1 config + 1 secret = 4 status lines.
        assert_eq!(out.status_lines.len(), 4);
        assert!(out.status_lines[0].contains("would run `wrangler kv namespace create sessions`"));
        assert!(out.status_lines[1].contains("would run `wrangler kv namespace create cache`"));
        assert!(out.status_lines[2].contains("would run `wrangler kv namespace create app_config`"));
        assert!(out.status_lines[3].contains("runtime-managed via `wrangler secret put`"));
        // Manifest untouched.
        let after = fs::read_to_string(dir.path().join("wrangler.toml")).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated wrangler.toml");
    }

    #[test]
    fn provision_dry_run_writes_resolved_platform_name_into_binding() {
        // Regression: provision used to receive only logical ids
        // and write them verbatim into wrangler.toml. With the
        // platform-name flow, an operator who sets
        // `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config`
        // sees `prod_config` land as the binding name (matching what
        // the runtime resolves via `env.kv(...)`), with the logical
        // id still mentioned for human-facing wording.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("wrangler kv namespace create prod_config"),
            "dry-run uses platform name in the `wrangler` invocation: {out:?}"
        );
        assert!(
            out.status_lines[0].contains("binding = \"prod_config\""),
            "dry-run writes platform name as the binding: {out:?}"
        );
        assert!(
            out.status_lines[0].contains("logical id `app_config`"),
            "logical id is preserved for operator wording: {out:?}"
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
        let err = CloudflareCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_dry_run_skips_bindings_already_provisioned_with_real_id() {
        let dir = tempdir().expect("tempdir");
        // 32-char lowercase hex id == real Cloudflare namespace id.
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("already provisioned")
                && out.status_lines[0].contains("00112233445566778899aabbccddeeff"),
            "skip line names the existing id: {out:?}"
        );
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("00112233445566778899aabbccddeeff"),
            "did not touch existing id: {after}"
        );
    }

    #[test]
    fn provision_dry_run_treats_placeholder_id_as_unprovisioned() {
        // A scaffolded wrangler.toml ships with placeholder ids the
        // user is expected to overwrite by running provision.
        // Dry-run should report the would-be create call, NOT the
        // already-provisioned skip.
        let dir = tempdir().expect("tempdir");
        write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("would run `wrangler kv namespace create sessions`"),
            "placeholder id is treated as unprovisioned: {out:?}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("no-store provision is fine");
        assert_eq!(
            out.status_lines,
            vec!["cloudflare has no declared stores to provision"]
        );
    }

    // ---------- find_namespace_id ----------

    #[test]
    fn find_namespace_id_reads_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let id = find_namespace_id(&path, TEST_CONFIG_ID).expect("found");
        assert_eq!(id, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn find_namespace_id_reads_inline_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\nkv_namespaces = [{ binding = \"app_config\", id = \"ffeeddccbbaa99887766554433221100\" }]\n",
        );
        let id = find_namespace_id(&path, TEST_CONFIG_ID).expect("found");
        assert_eq!(id, "ffeeddccbbaa99887766554433221100");
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"other\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let err = find_namespace_id(&path, TEST_CONFIG_ID).expect_err("missing must error");
        assert!(
            err.contains(TEST_CONFIG_ID) && err.contains("provision"),
            "error names the binding and points at provision: {err}"
        );
    }

    #[test]
    fn find_namespace_id_rejects_placeholder_id_with_provision_hint() {
        // A binding with `id = "local-dev-placeholder"` (or any
        // other non-32-char-hex value) is treated the same as
        // a missing binding: the operator needs to run provision
        // before the id is usable for `wrangler kv bulk put`.
        // Without this guard, push would shell out with the
        // placeholder as `--namespace-id=...` and fail at wrangler
        // with a less actionable error.
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"local-dev-placeholder\"\n",
        );
        let err =
            find_namespace_id(&path, TEST_CONFIG_ID).expect_err("placeholder id must be rejected");
        assert!(
            err.contains("local-dev-placeholder") && err.contains("provision"),
            "error names the placeholder and points at provision: {err}"
        );
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let err =
            find_namespace_id(&path, TEST_CONFIG_ID).expect_err("missing wrangler.toml must error");
        assert!(
            err.contains("provision"),
            "error points at provision: {err}"
        );
    }

    // ---------- bulk_payload ----------

    #[test]
    fn bulk_payload_emits_wrangler_array_of_key_value_objects() {
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        let raw = bulk_payload(&entries).expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let array = parsed.as_array().expect("array");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["key"], "greeting");
        assert_eq!(array[0]["value"], "hello");
        assert_eq!(array[1]["key"], "service.timeout_ms");
        assert_eq!(array[1]["value"], "1500");
    }

    #[test]
    fn bulk_payload_with_no_entries_is_empty_array() {
        let raw = bulk_payload(&[]).expect("empty payload");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed, serde_json::json!([]));
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_resolves_namespace_id_and_does_not_invoke_wrangler() {
        let dir = tempdir().expect("tempdir");
        let original =
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n";
        let path = write_wrangler(dir.path(), original);
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        // Header + per-entry preview, matching the fastly dry-run shape.
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would run `wrangler kv bulk put")
                && out[0].contains("--namespace-id=00112233445566778899aabbccddeeff"),
            "dry-run header names namespace id: {out:?}"
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
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, original, "dry-run must not mutate wrangler.toml");
    }

    #[test]
    fn push_dry_run_is_lenient_when_binding_not_yet_provisioned() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run is lenient: pre-provision preview is allowed");
        assert!(
            out[0].contains("<unresolved>") && out[0].contains("provision"),
            "dry-run header explains the namespace is unresolved and points at provision: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run still lists the entries it would push: {out:?}"
        );
    }

    #[test]
    fn push_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("k".to_owned(), "v".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml") && err.contains("config push"),
            "error explains the missing manifest pointer: {err}"
        );
    }

    #[test]
    fn push_real_run_errors_with_provision_hint_when_binding_absent() {
        // dry-run is now lenient (see
        // `push_dry_run_is_lenient_when_binding_not_yet_provisioned`),
        // but a real run still must err so we don't silently push
        // to a non-existent namespace.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("missing binding must error on real run");
        assert!(
            err.contains("provision") && err.contains(TEST_CONFIG_ID),
            "error points at provision: {err}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_after_resolving_namespace() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries")
                && out[0].contains("00112233445566778899aabbccddeeff"),
            "status line names empty + namespace id: {out:?}"
        );
    }

    // ---------- read_config_entry / read_config_entry_local (fake wrangler) ----------

    /// Build a tempdir containing a `wrangler` script that emits fixed stdout /
    /// stderr and exits with the given code. The files are written to siblings
    /// of the script so shell-active chars in the payloads don't get
    /// re-interpreted.
    #[cfg(unix)]
    fn fake_wrangler_returning(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("wrangler");
        let stdout_file = dir.path().join("stdout_payload.txt");
        let stderr_file = dir.path().join("stderr_payload.txt");
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        let script = format!(
            "#!/bin/sh\ncat '{stdout}'\ncat '{stderr}' >&2\nexit {code}\n",
            stdout = stdout_file.display(),
            stderr = stderr_file.display(),
            code = exit_code,
        );
        fs::write(&script_path, script).expect("write wrangler script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Build a fake `wrangler` that logs each argv token (one per line) to
    /// `out_path`, prints a single line of stdout, and exits 0.
    #[cfg(unix)]
    fn fake_wrangler_argv_log(out_path: &Path) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("wrangler");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{out}'; done\nprintf 'val'\n",
            out = out_path.display(),
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
        use std::sync::{Mutex, OnceLock};
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_present_on_success() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("hello-cloudflare", "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("wrangler exit-0 must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, "hello-cloudflare");
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_not_found_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("", "Error: key not found", 1);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
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

    /// Wrangler 4.x (verified 4.64.0) returns exit 0 + stdout
    /// `"Value not found"` for a missing key instead of exit 1 +
    /// stderr. The previous read path treated every exit-0 stdout
    /// as a `Present` envelope, which made the next CLI step try
    /// to parse `"Value not found"` as a `BlobEnvelope` and abort.
    /// A missing key in the blob model is valid initial state --
    /// the first push hasn't run yet -- not corrupt remote state,
    /// so it must map to `MissingKey`.
    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_wrangler_4_value_not_found_stdout() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("Value not found\n", "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("Wrangler 4.x exit-0 'Value not found' must map to MissingKey");
        if let ReadConfigEntry::Present(body) = &result {
            panic!(
                "expected MissingKey on Wrangler 4.x 'Value not found' stdout; \
                 got Present({body:?})",
            );
        }
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "Wrangler 4.x stdout='Value not found' (exit 0) must classify as MissingKey",
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_on_binding_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("", "Error: binding APP_CONFIG is not defined", 1);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("binding-error maps to MissingStore (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "binding stderr => MissingStore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_local_uses_local_flag() {
        // Verify that read_config_entry_local passes `--local` (not `--remote`)
        // to wrangler. We capture argv via a fake wrangler and check the args.
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let argv_log = project_dir.path().join("argv.txt");
        let fake = fake_wrangler_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry_local(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("local read succeeds");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "expected Present from local read"
        );
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            captured.contains("--local"),
            "read_local must pass --local to wrangler; got argv:\n{captured}"
        );
        assert!(
            !captured.contains("--remote"),
            "read_local must NOT pass --remote; got argv:\n{captured}"
        );
    }

    #[test]
    fn read_config_entry_requires_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let result = CloudflareCliAdapter.read_config_entry(
            dir.path(),
            None,
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("[adapters.cloudflare.adapter].manifest"),
                "error names the missing field: {err}"
            ),
            Ok(_) => panic!("expected Err when adapter_manifest_path is None"),
        }
    }
}
