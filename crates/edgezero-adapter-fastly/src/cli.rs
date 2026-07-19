use std::env;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use crate::chunked_config::{prepare_fastly_config_entries, resolve_fastly_config_value};
use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{
    Adapter, AdapterAction, AdapterPushContext, ProvisionStores, ReadConfigEntry, ResolvedStoreId,
    register_adapter,
};
use edgezero_adapter::scaffold::{
    AdapterBlueprint, AdapterFileSpec, CommandTemplates, DependencySpec, LoggingDefaults,
    ManifestSpec, ReadmeInfo, TemplateRegistration, register_adapter_blueprint,
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
        fallback: "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_fastly_wasm",
        repo_crate: "crates/edgezero-adapter-fastly",
        fallback: "edgezero-adapter-fastly = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-fastly\", default-features = false, features = [\"fastly\"] }",
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

const FASTLY_INSTALL_HINT: &str = "install the Fastly CLI (https://www.fastly.com/documentation/reference/tools/cli/) and try again";

/// The config store the runtime opens for `EDGEZERO__*` overrides. Compute@Edge
/// has no process env, so the runtime reads its config-store KEY selector from
/// here (see `env_config_from_runtime_dictionary` in lib.rs).
const RUNTIME_ENV_STORE: &str = "edgezero_runtime_env";

/// Base name of the staging twin of [`RUNTIME_ENV_STORE`]. The actual store is
/// named PER SERVICE — [`staging_selector_store_name`] appends the service id —
/// because Fastly config stores are account-wide, versionless resources: a
/// single shared twin would let a staged deploy of service B destructively
/// overwrite the selectors a staged version of service A is reading.
///
/// A staged deploy clones the active version, and a clone inherits its resource
/// links — so without a second store the staged version reads production's
/// selector, and therefore production's config. Fastly resource links are
/// per-version and carry an overridable NAME, so the staged draft links THIS
/// store under the name `edgezero_runtime_env`. The runtime opens that name and
/// gets staged config; the active version is untouched.
const RUNTIME_ENV_STAGING_STORE_PREFIX: &str = "edgezero_runtime_env_staging";

/// Env var carrying the Fastly API token (read by the Fastly CLI and
/// forwarded to the Fastly API via the `Fastly-Key` header). Part of
/// the Fastly staging lifecycle.
const FASTLY_API_TOKEN_ENV: &str = "FASTLY_API_TOKEN";
/// Env var carrying the default Fastly service id, used when
/// `--service-id` is not passed explicitly.
const FASTLY_SERVICE_ID_ENV: &str = "FASTLY_SERVICE_ID";

/// Flags `fastly compute update` accepts that take a VALUE (either
/// `--flag value` or `--flag=value`). Verified against
/// `fastly compute update --help` (Fastly CLI v15): the command's
/// `--service-id`/`-s`, `--service-name`, `--package`/`-p`, `--version`,
/// plus the global `--token`/`-t`.
const COMPUTE_UPDATE_VALUE_FLAGS: &[&str] = &[
    "--service-id",
    "-s",
    "--service-name",
    "--package",
    "-p",
    "--version",
    "--token",
    "-t",
];

/// Boolean flags `fastly compute update` accepts: the command's
/// `--autoclone` plus the Fastly CLI globals. NOTE the absence of
/// `--comment` -- `compute update` does NOT support it (unlike
/// `compute deploy`), which is why an operator `--comment` is routed to
/// `service-version update` instead (see `deploy_staged`).
const COMPUTE_UPDATE_BOOL_FLAGS: &[&str] = &[
    "--autoclone",
    "--accept-defaults",
    "-d",
    "--auto-yes",
    "-y",
    "--debug-mode",
    "--non-interactive",
    "-i",
    "--quiet",
    "-q",
    "--verbose",
    "-v",
];

struct FastlyCliAdapter;

/// An operator passthrough arg list split for a staged deploy (see
/// `split_staged_passthrough`).
struct StagedPassthrough {
    /// The `--comment` value, applied to the version separately via
    /// `fastly service-version update --comment` (`compute update` has
    /// no `--comment` flag).
    comment: Option<String>,
    /// Args `compute update` does not support; dropped with a warning
    /// rather than forwarded (forwarding them makes the CLI exit
    /// non-zero and fails the whole staged deploy).
    dropped: Vec<String>,
    /// Args that `fastly compute update` actually supports.
    forwarded: Vec<String>,
}

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
            // Fastly staging lifecycle.
            AdapterAction::DeployStaged => deploy_staged(args),
            AdapterAction::EmitVersion => emit_active_version(args),
            AdapterAction::Healthcheck => healthcheck(args),
            AdapterAction::Rollback => rollback(args),
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
                let post_create_note = resource_link_note(&fastly_path, kind, name)?;
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
        // EdgeZero runtime overrides live in a dedicated Fastly Config
        // Store named `edgezero_runtime_env`. Compute@Edge has no
        // process env, so `EDGEZERO__STORES__CONFIG__<ID>__KEY` and
        // similar overrides have to come from a platform Config Store
        // the runtime opens by name (see
        // `env_config_from_runtime_dictionary` in lib.rs). Provision
        // owns the store creation alongside the operator's declared
        // stores so the runtime override path is wired correctly out
        // of the box; if the store already appears in
        // `[setup.config_stores.edgezero_runtime_env]`, skip.
        let runtime_env_kind = "config";
        let runtime_env_name = "edgezero_runtime_env";
        if dry_run {
            out.push(format!(
                "would run `fastly {runtime_env_kind}-store create --name={runtime_env_name}` and append [setup.{runtime_env_kind}_stores.{runtime_env_name}] to {} (EdgeZero runtime override store)",
                fastly_path.display()
            ));
        } else if !setup_block_present(&fastly_path, runtime_env_kind, runtime_env_name)? {
            create_fastly_store(runtime_env_kind, runtime_env_name)?;
            append_fastly_setup(&fastly_path, runtime_env_kind, runtime_env_name).map_err(
                |err| {
                    format!(
                        "fastly {runtime_env_kind}-store `{runtime_env_name}` was created remotely, but writeback to {path} failed: {err}\n  Recover via `fastly {runtime_env_kind}-store delete --name={runtime_env_name}` then re-run `edgezero provision --adapter fastly`.",
                        path = fastly_path.display()
                    )
                },
            )?;
            // Same already-deployed-service caveat as the declared-store
            // path: if `service_id` is set in fastly.toml, the
            // `[setup.config_stores.edgezero_runtime_env]` table won't
            // be re-applied by the next `fastly compute deploy`, so the
            // runtime can't open the store. Emit the resource-link
            // remediation alongside the populate-keys hint.
            let post_create_note =
                resource_link_note(&fastly_path, runtime_env_kind, runtime_env_name)?;
            // NB: this store is what the ACTIVE (production) service reads. The
            // example must never point it at a staging key — following that would
            // make production serve staged config. Staged versions get their own
            // selector via `edgezero_runtime_env_staging`, wired automatically by
            // a staged deploy; nothing here should be edited to stage config.
            let mut line = format!(
                "created fastly {runtime_env_kind}-store `{runtime_env_name}` (EdgeZero runtime override store, read by the ACTIVE version); appended setup tables to {}\n  It already selects each store's default key, so no edit is needed for a normal setup.\n  To point PRODUCTION at a different key (e.g. a renamed store), and only then:\n    fastly config-store-entry update --store-id=<STORE-ID> --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY --value=<production-key> --upsert\n  Do NOT set a `_staging` key here: staged config is isolated by a per-service `{RUNTIME_ENV_STAGING_STORE_PREFIX}_<service-id>` store, which a staged deploy creates and links automatically.",
                fastly_path.display()
            );
            if let Some(note) = post_create_note {
                line.push('\n');
                line.push_str(&note);
            }
            out.push(line);
        } else {
            // Already declared; nothing to do.
        }

        // The STAGING twin of the runtime-override store is created and
        // populated entirely by a staged deploy (see
        // `relink_runtime_env_for_staging` → `mirror_production_to_staging`), so
        // it always mirrors production's CURRENT overrides. Provision does not
        // touch it: a twin populated here would drift the moment an operator
        // edited a production override.

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
                return Ok(ReadConfigEntry::MissingStore);
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

/// If fastly.toml declares `service_id`, the next
/// `fastly compute deploy` skips `[setup]` entirely (it only runs on
/// the FIRST deploy of a service). Any store created by provision
/// after that needs a separate `fastly resource-link create` to link
/// the platform store to the service version. This helper returns the
/// remediation note to surface in the provision output, or `None`
/// when the service hasn't been deployed yet (so the next
/// `compute deploy` will pick up the `[setup]` row automatically).
fn resource_link_note(path: &Path, kind: &str, name: &str) -> Result<Option<String>, String> {
    let note = read_fastly_service_id(path)?.map(|svc_id| {
        format!(
            "  fastly.toml declares `service_id = \"{svc_id}\"`, so this service is already deployed -- `[setup]` will NOT be re-run on the next `fastly compute deploy`. The store exists in the account but is NOT yet linked to the service. To finish provisioning, look up the store id with `fastly {kind}-store list --json` (match by name=`{name}`), then run:\n    fastly resource-link create --service-id={svc_id} --resource-id=<STORE-ID> --version=latest --autoclone --name={name}\n  (the link clones the active version so existing traffic is not affected until you `fastly service-version activate`)."
        )
    });
    Ok(note)
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
    use toml_edit::{DocumentMut, Item, table};

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
    use toml_edit::{DocumentMut, Item, Table, Value, table};

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
    // previously-pushed `app_config` blob. The
    // default + staging keys must coexist so the runtime
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

/// Read every `(key, value)` in config store `store_id` via
/// `fastly config-store-entry list --store-id=<id> --json`.
///
/// Accepts a bare array or an `{"items": [...]}` envelope, and reads each
/// entry's key/value from `item_key`/`item_value` (the field names
/// `config-store-entry describe` uses), falling back to `key`/`value`. A parse
/// failure is an error, NOT an empty list: a staged deploy mirrors this store,
/// and treating an unreadable listing as "no entries" would silently drop
/// production's overrides from the staged version.
fn read_config_store_entries(store_id: &str, cwd: &Path) -> Result<Vec<(String, String)>, String> {
    let stdout = run_fastly_capture(
        &[
            "config-store-entry".to_owned(),
            "list".to_owned(),
            format!("--store-id={store_id}"),
            "--json".to_owned(),
        ],
        cwd,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|err| {
        format!("failed to parse `fastly config-store-entry list --json` JSON: {err}\nraw stdout: {stdout}")
    })?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
        .ok_or_else(|| {
            format!(
                "`fastly config-store-entry list --json` output is neither a bare array nor an `items` envelope; fastly CLI may have changed its schema. Raw stdout: {stdout}"
            )
        })?;
    let mut entries = Vec::with_capacity(array.len());
    for entry in array {
        let key = entry
            .get("item_key")
            .or_else(|| entry.get("key"))
            .and_then(serde_json::Value::as_str);
        let value = entry
            .get("item_value")
            .or_else(|| entry.get("value"))
            .and_then(serde_json::Value::as_str);
        match (key, value) {
            (Some(found_key), Some(found_value)) => {
                entries.push((found_key.to_owned(), found_value.to_owned()));
            }
            _ => {
                return Err(format!(
                    "a `fastly config-store-entry list --json` entry has no string `item_key`/`item_value` fields; fastly CLI may have changed its schema. Raw stdout: {stdout}"
                ));
            }
        }
    }
    Ok(entries)
}

/// `fastly config-store-entry delete --store-id=<id> --key=<k>`.
fn delete_config_store_entry(store_id: &str, key: &str, cwd: &Path) -> Result<(), String> {
    run_fastly_status(
        &[
            "config-store-entry".to_owned(),
            "delete".to_owned(),
            format!("--store-id={store_id}"),
            format!("--key={key}"),
        ],
        cwd,
    )
}

/// Compute the staging selector store's entries from production's, given the
/// declared config-store logical ids.
///
/// The twin is a faithful MIRROR of production's runtime overrides — adapter
/// host, logging level, `__NAME` redirects — with exactly one transform: every
/// declared config store's selector key (`EDGEZERO__STORES__CONFIG__<ID>__KEY`)
/// points at `<logical>_staging`, the key `config push --staging` writes. A
/// declared store gets that selector even when production has no explicit entry
/// for it (production relies on the runtime's default = the logical id; staging
/// must NOT inherit that default, or it would read production's key).
///
/// Pure so the transform is unit-testable without the fastly CLI.
fn staging_entries_from_production(
    production: &[(String, String)],
    config_logical_ids: &[String],
) -> Vec<(String, String)> {
    // selector key -> staging value, one per declared config store.
    let selectors: Vec<(String, String)> = config_logical_ids
        .iter()
        .map(|id| (runtime_env_key_for(id), format!("{id}_staging")))
        .collect();
    let is_selector = |key: &str| selectors.iter().any(|(sel, _)| sel == key);

    // Copy every non-selector production override verbatim; selectors are
    // supplied from `selectors` below (whether or not production carried one).
    let mut out: Vec<(String, String)> = production
        .iter()
        .filter(|(key, _)| !is_selector(key))
        .cloned()
        .collect();
    out.extend(selectors);
    out
}

/// Resolve the staging twin store, creating it on demand. A staged deploy owns
/// this store end to end (it is never linked on the ACTIVE version), so it does
/// not depend on `provision` having created it first. Fails closed on a lookup
/// FAILURE rather than blindly creating a duplicate.
/// The per-service staging twin store name — the base prefix plus the service
/// id, so concurrent staged deploys of different services on one account never
/// clobber each other's selectors.
fn staging_selector_store_name(service_id: &str) -> String {
    format!("{RUNTIME_ENV_STAGING_STORE_PREFIX}_{service_id}")
}

fn ensure_staging_selector_store(store_name: &str) -> Result<String, String> {
    match classify_remote_config_store(store_name)? {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => {
            create_fastly_store("config", store_name)?;
            resolve_remote_config_store_id(store_name).map_err(|err| {
                format!(
                    "created fastly config-store `{store_name}` but could not resolve its id: {err}"
                )
            })
        }
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` while resolving `{store_name}`: {detail}.\n  Refusing to stage. Pin a known-compatible fastly CLI version and retry."
        )),
    }
}

/// Reconcile the staging twin so it MIRRORS production's runtime overrides
/// (`production`) with only the config selectors redirected to
/// `<logical>_staging`.
///
/// Deletes twin entries production no longer has (so a removed override does not
/// linger and diverge staging from production), then upserts the full desired
/// set. Runs while the staged draft is still editable, before the relink. When
/// production has NO override store, `production` is empty and the twin holds
/// only the derived staging selectors — staging is still isolated.
fn mirror_production_to_staging(
    production: &[(String, String)],
    staging_id: &str,
    config_logical_ids: &[String],
    cwd: &Path,
) -> Result<(), String> {
    let desired = staging_entries_from_production(production, config_logical_ids);

    let current = read_config_store_entries(staging_id, cwd)?;
    for (key, _) in &current {
        if !desired.iter().any(|(dk, _)| dk == key) {
            delete_config_store_entry(staging_id, key, cwd)?;
        }
    }
    for (key, value) in &desired {
        create_config_store_entry(staging_id, key, value)?;
    }
    Ok(())
}

/// The runtime-override entry naming the config-store KEY for logical store
/// `id` — `EDGEZERO__STORES__CONFIG__<ID>__KEY`.
///
/// Must match what the runtime reads: `EnvConfig::from_vars` strips the
/// `EDGEZERO__` prefix, splits on `__`, and lowercases each segment, and
/// `store_key("config", id)` looks up `["stores", "config", id, "key"]`. So the
/// entry name is the id uppercased. A near-miss here is silent — the runtime
/// would just fall back to the id and read production config.
fn runtime_env_key_for(logical_id: &str) -> String {
    format!(
        "EDGEZERO__STORES__CONFIG__{}__KEY",
        logical_id.to_ascii_uppercase()
    )
}

/// Find the id of the resource link published under `link_name` in
/// `fastly resource-link list --json` output.
///
/// The link's own `name` is an alias that defaults to the linked resource's
/// name, so match on it rather than the resource name — the whole point of the
/// staging relink is that a store named `edgezero_runtime_env_staging` is linked
/// under the name `edgezero_runtime_env`.
///
/// Returns `None` when the version has no such link (nothing to delete).
fn find_resource_link_id(stdout: &str, link_name: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let array = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))?;
    array.iter().find_map(|entry| {
        let name = entry.get("name").and_then(serde_json::Value::as_str)?;
        if name != link_name {
            return None;
        }
        entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
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
    // Scan the WHOLE list — never short-circuit on the first match — so every
    // entry is validated and a second entry with the same name (an ambiguous
    // listing) is caught. EVERY entry must be well-formed before a Found/NotFound
    // verdict can be trusted: a malformed entry could be the very store we're
    // looking for, hidden behind a missing `name`/`id`, and reporting NotFound
    // then would fail OPEN (e.g. staging would mirror no production overrides).
    let mut found: Option<String> = None;
    for entry in array {
        let (Some(entry_name), Some(entry_id)) = (
            entry.get("name").and_then(serde_json::Value::as_str),
            entry.get("id").and_then(serde_json::Value::as_str),
        ) else {
            return ConfigStoreLookup::SchemaDrift(format!(
                "a config-store entry is missing a string `name` or `id` field, so the listing cannot be trusted -- fastly CLI may have changed its output schema. Entry: {entry}"
            ));
        };
        if entry_name == name {
            if found.is_some() {
                return ConfigStoreLookup::SchemaDrift(format!(
                    "the config-store listing has more than one store named `{name}`, so a lookup is ambiguous -- refusing to pick one"
                ));
            }
            found = Some(entry_id.to_owned());
        }
    }
    found.map_or(ConfigStoreLookup::NotFound, ConfigStoreLookup::Found)
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
    match classify_remote_config_store(name)? {
        ConfigStoreLookup::Found(id) => Ok(id),
        ConfigStoreLookup::NotFound => Err(format!(
            "no fastly config-store matches `{name}` (did you run `edgezero provision --adapter fastly`?)"
        )),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// Look a config store up by name and return the raw [`ConfigStoreLookup`], so
/// callers can tell "the account has no such store" (`NotFound`) apart from "the
/// lookup itself failed" (`Err` — CLI missing / non-zero exit — or
/// `SchemaDrift`). A staged deploy relies on that distinction to decide whether
/// to skip config isolation (genuinely no store) or fail closed (couldn't tell).
///
/// `Err` is only for a failure to OBTAIN an answer; a successful listing that
/// simply doesn't contain `name` is `Ok(ConfigStoreLookup::NotFound)`.
fn classify_remote_config_store(name: &str) -> Result<ConfigStoreLookup, String> {
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
    Ok(find_config_store_id(&stdout, name))
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

/// Whether `args` already carries the Fastly CLI's non-interactive
/// switch, in either its long (`--non-interactive`) or short (`-i`)
/// form. Used to avoid passing the flag twice when a caller already
/// supplied it via `deploy-args` passthrough.
fn has_non_interactive(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--non-interactive" || arg == "-i")
}

/// Build the argv for `fastly compute deploy`, appending
/// `--non-interactive` (a Fastly CLI *global* flag, supported by
/// `compute deploy`) unless the caller already passed it. Without it a
/// production deploy can block on an interactive prompt in CI.
fn build_compute_deploy_args(extra_args: &[String]) -> Vec<String> {
    let mut argv = vec!["compute".to_owned(), "deploy".to_owned()];
    argv.extend_from_slice(extra_args);
    if !has_non_interactive(extra_args) {
        argv.push("--non-interactive".to_owned());
    }
    argv
}

/// # Errors
/// Returns an error if the Fastly CLI deploy command fails.
///
/// Honours a CLI-threaded `--manifest-path <abs fastly.toml>` (see
/// [`resolve_manifest_dir`]) so a monorepo with several Fastly apps
/// deploys the one the operator's `edgezero.toml` selected, rather than
/// whichever `fastly.toml` a bare working-directory search finds first.
/// The flag is EdgeZero-internal — `fastly compute deploy` has no such
/// flag — so it is stripped from the forwarded argv.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest_dir = resolve_manifest_dir(extra_args)?;
    let forwarded = args_without_flag_value(extra_args, "--manifest-path");

    let status = Command::new("fastly")
        .args(build_compute_deploy_args(&forwarded))
        .current_dir(&manifest_dir)
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

// ===================================================================
// Fastly staging lifecycle
// ===================================================================
//
// These entry points back the `deploy --stage`, `healthcheck`, and
// `rollback` app-CLI subcommands. They mirror the Fastly semantics of
// `stackpop/trusted-server-actions`:
//
//   * staged deploy  → build + `compute update --autoclone` (no
//     activation) + `service-version stage`; emits the staged version.
//   * production      → `fastly compute deploy` runs via the manifest
//     command; `emit_active_version` resolves the activated version.
//   * healthcheck     → curl the domain (production) or the version's
//     resolved staging IP (`--staging`); non-zero exit when unhealthy.
//   * rollback        → activate `<v>-1` (production) or deactivate
//     `<v>` (staging) via the Fastly API.
//
// **Version-output contract:** deploy/stage print a
// single `version=<N>` line to stdout (via `log::info!`, which the CLI
// logger emits verbatim). The `deploy-fastly` action greps that line
// to surface `fastly-version`. Rollback prints `rolled-back-to=<N>`.
//
// Provider HTTP calls shell out to `curl` (matching
// trusted-server-actions and avoiding a WASM-incompatible HTTP client
// in the adapter). The `FASTLY_API_TOKEN` is passed to `curl` via a
// `--config -` stdin file rather than on argv, so it never appears in
// `ps` / `/proc/<pid>/cmdline` (same discipline as
// `create_config_store_entry`'s `--stdin`).

/// Value that follows `flag` in a `--flag value` arg slice, if present.
fn arg_value<'args>(args: &'args [String], flag: &str) -> Option<&'args str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| idx.checked_add(1))
        .and_then(|idx| args.get(idx))
        .map(String::as_str)
}

/// Whether a boolean `flag` (e.g. `--staging`) is present in `args`.
fn arg_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

/// Copy of `args` with `--flag value` removed (both tokens). Used to
/// forward operator passthrough (e.g. `--comment`) to `fastly compute
/// update` without re-passing `--service-id`, which is threaded
/// explicitly.
fn args_without_flag_value(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == flag {
            skip = true;
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Split an arg on a leading `--flag=value`, returning `(flag, value)`.
fn split_inline_value(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with('-') => (flag, Some(value)),
        Some(_) | None => (arg, None),
    }
}

/// Partition operator passthrough args for a staged deploy: forward only
/// what `fastly compute update` supports, lift `--comment` out (it is a
/// `compute deploy` / `service-version update` flag, NOT a
/// `compute update` one), and drop the rest.
///
/// Both `--comment value` and `--comment=value` are recognised.
fn split_staged_passthrough(args: &[String]) -> StagedPassthrough {
    let mut split = StagedPassthrough {
        forwarded: Vec::with_capacity(args.len()),
        comment: None,
        dropped: Vec::new(),
    };
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline) = split_inline_value(arg);
        if flag == "--comment" {
            split.comment = match inline {
                Some(value) => Some(value.to_owned()),
                None => iter.next().cloned(),
            };
        } else if COMPUTE_UPDATE_VALUE_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
            if inline.is_none()
                && let Some(value) = iter.next()
            {
                split.forwarded.push(value.clone());
            }
        } else if COMPUTE_UPDATE_BOOL_FLAGS.contains(&flag) {
            split.forwarded.push(arg.clone());
        } else {
            // Unsupported by `compute update`. Consume a detached value
            // too, so a stray `stage` from `--env stage` is not left
            // behind as a bogus positional.
            split.dropped.push(flag.to_owned());
            if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                iter.next();
            }
        }
    }
    split
}

/// Resolve the target service id from `--service-id` or, failing that,
/// `FASTLY_SERVICE_ID`.
fn resolve_service_id(args: &[String]) -> Result<String, String> {
    if let Some(value) = arg_value(args, "--service-id") {
        return Ok(value.to_owned());
    }
    env::var(FASTLY_SERVICE_ID_ENV).map_err(|_err| {
        format!("no service id: pass `--service-id <id>` or set {FASTLY_SERVICE_ID_ENV}")
    })
}

/// Read the required Fastly API token from the environment.
fn require_token() -> Result<String, String> {
    env::var(FASTLY_API_TOKEN_ENV)
        .map_err(|_err| format!("{FASTLY_API_TOKEN_ENV} must be set in the environment"))
}

/// Whether an HTTP status counts as healthy (2xx/3xx).
fn is_healthy_status(code: u16) -> bool {
    (200..400).contains(&code)
}

/// Digits immediately following `marker` in `lower` (a lowercased
/// haystack), for the LAST occurrence of `marker`. The number must be
/// terminated by `terminator` — so a partial/confusable match (e.g. a
/// semver `15.2.0`) yields `None` rather than a bogus version.
fn last_version_after(lower: &str, marker: &str, terminator: char) -> Option<u64> {
    let mut result = None;
    for (idx, _) in lower.match_indices(marker) {
        let after = idx.saturating_add(marker.len());
        let Some(rest) = lower.get(after..) else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || rest.chars().nth(digits.len()) != Some(terminator) {
            continue;
        }
        if let Ok(parsed) = digits.parse::<u64>() {
            result = Some(parsed);
        }
    }
    result
}

/// Parse a Fastly service version out of Fastly CLI output, accepting
/// ONLY the shapes the CLI actually emits, in precedence order:
///
///   1. Our canonical `version=<N>` contract line.
///   2. The CLI's success line, whose Go format string is
///      `"Updated package (service %s, version %v)"` (and
///      `"Deployed package (...)"` for `compute deploy`) — matched as
///      `, version <N>)`. This names the version the package landed on,
///      so it wins over (3).
///   3. The `--autoclone` notice, `"... Now operating on version %d."` —
///      the freshly-cloned draft, used when the success line is absent.
///
/// Everything else yields `None` and the caller FAILS CLOSED.
///
/// Deliberately strict. The previous implementation took ANY digits
/// appearing after the word "version" and let the last match win, so:
///   * `Uploaded package to service 12345, version unchanged` parsed as
///     version 12345, and
///   * the autoclone notice's *pre-clone* version
///     (`Service version 3 is not editable...`) could beat the real one,
///     since stdout and stderr are concatenated and their relative order
///     is not guaranteed.
///
/// A misparse here stages, comments, or rolls back the WRONG service
/// version, so ambiguity must be an error, not a guess.
fn parse_fastly_version(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    parse_canonical_version_line(&lower)
        .or_else(|| last_version_after(&lower, ", version ", ')'))
        .or_else(|| last_version_after(&lower, "now operating on version ", '.'))
}

/// Last standalone `version=<N>` line (the whole trimmed line must be
/// exactly that, so a `--version=active` flag echoed in a command line
/// cannot masquerade as one).
fn parse_canonical_version_line(lower: &str) -> Option<u64> {
    lower.lines().rev().find_map(|line| {
        let digits = line.trim().strip_prefix("version=")?;
        (!digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// Parse `fastly service-version list --json` (or the Fastly API
/// `/service/<id>/version` array) for the `number` of the `active`
/// version.
/// Resolve the active version from a Fastly version-list JSON.
///
/// `Ok(Some(n))` — exactly one version is active. `Ok(None)` — the list parsed
/// but NO version is active (a first-ever deploy; the caller records an empty
/// rollback target and proceeds). `Err(_)` — the payload could not be parsed as
/// a version list, OR it is MALFORMED (a non-boolean `active` on ANY entry, an
/// `active: true` entry whose `number` is missing or not an unsigned integer, or
/// MORE THAN ONE active version). All are OPERATIONAL failures the caller must
/// NOT silently treat as "no active version" — otherwise a garbled or ambiguous
/// response would fail open and let a production deploy proceed with no rollback
/// target.
///
/// The ENTIRE list is scanned (not short-circuited at the first active entry) so
/// that a malformed `active` field or a second active version anywhere in the
/// response is caught rather than ignored.
fn resolve_active_version(json: &str) -> Result<Option<u64>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("failed to parse the Fastly version list as JSON: {err}"))?;
    let array = value.as_array().ok_or_else(|| {
        "the Fastly version list was not a JSON array; the API may have changed its schema"
            .to_owned()
    })?;
    let mut active_version: Option<u64> = None;
    for entry in array {
        // EVERY entry must be a well-formed version object with an unsigned
        // integer `number` — Fastly includes it on every version. A `null`, a
        // non-object, or a missing/non-integer `number` means the response
        // cannot be trusted; treating such an entry as merely "not active" would
        // let a garbled payload read as "no active version" (fail open).
        let Some(object) = entry.as_object() else {
            return Err(format!(
                "a Fastly version list element is not an object; the API may have changed its schema. Element: {entry}"
            ));
        };
        let number = object.get("number").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            format!(
                "a Fastly version entry has no unsigned-integer `number`; the API may have changed its schema. Entry: {entry}"
            )
        })?;
        // `active` is optional (an omitted field means not active), but a PRESENT
        // non-boolean is schema drift.
        let active = match object.get("active") {
            None => false,
            Some(active_field) => active_field.as_bool().ok_or_else(|| {
                format!(
                    "a Fastly version entry has a non-boolean `active` field; the API may have changed its schema. Entry: {entry}"
                )
            })?,
        };
        if active {
            if active_version.is_some() {
                return Err(format!(
                    "the Fastly version list reports more than one active version ({} and {number}); the response is ambiguous, refusing to pick one",
                    active_version.unwrap_or_default()
                ));
            }
            active_version = Some(number);
        }
    }
    Ok(active_version)
}

/// First staging IP found in a Fastly
/// `GET /service/<id>/version/<n>/domain?include=staging_ips` response.
///
/// The response is an ARRAY of domain objects, and the staging address
/// is a SINGULAR, nullable STRING field named `staging_ip` on each
/// domain (`staging_ips` is only the `include=` query-param value, never
/// a field name). Verified against the go-fastly `Domain` model, whose
/// field is `StagingIP` with the mapstructure tag `staging_ip`, and its
/// recorded API fixture `fixtures/domains/list_with_staging_ips.yaml`,
/// plus Fastly's "working with staging" guide. The field is absent from
/// the published Domain data model, so it is treated as optional.
///
/// We also tolerate a plural `staging_ips` array, in case a Fastly
/// response (or a future API version) carries that shape.
fn parse_staging_ip(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    find_staging_ip(&value)
}

fn find_staging_ip(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            // The documented shape: a singular `staging_ip` string.
            if let Some(ip) = map.get("staging_ip").and_then(serde_json::Value::as_str) {
                return Some(ip.to_owned());
            }
            // Tolerated: a plural `staging_ips` array of strings.
            if let Some(ip) = map
                .get("staging_ips")
                .and_then(serde_json::Value::as_array)
                .and_then(|arr| arr.iter().find_map(serde_json::Value::as_str))
            {
                return Some(ip.to_owned());
            }
            map.values().find_map(find_staging_ip)
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(find_staging_ip),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => None,
    }
}

/// Build the `curl` argv for a health probe. Production probes the
/// domain directly; staging reroutes the TLS connection to the
/// resolved staging IP via `--connect-to ::<ip>:443`.
fn build_curl_probe_args(domain: &str, staging_ip: Option<&str>, timeout_secs: u64) -> Vec<String> {
    let mut args = vec![
        "-sS".to_owned(),
        "-o".to_owned(),
        "/dev/null".to_owned(),
        "-w".to_owned(),
        "%{http_code}".to_owned(),
        "--max-time".to_owned(),
        timeout_secs.to_string(),
    ];
    if let Some(ip) = staging_ip {
        args.push("--connect-to".to_owned());
        args.push(format!("::{ip}:443"));
    }
    args.push(format!("https://{domain}/"));
    args
}

/// Retry a health probe. Returns `Ok(code)` on the first healthy
/// status, or `Err((last_code, message))` after exhausting attempts.
/// `between` runs between attempts (not after the last) so it can be a
/// no-op in tests.
fn probe_with_retries<P, S>(
    retry: u32,
    mut prober: P,
    mut between: S,
) -> Result<u16, (Option<u16>, String)>
where
    P: FnMut() -> Result<u16, String>,
    S: FnMut(),
{
    let attempts = retry.max(1);
    let mut last_code = None;
    let mut last_msg = "no probe attempts were made".to_owned();
    for attempt in 0..attempts {
        match prober() {
            Ok(code) if is_healthy_status(code) => return Ok(code),
            Ok(code) => {
                last_code = Some(code);
                last_msg = format!("unhealthy HTTP status {code}");
            }
            Err(err) => last_msg = err,
        }
        if attempt.saturating_add(1) < attempts {
            between();
        }
    }
    Err((last_code, last_msg))
}

/// Run `fastly <args>` in `cwd`, inheriting stdio, and map a non-zero
/// exit to an error.
fn run_fastly_status(fastly_args: &[String], cwd: &Path) -> Result<(), String> {
    let status = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .status()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`fastly {}` exited with status {status}",
            fastly_args.join(" ")
        ))
    }
}

/// Run `fastly <args>` in `cwd` capturing stdout+stderr (combined) for
/// version parsing. Errors on a non-zero exit.
fn run_fastly_capture(fastly_args: &[String], cwd: &Path) -> Result<String, String> {
    let output = Command::new("fastly")
        .args(fastly_args)
        .current_dir(cwd)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to run fastly CLI: {err}")
            }
        })?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "`fastly {}` exited with status {}\n{}",
            fastly_args.join(" "),
            output.status,
            combined.trim()
        ))
    }
}

/// Run `curl -sS --config -`, piping `config` (which carries the
/// `Fastly-Key` header + url) through stdin so the token never touches
/// argv. Returns stdout on a zero exit.
fn curl_config_capture(config: &str) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args(["-sS", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                "`curl` not found on PATH; install curl and retry".to_owned()
            } else {
                format!("failed to spawn `curl`: {err}")
            }
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `curl`".to_owned())?;
    stdin
        .write_all(config.as_bytes())
        .map_err(|err| format!("failed to write curl config to stdin: {err}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `curl`: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "`curl` exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Wrap `value` in a curl-config double-quoted string, escaping the
/// characters that would otherwise let a value terminate its quote and
/// inject additional curl options. Within a curl `--config` file a
/// double-quoted value only honours the escapes `\\`, `\"`, `\n`, `\r`,
/// `\t` (and the config is parsed line-by-line, so a raw newline ends
/// the directive regardless of quoting). We escape backslash and quote
/// so the value cannot break out of the quotes, and map raw control
/// characters to their escape form so NO raw newline (or CR/tab) is
/// ever written into the config file. This is the second half of the
/// injection defence: untrusted identifiers are also validated (see
/// `validate_service_id` / `validate_version_str` / `validate_domain`),
/// but the token is a secret we cannot constrain to a charset, so it
/// relies on this escaping alone.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len().saturating_add(2));
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Validate an operator-supplied Fastly service id before it is
/// interpolated into an API URL. Fastly service ids are opaque
/// alphanumeric handles; constrain to `^[A-Za-z0-9_-]+$` so a value
/// carrying a quote / newline / space (which could inject curl options
/// via the `--config` file) is rejected with a clear error.
fn validate_service_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        Ok(())
    } else {
        Err(format!(
            "invalid service id {id:?}: expected only ASCII letters, digits, `_`, or `-`"
        ))
    }
}

/// Validate a service-version string is a plain non-negative integer
/// before it is interpolated into an API URL. Returns the parsed value
/// so callers can reuse it.
fn validate_version_str(version: &str) -> Result<u64, String> {
    version.parse::<u64>().map_err(|err| {
        format!("invalid version {version:?}: expected a non-negative integer: {err}")
    })
}

/// Validate a domain is a plausible hostname before it is placed into a
/// `curl` URL. Rejects anything outside the DNS label charset
/// (`[A-Za-z0-9-.]`), empty / over-long values, leading/trailing dots,
/// and empty labels so an injected quote / slash / space / newline
/// cannot smuggle curl options or a second URL.
fn validate_domain(domain: &str) -> Result<(), String> {
    let charset_ok = domain
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.');
    let shape_ok = !domain.is_empty()
        && domain.len() <= 253
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..");
    if charset_ok && shape_ok {
        Ok(())
    } else {
        Err(format!(
            "invalid domain {domain:?}: expected a hostname like `example.com`"
        ))
    }
}

/// `GET https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the response body ONLY on a 2xx status. Both the header (carrying the
/// secret token) and the URL are written through `curl_quote` so neither can
/// inject curl options into the `--config` document.
///
/// The HTTP status is captured explicitly via `write-out` (as the PUT helper
/// does) and required to be 2xx before the body is trusted. `--fail` alone would
/// reject 4xx/5xx but still accept a 3xx — whose (array-shaped) body could
/// otherwise be parsed as version data. No `location` directive is set, so a
/// redirect is never followed.
fn fastly_api_get(path: &str, token: &str) -> Result<String, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    // `write-out` appends the status on its own trailing line AFTER the body.
    let config = format!("header = {header}\nurl = {url}\nwrite-out = \"\\n%{{http_code}}\"\n");
    let out = curl_config_capture(&config)
        .map_err(|err| format!("Fastly API GET {path} failed: {err}"))?;
    let (body, status_line) = out
        .rsplit_once('\n')
        .ok_or_else(|| format!("Fastly API GET {path}: no HTTP status in the curl output"))?;
    let status: u16 = status_line.trim().parse().map_err(|err| {
        format!(
            "Fastly API GET {path}: could not parse the HTTP status {:?}: {err}",
            status_line.trim()
        )
    })?;
    if !(200..300).contains(&status) {
        return Err(format!("Fastly API GET {path} returned HTTP {status}"));
    }
    Ok(body.to_owned())
}

/// `PUT https://api.fastly.com<path>` with the `Fastly-Key` header;
/// returns the HTTP status, erroring on non-2xx. Fastly's version
/// activate/deactivate endpoints require `PUT` (not `POST`). Header and
/// URL are escaped via `curl_quote`; the literal `request`, `output`,
/// and `write-out` directives are fixed constants.
fn fastly_api_put(path: &str, token: &str) -> Result<u16, String> {
    let header = curl_quote(&format!("Fastly-Key: {token}"));
    let url = curl_quote(&format!("https://api.fastly.com{path}"));
    let config = format!(
        "request = \"PUT\"\nheader = {header}\nurl = {url}\noutput = \"/dev/null\"\nwrite-out = \"%{{http_code}}\"\n"
    );
    let out = curl_config_capture(&config)?;
    let code: u16 = out.trim().parse().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            out.trim()
        )
    })?;
    if (200..300).contains(&code) {
        Ok(code)
    } else {
        Err(format!("Fastly API PUT {path} returned HTTP {code}"))
    }
}

/// Resolve the directory containing the Fastly manifest for a deploy
/// (production [`deploy`] or [`deploy_staged`]).
///
/// The CLI (`edgezero_cli::run_deploy`) resolves the `edgezero.toml`
/// manifest — honouring `EDGEZERO_MANIFEST` — and threads the
/// manifest-configured `[adapters.fastly.adapter].manifest` path in as
/// `--manifest-path <abs fastly.toml>`. Prefer that so a monorepo with
/// multiple Fastly apps deploys/stages the app the operator actually
/// selected, rather than whichever `fastly.toml` a bare working-directory
/// search happens to find first. Only when no `--manifest-path` is
/// threaded (e.g. a manifest that declares Fastly commands but no adapter
/// `manifest` key) do we fall back to the working-directory search.
fn resolve_manifest_dir(args: &[String]) -> Result<PathBuf, String> {
    if let Some(raw) = arg_value(args, "--manifest-path") {
        let path = PathBuf::from(raw);
        return path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("fastly manifest path {raw:?} has no parent directory"));
    }
    let manifest =
        find_fastly_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "fastly manifest has no parent directory".to_owned())
}

/// `deploy --adapter fastly --service-id <id> --stage`:
/// build, upload to a new draft version (no activation), stage it, and
/// emit `version=<N>`.
fn deploy_staged(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    // The Fastly CLI reads FASTLY_API_TOKEN from the env; fail fast
    // with a clear message when it's missing rather than deep in a
    // `fastly compute update` error.
    require_token()?;

    let manifest_dir_buf = resolve_manifest_dir(args)?;
    let manifest_dir = manifest_dir_buf.as_path();
    // The CLI threads the app's declared config-store logical ids as
    // `--edgezero-staging-config=<logical>` (one per store) so the staging relink
    // knows which selectors to redirect — read from the app manifest, never a
    // remote probe. These are EdgeZero-internal inline tokens; strip them so they
    // never reach `fastly compute update`.
    let config_logical_ids: Vec<String> = args
        .iter()
        .filter_map(|arg| {
            arg.strip_prefix("--edgezero-staging-config=")
                .map(str::to_owned)
        })
        .collect();
    let deploy_args: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--edgezero-staging-config="))
        .cloned()
        .collect();
    // Strip both the explicitly-threaded `--service-id` and the
    // CLI-injected `--manifest-path` (which `fastly compute update`
    // doesn't understand), then keep only the passthrough flags
    // `compute update` actually supports. `--comment` in particular is
    // NOT a `compute update` flag — it is lifted out here and applied to
    // the version below.
    let extra = args_without_flag_value(
        &args_without_flag_value(&deploy_args, "--service-id"),
        "--manifest-path",
    );
    let passthrough = split_staged_passthrough(&extra);
    if !passthrough.dropped.is_empty() {
        log::warn!(
            "[edgezero] ignoring deploy args not supported by `fastly compute update`: {}",
            passthrough.dropped.join(" ")
        );
    }

    // 1. Build the wasm package (no deploy / activation).
    run_fastly_status(
        &[
            "compute".to_owned(),
            "build".to_owned(),
            "--non-interactive".to_owned(),
        ],
        manifest_dir,
    )?;

    // 2. Clone the active version into a new draft and upload the
    //    package to it — `--autoclone` + `--version=active` keeps
    //    production traffic on the currently-active version.
    let mut update = vec![
        "compute".to_owned(),
        "update".to_owned(),
        "--autoclone".to_owned(),
        format!("--service-id={service_id}"),
        "--version=active".to_owned(),
    ];
    update.extend(passthrough.forwarded.iter().cloned());
    if !has_non_interactive(&passthrough.forwarded) {
        update.push("--non-interactive".to_owned());
    }
    let update_out = run_fastly_capture(&update, manifest_dir)?;

    // Resolve the new draft version from the update output. FAIL CLOSED:
    // if the version cannot be parsed with confidence we return an error
    // rather than guessing. The old fallback picked the service's
    // HIGHEST version, which under concurrent deploys could silently
    // stage/roll back a version created by someone else's run.
    let version = parse_fastly_version(&update_out).ok_or_else(|| {
        format!(
            "could not determine the staged version from `fastly compute update` output; \
             refusing to guess (a wrong version would stage another deploy's changes). \
             Raw output:\n{update_out}"
        )
    })?;

    // 3. Apply the operator's `--comment` to the freshly-created draft.
    //    `compute update` has no `--comment`; the version comment is set
    //    with `service-version update`. Done BEFORE staging, while the
    //    version is still an editable draft (and without `--autoclone`,
    //    so it can never clone into yet another version).
    if let Some(comment) = passthrough.comment.as_deref() {
        run_fastly_status(
            &[
                "service-version".to_owned(),
                "update".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                "--comment".to_owned(),
                comment.to_owned(),
            ],
            manifest_dir,
        )?;
    }

    // 4. Point the draft's runtime-override link at the STAGING selector store,
    //    so this version reads staged config and production keeps reading its
    //    own. Done while the version is still an editable draft.
    relink_runtime_env_for_staging(&service_id, version, &config_logical_ids, manifest_dir)?;

    // 5. Mark the draft version staged (no activation).
    run_fastly_status(
        &[
            "service-version".to_owned(),
            "stage".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
        ],
        manifest_dir,
    )?;

    // 6. Emit the staged version (parseable contract).
    log::info!("version={version}");
    Ok(())
}

/// Point a staged draft's `edgezero_runtime_env` link at the STAGING selector
/// store, so the staged version reads staged config.
///
/// Why this exists: `compute update --autoclone --version=active` clones the
/// active version, and a clone inherits its resource links. Without this, a
/// staged version opens the SAME `edgezero_runtime_env` store as production and
/// therefore reads production's config key — `config push --staging` would write
/// `<key>_staging` that nothing ever reads. Flipping the shared store's selector
/// instead is worse: it redirects production too.
///
/// Fastly resource links are per-version and their `name` is an overridable
/// alias, so linking the staging store under the name `edgezero_runtime_env`
/// gives this draft (and only this draft) staged config.
///
/// Fails closed: if the staging store does not exist we refuse rather than stage
/// a version that would silently serve production config.
fn relink_runtime_env_for_staging(
    service_id: &str,
    version: u64,
    config_logical_ids: &[String],
    manifest_dir: &Path,
) -> Result<(), String> {
    // An app that declares no config stores has no selector to isolate, so
    // staging is still perfectly meaningful for it (staged CODE, no config): the
    // draft keeps the inherited production link and this is a no-op.
    if config_logical_ids.is_empty() {
        log::info!(
            "app declares no config stores, so staged version {version} has no config selector to isolate; keeping the inherited runtime-env link"
        );
        return Ok(());
    }

    // Read the PRODUCTION runtime-override entries to mirror. Fail CLOSED on a
    // lookup FAILURE (CLI missing / non-zero exit / schema drift) — treating
    // "couldn't tell" as "no store" would stage a version that silently reads
    // production config. A genuine `NotFound` is NOT a no-op here: the app
    // DECLARES config (checked above), so the staged version must still be
    // isolated. There is simply nothing to mirror — the twin gets only the
    // derived `<logical>_staging` selectors, and the staged draft is relinked to
    // it so it reads staged config while production keeps its default key.
    let production = match classify_remote_config_store(RUNTIME_ENV_STORE)? {
        ConfigStoreLookup::Found(id) => read_config_store_entries(&id, manifest_dir)?,
        ConfigStoreLookup::NotFound => Vec::new(),
        ConfigStoreLookup::SchemaDrift(detail) => {
            return Err(format!(
                "could not parse `fastly config-store list --json` while resolving `{RUNTIME_ENV_STORE}` for a staged deploy: {detail}.\n  Refusing to stage rather than risk serving PRODUCTION config. Pin a known-compatible fastly CLI version and retry."
            ));
        }
    };

    // Mirror production's runtime overrides into the PER-SERVICE staging twin,
    // overriding only the config selectors to `<logical>_staging`, then point
    // THIS draft at the twin. Create the twin on demand so a staged deploy never
    // depends on a prior provision having created it.
    let staging_store_name = staging_selector_store_name(service_id);
    let staging_store_id = ensure_staging_selector_store(&staging_store_name)?;
    mirror_production_to_staging(
        &production,
        &staging_store_id,
        config_logical_ids,
        manifest_dir,
    )?;

    // Drop the inherited production link first: a version cannot carry two links
    // under the same name.
    let existing = run_fastly_capture(
        &[
            "resource-link".to_owned(),
            "list".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            "--json".to_owned(),
        ],
        manifest_dir,
    )?;
    if let Some(link_id) = find_resource_link_id(&existing, RUNTIME_ENV_STORE) {
        run_fastly_status(
            &[
                "resource-link".to_owned(),
                "delete".to_owned(),
                format!("--service-id={service_id}"),
                format!("--version={version}"),
                format!("--id={link_id}"),
            ],
            manifest_dir,
        )?;
    }

    // `--name` is the alias the runtime opens; the linked STORE is the staging
    // twin. No `--autoclone`: the draft is already editable, and cloning here
    // would silently move us onto yet another version.
    run_fastly_status(
        &[
            "resource-link".to_owned(),
            "create".to_owned(),
            format!("--service-id={service_id}"),
            format!("--version={version}"),
            format!("--resource-id={staging_store_id}"),
            format!("--name={RUNTIME_ENV_STORE}"),
        ],
        manifest_dir,
    )?;

    log::info!("staged version {version} now reads `{staging_store_name}` for its config selector");
    Ok(())
}

/// Production companion to `deploy`: resolve the active service version via the
/// Fastly API and emit it as a `version=<N>` line.
///
/// Distinguishes "confirmed no active version" from an operational failure: a
/// service with no active version yet (a first-ever deploy) is NOT an error — it
/// emits an empty `version=` line and succeeds, so the caller records an empty
/// rollback target. Only a real failure (API/auth error, or a version list that
/// cannot be parsed) returns `Err`, so the caller can fail closed instead of
/// silently proceeding without a rollback target.
///
/// `--require-active` flips the no-active-version case to an error: it is passed
/// by the production-`deploy` version fallback, where a version was JUST
/// activated, so "no active version" is not a valid first-deploy state but an
/// operational failure the CLI must not report as success.
fn emit_active_version(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let token = require_token()?;
    let json = fastly_api_get(&format!("/service/{service_id}/version"), &token)?;
    if let Some(version) =
        active_version_or_require(&json, arg_flag(args, "--require-active"), &service_id)?
    {
        log::info!("version={version}");
    } else {
        // Confirmed no active version (first-ever deploy), and it was not
        // required. Emit an explicit empty line so the caller records an empty
        // rollback target and succeeds — distinct from a failure (`Err`).
        log::info!("version=");
        log::info!(
            "service {service_id} has no active version yet; emitting an empty rollback target"
        );
    }
    Ok(())
}

/// Resolve the active version and apply the `--require-active` policy.
///
/// `Ok(Some(n))` — a version is active. `Ok(None)` — no active version and
/// `require_active` is false (a first-ever `active-version` call; the caller
/// records an empty rollback target). `Err` — the response was malformed
/// ([`resolve_active_version`]), OR no version is active while `require_active`
/// is true. The latter is the production-`deploy` fallback: a version was JUST
/// activated, so "no active version" is an error, not a valid empty result.
fn active_version_or_require(
    json: &str,
    require_active: bool,
    service_id: &str,
) -> Result<Option<u64>, String> {
    match resolve_active_version(json)? {
        Some(version) => Ok(Some(version)),
        None if require_active => Err(format!(
            "the deploy reported success but the Fastly API returns no active version for service {service_id}; refusing to report a deploy with no resolvable version"
        )),
        None => Ok(None),
    }
}

/// `healthcheck --adapter fastly ...`: probe the domain
/// (production) or the version's staging IP (`--staging`), retrying up
/// to `--retry` times. Emits `status-code` / `healthy` and returns
/// `Err` (non-zero exit) when unhealthy after retries.
///
/// `--domain`, `--service-id` and `--version` are REQUIRED and validated
/// on BOTH the production and the staging path. GitHub Actions' `required:
/// true` does not actually fail a workflow when an input is omitted or
/// empty, so this is the real guard: a production healthcheck must never
/// probe on behalf of an absent/empty version it never verified — the
/// caller chains that same version into rollback.
fn healthcheck(args: &[String]) -> Result<(), String> {
    let domain =
        arg_value(args, "--domain").ok_or_else(|| "healthcheck requires --domain".to_owned())?;
    validate_domain(domain)?;
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "healthcheck requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let retry = arg_value(args, "--retry")
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_u32);
    let retry_delay = arg_value(args, "--retry-delay")
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_u64);
    let timeout = arg_value(args, "--timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_u64);

    let staging_ip = if arg_flag(args, "--staging") {
        let token = require_token()?;
        let json = fastly_api_get(
            &format!("/service/{service_id}/version/{version}/domain?include=staging_ips"),
            &token,
        )?;
        Some(parse_staging_ip(&json).ok_or_else(|| {
            format!("no staging IP found for service {service_id} version {version}")
        })?)
    } else {
        None
    };

    let curl_args = build_curl_probe_args(domain, staging_ip.as_deref(), timeout);
    let delay = Duration::from_secs(retry_delay);
    let outcome = probe_with_retries(retry, || curl_status(&curl_args), || thread::sleep(delay));
    match outcome {
        Ok(code) => {
            log::info!("status-code={code}");
            log::info!("healthy=true");
            Ok(())
        }
        Err((last_code, msg)) => {
            if let Some(code) = last_code {
                log::info!("status-code={code}");
            }
            log::info!("healthy=false");
            Err(format!(
                "healthcheck for {domain} failed after {} attempt(s): {msg}",
                retry.max(1)
            ))
        }
    }
}

/// Run a single `curl` health probe, returning the HTTP status. A
/// transport failure (timeout, DNS, refused) surfaces as `Err` so the
/// retry loop treats it as an unhealthy attempt.
fn curl_status(args: &[String]) -> Result<u16, String> {
    let output = Command::new("curl").args(args).output().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            "`curl` not found on PATH; install curl and retry".to_owned()
        } else {
            format!("failed to spawn `curl`: {err}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "curl transport failure (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u16>().map_err(|err| {
        format!(
            "could not parse HTTP status from curl output {:?}: {err}",
            stdout.trim()
        )
    })
}

/// `rollback --adapter fastly ...`: production activates
/// `<version> - 1`; staging deactivates `<version>`.
fn rollback(args: &[String]) -> Result<(), String> {
    let service_id = resolve_service_id(args)?;
    validate_service_id(&service_id)?;
    let version_str =
        arg_value(args, "--version").ok_or_else(|| "rollback requires --version".to_owned())?;
    let version = validate_version_str(version_str)?;
    let token = require_token()?;

    if arg_flag(args, "--staging") {
        // Staging rollback deactivates the STAGED version on the
        // `staging` environment. Fastly's environment-scoped
        // deactivate is `PUT .../deactivate/staging` (a plain
        // `.../deactivate` would target the production activation).
        fastly_api_put(
            &format!("/service/{service_id}/version/{version}/deactivate/staging"),
            &token,
        )?;
        log::info!(
            "[edgezero] deactivated staged version {version} on Fastly service {service_id}"
        );
    } else {
        // Production rollback re-activates an EXPLICIT target. Fastly's version
        // list has no field distinguishing a previously-live version from a
        // staged one (`staging`/`deployed` are documented "Unused"; `locked`
        // only means "not editable"), so the target cannot be inferred — it is
        // captured before the superseding deploy and passed in as --rollback-to.
        let previous = arg_value(args, "--rollback-to")
            .and_then(|raw| validate_version_str(raw).ok())
            .ok_or_else(|| {
                "production rollback requires a valid --rollback-to version".to_owned()
            })?;
        // Fastly's activate endpoint requires `PUT` (not `POST`).
        fastly_api_put(
            &format!("/service/{service_id}/version/{previous}/activate"),
            &token,
        )?;
        log::info!("rolled-back-to={previous}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::read_package_name;
    #[cfg(unix)]
    use edgezero_core::test_env::{EnvOverride, PathPrepend};

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

    // `PathPrepend` (RAII $PATH guard) is the shared helper imported above from
    // `edgezero_core::test_env`; the merge with edition-2024 main replaced our
    // local copy with it (its `set_var` calls are wrapped for 2024's unsafe-env).

    // ── Fastly staging lifecycle helpers ──────────────────────────────

    #[test]
    fn arg_value_reads_flag_value() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--version".to_owned(),
            "42".to_owned(),
        ];
        assert_eq!(arg_value(&args, "--service-id"), Some("SVC1"));
        assert_eq!(arg_value(&args, "--version"), Some("42"));
        assert_eq!(arg_value(&args, "--missing"), None);
    }

    #[test]
    fn arg_value_none_when_flag_is_last() {
        let args = vec!["--version".to_owned()];
        assert_eq!(arg_value(&args, "--version"), None);
    }

    #[test]
    fn arg_flag_detects_presence() {
        let args = vec!["--staging".to_owned()];
        assert!(arg_flag(&args, "--staging"));
        assert!(!arg_flag(&args, "--nope"));
    }

    #[test]
    fn args_without_flag_value_strips_pair() {
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--comment".to_owned(),
            "ci".to_owned(),
        ];
        assert_eq!(
            args_without_flag_value(&args, "--service-id"),
            vec!["--comment".to_owned(), "ci".to_owned()]
        );
    }

    #[test]
    fn resolve_manifest_dir_prefers_manifest_path_flag() {
        // When the CLI threads `--manifest-path <abs fastly.toml>`, the
        // deploy (production AND staged) must use its parent directory
        // rather than a bare working-directory search (which in a
        // monorepo could pick a different app's fastly.toml).
        let args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            "/repo/apps/edge/fastly.toml".to_owned(),
        ];
        let dir = resolve_manifest_dir(&args).expect("resolves from --manifest-path");
        assert_eq!(dir, PathBuf::from("/repo/apps/edge"));
    }

    #[test]
    fn resolve_service_id_prefers_flag() {
        let args = vec!["--service-id".to_owned(), "SVC_FROM_ARG".to_owned()];
        assert_eq!(resolve_service_id(&args).unwrap(), "SVC_FROM_ARG");
    }

    // ── `compute update` passthrough filtering (`--comment`) ─────────

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn split_staged_passthrough_lifts_comment_out_of_compute_update() {
        // `fastly compute update` has NO `--comment` flag (verified against
        // `fastly compute update --help`, CLI v15) — forwarding it makes the
        // command exit non-zero and fails the whole staged deploy. It must be
        // lifted out and applied via `service-version update` instead.
        for args in [owned(&["--comment", "ci run 12"]), owned(&["--comment=x"])] {
            let split = split_staged_passthrough(&args);
            assert!(
                !split
                    .forwarded
                    .iter()
                    .any(|arg| arg.starts_with("--comment")),
                "--comment must never reach `compute update`: {:?}",
                split.forwarded
            );
            assert!(
                split.comment.is_some(),
                "comment must be captured: {args:?}"
            );
        }
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment", "ci run 12"])).comment,
            Some("ci run 12".to_owned())
        );
        assert_eq!(
            split_staged_passthrough(&owned(&["--comment=x"])).comment,
            Some("x".to_owned())
        );
    }

    #[test]
    fn split_staged_passthrough_forwards_supported_flags_only() {
        let args = owned(&[
            "--package",
            "pkg.tar.gz",
            "--autoclone",
            "--verbose",
            "--comment",
            "note",
            "--env",
            "stage",
            "--status-check-off",
        ]);
        let split = split_staged_passthrough(&args);
        // Supported by `compute update`: kept (value flags keep their value).
        assert_eq!(
            split.forwarded,
            owned(&["--package", "pkg.tar.gz", "--autoclone", "--verbose"])
        );
        // `--env`/`--status-check-off` are `compute deploy` flags, not
        // `compute update` ones: dropped, and `--env`'s detached value
        // `stage` is dropped with it (never left as a bogus positional).
        assert_eq!(split.dropped, owned(&["--env", "--status-check-off"]));
        assert!(!split.forwarded.iter().any(|arg| arg == "stage"));
        assert_eq!(split.comment, Some("note".to_owned()));
    }

    // ── non-interactive CI safety (`--non-interactive`) ───────────────

    #[test]
    fn build_compute_deploy_args_is_non_interactive() {
        // Without this a production deploy can block on an interactive
        // prompt in CI.
        let argv = build_compute_deploy_args(&owned(&["--service-id", "SVC1"]));
        assert_eq!(
            argv,
            owned(&[
                "compute",
                "deploy",
                "--service-id",
                "SVC1",
                "--non-interactive"
            ])
        );
    }

    #[test]
    fn build_compute_deploy_args_does_not_duplicate_caller_flag() {
        for flag in ["--non-interactive", "-i"] {
            let argv = build_compute_deploy_args(&owned(&[flag]));
            assert_eq!(
                argv.iter()
                    .filter(|arg| *arg == "--non-interactive" || *arg == "-i")
                    .count(),
                1,
                "must not pass the non-interactive switch twice ({flag})"
            );
        }
    }

    // ── healthcheck / rollback input validation ───────────────────────
    //
    // GitHub Actions' `required: true` does NOT fail when an input is
    // omitted or empty, so the CLI is the real guard. An absent / empty /
    // malformed `--service-id` or `--version` must be rejected on BOTH
    // the production and the staging path — a production healthcheck
    // that probes anyway "verifies" a version it never looked at, and
    // the caller chains that same version into rollback.

    #[test]
    fn healthcheck_rejects_missing_or_empty_required_values_on_production() {
        for (args, needle) in [
            (
                owned(&["--domain", "example.com", "--service-id", "SVC1"]),
                "--version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "SVC1",
                    "--version",
                    "15.2.0",
                ]),
                "invalid version",
            ),
            (
                owned(&[
                    "--domain",
                    "example.com",
                    "--service-id",
                    "",
                    "--version",
                    "7",
                ]),
                "invalid service id",
            ),
            (
                owned(&["--domain", "", "--service-id", "SVC1", "--version", "7"]),
                "invalid domain",
            ),
            (
                owned(&["--service-id", "SVC1", "--version", "7"]),
                "--domain",
            ),
        ] {
            let err = healthcheck(&args).expect_err("must reject absent/empty required value");
            assert!(
                err.contains(needle),
                "expected {needle:?} in error for {args:?}, got: {err}"
            );
        }
    }

    #[test]
    fn healthcheck_rejects_empty_required_values_on_staging() {
        for args in [
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "",
                "--version",
                "7",
            ]),
            owned(&[
                "--staging",
                "--domain",
                "example.com",
                "--service-id",
                "SVC1",
                "--version",
                "",
            ]),
        ] {
            healthcheck(&args).expect_err("staging must reject empty required values");
        }
    }

    #[test]
    fn rollback_rejects_missing_or_invalid_required_values() {
        for staging in [&[][..], &["--staging".to_owned()][..]] {
            for bad in [
                owned(&["--service-id", "SVC1"]),
                owned(&["--service-id", "SVC1", "--version", ""]),
                owned(&["--service-id", "SVC1", "--version", "12abc"]),
                owned(&["--service-id", "", "--version", "7"]),
            ] {
                let mut args = bad.clone();
                args.extend_from_slice(staging);
                rollback(&args).expect_err("rollback must reject invalid required values");
            }
        }
    }

    // ── curl-config escaping + input validation (injection defence) ───

    #[test]
    fn curl_quote_escapes_quotes_and_backslashes() {
        assert_eq!(curl_quote("plain"), "\"plain\"");
        assert_eq!(curl_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(curl_quote("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn curl_quote_never_emits_raw_control_characters() {
        // A token carrying a `"` and a newline must not be able to
        // terminate its quoted value and inject a second `url = "..."`
        // directive. The `"` is escaped and the newline is folded to a
        // `\n` escape so NO raw newline reaches the curl config file.
        let token = "tok\"en\nurl = \"https://evil.example\"";
        let quoted = curl_quote(token);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert!(!quoted.contains('\n'), "no raw newline: {quoted}");
        assert!(!quoted.contains('\r'));
        // The only unescaped `"` are the wrapping pair; every interior
        // quote is preceded by a backslash.
        assert_eq!(quoted, "\"tok\\\"en\\nurl = \\\"https://evil.example\\\"\"");
        // A tab folds too.
        assert_eq!(curl_quote("a\tb"), "\"a\\tb\"");
    }

    #[test]
    fn validate_service_id_accepts_opaque_handles() {
        validate_service_id("SU1Z0isxPaozGVKXdv0eY").expect("alphanumeric handle");
        validate_service_id("abc_DEF-123").expect("underscore + dash handle");
    }

    #[test]
    fn validate_service_id_rejects_injection_and_empty() {
        // The canonical attack: a service id that closes the url value
        // and appends a second url directive.
        validate_service_id("abc\nurl = \"http://evil\"").expect_err("newline injection");
        validate_service_id("abc\"def").expect_err("quote");
        validate_service_id("has space").expect_err("space");
        validate_service_id("has/slash").expect_err("slash");
        validate_service_id("").expect_err("empty");
    }

    #[test]
    fn validate_version_str_accepts_integer_rejects_junk() {
        assert_eq!(validate_version_str("42"), Ok(42));
        assert_eq!(validate_version_str("0"), Ok(0));
        validate_version_str("-1").expect_err("negative");
        validate_version_str("4.2").expect_err("float");
        validate_version_str("42\nurl = \"x\"").expect_err("newline injection");
        validate_version_str("").expect_err("empty");
    }

    #[test]
    fn validate_domain_accepts_hostnames_rejects_injection() {
        validate_domain("example.com").expect("bare hostname");
        validate_domain("staging.example.co.uk").expect("multi-label hostname");
        validate_domain("host-1.example.com").expect("hostname with dash");
        validate_domain("").expect_err("empty");
        validate_domain(".example.com").expect_err("leading dot");
        validate_domain("example.com.").expect_err("trailing dot");
        validate_domain("exa..mple.com").expect_err("empty label");
        validate_domain("example.com/evil").expect_err("slash");
        validate_domain("example.com\nurl = \"x\"").expect_err("newline injection");
        validate_domain("has space.com").expect_err("space");
    }

    #[test]
    fn is_healthy_status_covers_2xx_3xx() {
        assert!(is_healthy_status(200));
        assert!(is_healthy_status(204));
        assert!(is_healthy_status(301));
        assert!(is_healthy_status(399));
        assert!(!is_healthy_status(400));
        assert!(!is_healthy_status(500));
        assert!(!is_healthy_status(199));
    }

    #[test]
    fn parse_fastly_version_handles_the_shapes_fastly_emits() {
        // The Fastly CLI's own success lines. Go format strings:
        //   "Updated package (service %s, version %v)"  (compute update)
        //   "Deployed package (service %s, version %v)" (compute deploy)
        assert_eq!(
            parse_fastly_version("SUCCESS: Deployed package (service abc, version 7)"),
            Some(7)
        );
        assert_eq!(
            parse_fastly_version("\nSUCCESS: Updated package (service SU1Z0, version 42)\n"),
            Some(42)
        );
        // Our canonical contract line.
        assert_eq!(parse_fastly_version("version=12"), Some(12));
        // The --autoclone notice, when no success line is present.
        assert_eq!(
            parse_fastly_version(
                "Service version 3 is not editable, so it was automatically cloned because \
                 --autoclone is enabled. Now operating on version 4."
            ),
            Some(4)
        );
        // Full autoclone + success output: the SUCCESS line wins, and the
        // PRE-clone version (3) never does — even though stdout/stderr are
        // concatenated and their relative order is not guaranteed.
        let combined = "SUCCESS: \nUpdated package (service abc, version 4)\n\
             Service version 3 is not editable, so it was automatically cloned. \
             Now operating on version 4.";
        assert_eq!(parse_fastly_version(combined), Some(4));
        assert_eq!(parse_fastly_version("no numbers here"), None);
    }

    #[test]
    fn parse_fastly_version_rejects_confusable_lines() {
        // The old parser took ANY digits after the word "version", so each
        // of these silently produced a WRONG service version. They must now
        // all be `None`, which makes `deploy_staged` fail closed.
        assert_eq!(
            parse_fastly_version("Uploaded package to service 12345, version unchanged"),
            None
        );
        // The CLI's own semver must not be mistaken for a service version.
        assert_eq!(parse_fastly_version("Fastly CLI version 15.2.0"), None);
        assert_eq!(
            parse_fastly_version("Checking version compatibility for service 99"),
            None
        );
        // A bare `version <N>` mention with no success-line context is not
        // trusted either.
        assert_eq!(parse_fastly_version("cloning version 3"), None);
        // `--version=active` echoed in a command line is not a contract line.
        assert_eq!(
            parse_fastly_version("running: fastly compute update --version=active"),
            None
        );
    }

    #[test]
    fn parse_active_version_finds_active_entry() {
        let json = r#"[
            {"number": 1, "active": false},
            {"number": 2, "active": true},
            {"number": 3, "active": false}
        ]"#;
        assert_eq!(resolve_active_version(json), Ok(Some(2)));
    }

    #[test]
    fn parse_active_version_none_when_no_active() {
        // A parsed list with no active version is `Ok(None)` — confirmed
        // no active version (first deploy), NOT an operational failure.
        let json = r#"[{"number": 1, "active": false}]"#;
        assert_eq!(resolve_active_version(json), Ok(None));
    }

    #[test]
    fn resolve_active_version_errors_on_unparseable_payload() {
        // A truncated / non-array body is an operational failure, distinct from
        // "no active version" — the caller must fail closed, not record empty.
        resolve_active_version("not json").expect_err("non-JSON must be an operational error");
        resolve_active_version(r#"{"error":"unauthorized"}"#)
            .expect_err("a non-array body must be an operational error");
    }

    #[test]
    fn resolve_active_version_errors_on_malformed_active_entries() {
        // A garbled ACTIVE entry must fail closed, not read as "no active
        // version" — otherwise a production deploy proceeds with no rollback
        // target. Each of these is malformed and must be an operational error.
        resolve_active_version(r#"[{"active":true}]"#)
            .expect_err("active entry with no `number` must error");
        resolve_active_version(r#"[{"active":true,"number":"7"}]"#)
            .expect_err("active entry with a string `number` must error");
        resolve_active_version(r#"[{"active":"true","number":7}]"#)
            .expect_err("a non-boolean `active` must error");
        // A non-boolean `active` ANYWHERE is schema drift — the whole list is
        // scanned, so it is caught even AFTER a valid active entry (a naive
        // first-match parser would miss this one).
        resolve_active_version(r#"[{"active":"false"},{"active":true,"number":9}]"#)
            .expect_err("a non-boolean `active` before the active entry is schema drift");
        resolve_active_version(r#"[{"active":true,"number":9},{"active":"nope"}]"#)
            .expect_err("a non-boolean `active` AFTER the active entry is still schema drift");
        // More than one active version is ambiguous — refuse rather than pick one.
        resolve_active_version(r#"[{"active":true,"number":9},{"active":true,"number":10}]"#)
            .expect_err("two active versions must error as ambiguous");
        // EVERY element must be a version object with a numeric `number` — a
        // garbled entry must fail closed, not be skipped as "not active".
        resolve_active_version("[null]").expect_err("a null element must error");
        resolve_active_version("[{}]").expect_err("an entry with no `number` must error");
        resolve_active_version(r#"[{"number":"invalid"}]"#)
            .expect_err("a non-numeric `number` must error");
        // An omitted `active` field means "not active" (not an error), as long
        // as the entry is otherwise a well-formed version object.
        assert_eq!(resolve_active_version(r#"[{"number":42}]"#), Ok(None));
        // Sanity: a well-formed list still resolves.
        assert_eq!(
            resolve_active_version(r#"[{"active":false,"number":1},{"active":true,"number":2}]"#),
            Ok(Some(2))
        );
    }

    #[test]
    fn active_version_or_require_enforces_require_active() {
        let active = r#"[{"active":true,"number":5}]"#;
        let none = r#"[{"active":false,"number":5}]"#;

        // A resolvable active version is returned regardless of the flag.
        assert_eq!(active_version_or_require(active, false, "svc"), Ok(Some(5)));
        assert_eq!(active_version_or_require(active, true, "svc"), Ok(Some(5)));

        // No active version: tolerated for `active-version` (first deploy), but an
        // ERROR for the production-deploy fallback (`--require-active`), which
        // must never report a deploy with no resolvable version.
        assert_eq!(active_version_or_require(none, false, "svc"), Ok(None));
        active_version_or_require(none, true, "svc")
            .expect_err("require-active with no active version must fail closed");

        // A malformed response is an error either way.
        active_version_or_require("not json", false, "svc").expect_err("malformed must error");
    }

    #[test]
    fn parse_staging_ip_reads_the_singular_staging_ip_field() {
        // The REAL Fastly response shape for
        // `GET /service/<id>/version/<n>/domain?include=staging_ips`:
        // an array of domain objects, each with a SINGULAR `staging_ip`
        // STRING. Body copied from go-fastly's recorded API fixture
        // `fastly/fixtures/domains/list_with_staging_ips.yaml`, matching
        // its `StagingIP *string `mapstructure:"staging_ip"`` field.
        // (`staging_ips` is only the `include=` query value, never a
        // field name — the previous parser looked for it as an array and
        // therefore NEVER found a staging IP.)
        let json = r#"[
            {
                "created_at": "2022-11-04T17:36:56Z",
                "service_id": "kKJb5bOFI47uHeBVluGfX1",
                "name": "integ-test-20221104.go-fastly-1.com",
                "version": 73,
                "comment": "comment",
                "deleted_at": null,
                "staging_ip": "167.82.81.194"
            }
        ]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("167.82.81.194"));
    }

    #[test]
    fn parse_staging_ip_tolerates_a_plural_array_shape() {
        let json = r#"[{"name": "example.com", "staging_ips": ["151.101.2.10"]}]"#;
        assert_eq!(parse_staging_ip(json).as_deref(), Some("151.101.2.10"));
    }

    #[test]
    fn parse_staging_ip_none_when_absent_or_null() {
        assert_eq!(parse_staging_ip(r#"[{"name": "example.com"}]"#), None);
        // `staging_ip` is nullable for services without staging enabled.
        assert_eq!(
            parse_staging_ip(r#"[{"name": "example.com", "staging_ip": null}]"#),
            None
        );
    }

    #[test]
    fn build_curl_probe_args_production_has_no_connect_to() {
        let args = build_curl_probe_args("example.com", None, 10);
        assert!(!args.iter().any(|arg| arg == "--connect-to"));
        assert!(args.contains(&"https://example.com/".to_owned()));
        assert!(args.contains(&"--max-time".to_owned()));
        assert!(args.contains(&"10".to_owned()));
    }

    #[test]
    fn build_curl_probe_args_staging_reroutes_to_ip() {
        let args = build_curl_probe_args("staging.example.com", Some("151.101.2.10"), 15);
        let idx = args
            .iter()
            .position(|arg| arg == "--connect-to")
            .expect("--connect-to present for staging");
        assert_eq!(args[idx + 1], "::151.101.2.10:443");
        assert!(args.contains(&"https://staging.example.com/".to_owned()));
    }

    #[test]
    fn probe_with_retries_returns_first_healthy() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                Ok(200)
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 1_i32, "should stop after first healthy probe");
        assert_eq!(between, 0_i32, "no delay before the first attempt");
    }

    #[test]
    fn probe_with_retries_succeeds_after_unhealthy_attempts() {
        let mut calls: i32 = 0;
        let mut between: i32 = 0;
        let result = probe_with_retries(
            5,
            || {
                calls += 1_i32;
                if calls < 3_i32 { Ok(503) } else { Ok(200) }
            },
            || between += 1_i32,
        );
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 3_i32);
        assert_eq!(
            between, 2_i32,
            "delay runs between each of the first 3 attempts"
        );
    }

    #[test]
    fn probe_with_retries_exhausts_and_reports_last_code() {
        let mut between: i32 = 0;
        let result = probe_with_retries(3, || Ok(500), || between += 1_i32);
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(
            between, 2_i32,
            "delay runs between attempts, not after the last"
        );
    }

    #[test]
    fn probe_with_retries_reports_transport_error() {
        let result: Result<u16, (Option<u16>, String)> =
            probe_with_retries(1, || Err("connection refused".to_owned()), || {});
        assert_eq!(result, Err((None, "connection refused".to_owned())));
    }

    #[test]
    fn probe_with_retries_treats_zero_retry_as_one_attempt() {
        let mut calls: i32 = 0;
        let result = probe_with_retries(
            0,
            || {
                calls += 1_i32;
                Ok(500)
            },
            || {},
        );
        assert_eq!(
            result,
            Err((Some(500), "unhealthy HTTP status 500".to_owned()))
        );
        assert_eq!(calls, 1_i32);
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
        // `setup_block_present` only
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
        // No `[local_server.*]` write — that empty stanza
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
        // 1 KV + 1 config + 1 secret + runtime-env = 4 status lines. The staging
        // twin is created and populated by a staged deploy, NOT by provision, so
        // it does not appear here.
        assert_eq!(out.len(), 4, "dry-run rows: {out:?}");
        assert!(out[0].contains("would run `fastly kv-store create --name=sessions`"));
        assert!(out[1].contains("would run `fastly config-store create --name=app_config`"));
        assert!(out[2].contains("would run `fastly secret-store create --name=default`"));
        assert!(
            out[3].contains("would run `fastly config-store create --name=edgezero_runtime_env`"),
            "runtime-env store row: {out:?}",
        );
        assert!(
            !out.iter()
                .any(|row| row.contains("edgezero_runtime_env_staging")),
            "provision must NOT create the staging twin (a staged deploy owns it): {out:?}",
        );
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
        // Pre-populate the runtime-env block so the provision flow's
        // unconditional runtime-env step skips (otherwise it would
        // shell out to real `fastly` to create the store).
        fs::write(
            &path,
            "name = \"demo\"\n[setup.config_stores.edgezero_runtime_env]\n",
        )
        .expect("write");
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
            "[setup.kv_stores.sessions]\n[local_server.kv_stores.sessions]\n\
             [setup.config_stores.edgezero_runtime_env]\n",
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

    /// When `fastly.toml` declares `service_id`, the next
    /// `fastly compute deploy` skips `[setup]` entirely. provision
    /// must emit the `fastly resource-link create` remediation for
    /// every store it creates -- including the implicit
    /// `edgezero_runtime_env` store the runtime override path
    /// depends on. Without this, a freshly-provisioned override
    /// store would not be linked to the already-deployed service
    /// and the runtime would silently fall back to baked defaults.
    #[test]
    fn provision_emits_resource_link_note_for_runtime_env_on_existing_service() {
        // Dry-run only -- we just want to drive the resource_link_note
        // helper for the runtime-env store branch. The real-create
        // path can't run in tests (would shell out to `fastly`).
        // The dry-run output line for runtime-env doesn't include the
        // note (the helper only fires on real create), so we test the
        // helper directly here.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\nservice_id = \"abc123svc\"\n").expect("write");
        let note = resource_link_note(&path, "config", "edgezero_runtime_env")
            .expect("read service_id")
            .expect("note present when service_id set");
        assert!(
            note.contains("service_id = \"abc123svc\""),
            "note quotes the service id: {note}"
        );
        assert!(
            note.contains("fastly config-store list --json"),
            "note tells operator how to find the store id: {note}"
        );
        assert!(
            note.contains("name=`edgezero_runtime_env`"),
            "note names the runtime override store: {note}"
        );
        assert!(
            note.contains(
                "fastly resource-link create --service-id=abc123svc --resource-id=<STORE-ID> --version=latest --autoclone --name=edgezero_runtime_env"
            ),
            "note carries the full resource-link command: {note}"
        );
    }

    /// And the inverse: no `service_id` (a service that hasn't been
    /// deployed yet) means `[setup]` will be applied on the next
    /// `compute deploy`, so no manual resource-link step is needed.
    /// The helper must return `None` to avoid noisy false-positive
    /// guidance.
    #[test]
    fn provision_skips_resource_link_note_when_service_undeployed() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");
        let note =
            resource_link_note(&path, "config", "edgezero_runtime_env").expect("read service_id");
        assert!(
            note.is_none(),
            "no service_id => no resource-link prompt: {note:?}"
        );
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

    #[test]
    fn find_config_store_id_flags_schema_drift_when_any_entry_is_malformed() {
        // A well-formed, non-matching entry must NOT mask a malformed one: the
        // malformed entry could be the store we're looking for (hidden behind a
        // missing name/id). Deciding NotFound here would fail OPEN — staging
        // would then mirror no production overrides. Any malformed entry is drift.
        let stdout = r#"[
            {"id": "abc123", "name": "some_other_store"},
            {"id": "def456"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a malformed entry alongside a well-formed one must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_scans_past_a_match() {
        // The full list is scanned: a malformed entry AFTER the match must still
        // be caught (no short-circuit on the first Found).
        let stdout = r#"[
            {"id": "abc123", "name": "edgezero_runtime_env"},
            {"name": "broken"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a malformed entry after the match must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_flags_duplicate_names_as_ambiguous() {
        let stdout = r#"[
            {"id": "abc123", "name": "edgezero_runtime_env"},
            {"id": "def456", "name": "edgezero_runtime_env"}
        ]"#;
        let drift = find_config_store_id(stdout, "edgezero_runtime_env");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "two stores with the same name must be ambiguous drift, got {drift:?}"
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

    /// Pushing two blobs under different root keys
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

    /// A second oversized push must converge the
    /// runtime on the NEW envelope — chunk keys are content-addressed
    /// by the full-envelope SHA, so push B writes a new chunk-set and
    /// installs a new root pointer.
    ///
    /// The local fastly.toml writer upserts per-key (so a sibling
    /// `--key app_config_staging` push leaves `app_config` intact).
    /// Within the SAME root key, old chunks for envelope
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
        // in v1).
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

    // ── staged deploy: end-to-end argv contract (fake `fastly`) ───────

    /// Fake `fastly` on `$PATH` that appends every invocation's argv (one
    /// space-joined line per call) to a record file, and echoes
    /// `update_stdout` for `fastly compute update`. Returns the temp dir
    /// (which must outlive the test) and the record path.
    #[cfg(unix)]
    fn fake_fastly_recorder(update_stdout: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let script_path = dir.path().join("fastly");
        // Answers every call `deploy_staged` makes. The staging relink needs the
        // selector store to resolve and the inherited link to be listed; without
        // these the staged path fails closed (which is correct, but not what
        // these tests are exercising).
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' '{update_stdout}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               case \"$*\" in\n    \
                 *--store-id=ENVSEL1*) printf '%s\\n' '[{{\"item_key\":\"EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL\",\"item_value\":\"debug\"}}]' ;;\n    \
                 *) printf '%s\\n' '[]' ;;\n  \
               esac\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
        );
        fs::write(&script_path, script).expect("write fake fastly");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        (dir, record)
    }

    /// Run `deploy_staged` against a fake `fastly`, returning the result
    /// and the recorded argv lines.
    #[cfg(unix)]
    fn run_deploy_staged_with_fake(
        update_stdout: &str,
        extra: &[&str],
    ) -> (Result<(), String>, Vec<String>) {
        let _lock = path_mutation_guard().lock().expect("guard");
        let (fake, record) = fake_fastly_recorder(update_stdout);
        let _path = PathPrepend::new(fake.path());
        let app = tempdir().expect("app dir");
        let manifest = app.path().join("fastly.toml");
        fs::write(&manifest, "name = \"app\"\n").expect("write fastly.toml");

        // RAII: set the token for the call, restore it on drop. Uses the shared
        // guard (edition-2024 wraps the env mutation's `unsafe` and holds the
        // lock we already took above).
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");
        let mut args = vec![
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            manifest.display().to_string(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        let result = deploy_staged(&args);

        let recorded = fs::read_to_string(&record).unwrap_or_default();
        let lines = recorded.lines().map(str::to_owned).collect();
        (result, lines)
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_routes_comment_to_service_version_update() {
        // `--comment` is allowlisted for `deploy-args` and recommended by the
        // adoption guide, but `fastly compute update` has no such flag. It
        // must NOT be forwarded there (that would fail the deploy) and must
        // instead land on the version via `service-version update`.
        for comment_args in [vec!["--comment", "ci run 12"], vec!["--comment=ci run 12"]] {
            let (result, argv) = run_deploy_staged_with_fake(
                "SUCCESS: Updated package (service SVC1, version 7)",
                &comment_args,
            );
            result.expect("staged deploy with --comment must succeed");

            let update = argv
                .iter()
                .find(|line| line.starts_with("compute update"))
                .expect("compute update was invoked");
            assert!(
                !update.contains("--comment"),
                "--comment must not be forwarded to `compute update`: {update}"
            );
            assert!(
                update.contains("--non-interactive"),
                "compute update must be non-interactive: {update}"
            );

            let comment_call = argv
                .iter()
                .find(|line| line.starts_with("service-version update"))
                .expect("`service-version update` must apply the version comment");
            assert_eq!(
                comment_call,
                "service-version update --service-id=SVC1 --version=7 --comment ci run 12"
            );

            // The comment lands on the version BEFORE it is staged (while it
            // is still an editable draft).
            let comment_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version update"))
                .expect("comment call");
            let stage_idx = argv
                .iter()
                .position(|line| line.starts_with("service-version stage"))
                .expect("stage call");
            assert!(comment_idx < stage_idx, "comment must precede staging");
            assert_eq!(
                argv[stage_idx],
                "service-version stage --service-id=SVC1 --version=7"
            );
        }
    }

    #[test]
    fn runtime_env_key_matches_what_the_runtime_reads() {
        use edgezero_core::env_config::EnvConfig;

        // EnvConfig::from_vars strips `EDGEZERO__`, splits on `__`, lowercases;
        // store_key("config", id) looks up ["stores","config",id,"key"]. So the
        // entry name is the id uppercased. A near-miss is SILENT: the runtime
        // would fall back to the id and read production config.
        assert_eq!(
            runtime_env_key_for("app_config"),
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
        );

        // Prove it against the real reader rather than restating the format.
        let cfg = EnvConfig::from_vars([(
            runtime_env_key_for("app_config"),
            "app_config_staging".to_owned(),
        )]);
        assert_eq!(
            cfg.store_key("config", "app_config"),
            "app_config_staging",
            "the entry provision writes must be the one the runtime reads"
        );
    }

    #[test]
    fn staging_entries_from_production_mirrors_and_overrides() {
        // Production carries a non-config override, an explicit config selector,
        // and a __NAME redirect. The twin must copy the non-config entries
        // verbatim and redirect EVERY declared config store to `<logical>_staging`
        // — including one production has no explicit entry for (it relies on the
        // runtime default; the twin must NOT inherit that default).
        let production = vec![
            (
                "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL".to_owned(),
                "debug".to_owned(),
            ),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "custom_prod_key".to_owned(),
            ),
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
                "app_config".to_owned(),
            ),
        ];
        let out = staging_entries_from_production(
            &production,
            &["app_config".to_owned(), "feature_flags".to_owned()],
        );

        // Non-selector overrides copied verbatim.
        assert!(out.contains(&(
            "EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL".to_owned(),
            "debug".to_owned()
        )));
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME".to_owned(),
            "app_config".to_owned()
        )));
        // The selector production HAD is overridden to `<logical>_staging`, NOT
        // production's custom value.
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
            "app_config_staging".to_owned()
        )));
        assert!(!out.iter().any(|(_, value)| value == "custom_prod_key"));
        // The declared store production LACKED a selector for still gets one.
        assert!(out.contains(&(
            "EDGEZERO__STORES__CONFIG__FEATURE_FLAGS__KEY".to_owned(),
            "feature_flags_staging".to_owned()
        )));
        // Exactly one entry per selector key (no duplicate from the copy path).
        assert_eq!(
            out.iter()
                .filter(|(key, _)| key == "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY")
                .count(),
            1
        );
    }

    #[test]
    fn find_resource_link_id_matches_on_link_name_not_resource_name() {
        // The link's `name` is an alias defaulting to the resource's name. The
        // staging relink depends on that alias: a store named
        // `edgezero_runtime_env_staging` is linked AS `edgezero_runtime_env`.
        let json = r#"[
            {"id":"LINK_KV","name":"sessions"},
            {"id":"LINK_ENV","name":"edgezero_runtime_env"}
        ]"#;
        assert_eq!(
            find_resource_link_id(json, "edgezero_runtime_env").as_deref(),
            Some("LINK_ENV")
        );
        // Absent link -> nothing to delete, not an error.
        assert_eq!(find_resource_link_id(json, "nope"), None);
        // Tolerates the `{"items": [...]}` envelope, like the store lookup.
        let enveloped = r#"{"items":[{"id":"L1","name":"edgezero_runtime_env"}]}"#;
        assert_eq!(
            find_resource_link_id(enveloped, "edgezero_runtime_env").as_deref(),
            Some("L1")
        );
        assert_eq!(find_resource_link_id("not json", "x"), None);
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_points_the_draft_at_the_staging_selector_store() {
        // The defect this closes: a clone inherits the active version's links,
        // so without a relink the staged version opens production's selector
        // store and reads PRODUCTION config -- `config push --staging` would
        // write a key nothing ever reads. The CLI threads the declared config
        // store as `--edgezero-staging-config=<logical>`.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--edgezero-staging-config=app_config"],
        );
        result.expect("staged deploy must succeed");

        // The twin MIRRORS production: the non-selector override is copied
        // verbatim, and the config selector is upserted (redirected to
        // `app_config_staging` via stdin) into the staging store.
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL"
            )),
            "production's non-config override must be mirrored into the twin: {argv:?}"
        );
        assert!(
            argv.iter().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the config selector must be written into the twin: {argv:?}"
        );
        // The mirror runs while the draft is still editable, before the relink.
        let mirror_idx = argv
            .iter()
            .position(|line| line.starts_with("config-store-entry update --store-id=STAGEID1"))
            .expect("mirror upsert");

        // The inherited production link is dropped: a version cannot hold two
        // links under one name.
        let delete_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link delete"))
            .expect("the inherited runtime-env link must be deleted");
        assert_eq!(
            argv[delete_idx],
            "resource-link delete --service-id=SVC1 --version=7 --id=LINK1"
        );

        // The staging STORE is linked under the name the runtime opens.
        let create_idx = argv
            .iter()
            .position(|line| line.starts_with("resource-link create"))
            .expect("the staging selector store must be linked");
        assert_eq!(
            argv[create_idx],
            "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
        );

        // Order matters: delete before create (name collision), and both while
        // the version is still an editable draft -- i.e. before staging.
        assert!(delete_idx < create_idx, "delete must precede create");
        assert!(
            mirror_idx < delete_idx,
            "the twin must be mirrored before the draft is relinked to it"
        );
        let stage_idx = argv
            .iter()
            .position(|line| line.starts_with("service-version stage"))
            .expect("stage call");
        assert!(
            create_idx < stage_idx,
            "the relink must happen while the version is still a draft"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_works_for_an_app_that_selects_no_config() {
        use std::os::unix::fs::PermissionsExt as _;

        // An app declaring no config stores threads no
        // `--edgezero-staging-config`, so there is no selector to isolate:
        // staging is still meaningful (staged CODE, no config), the draft keeps
        // the inherited link, and no config-store lookup happens at all.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        // No config stores at all on the account.
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' '[]'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
        ])
        .expect("an app with no config selection must still be stageable");
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_auto_creates_the_staging_twin_when_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // A staged deploy owns the twin end to end: if the account has no
        // staging store yet, the deploy creates it (rather than failing), so a
        // provisioned app can stage without a separate setup step.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // Stateful fake: `config-store list` includes the twin ONLY after a
        // `config-store create` has touched the marker.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}},{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[{{\"id\":\"ENVSEL1\",\"name\":\"edgezero_runtime_env\"}}]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[{{\"id\":\"LINK1\",\"name\":\"edgezero_runtime_env\"}}]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("staged deploy must auto-create the twin and succeed");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines()
                .any(|line| line == "config-store create --name=edgezero_runtime_env_staging_SVC1"),
            "the per-service twin must be created on demand: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_isolates_when_config_declared_but_prod_store_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        // The app DECLARES config but has no `edgezero_runtime_env` store (never
        // provisioned an override store — production reads its default key). A
        // staged deploy must NOT silently inherit production config: it creates
        // the per-service twin, writes the `<logical>_staging` selector, and
        // relinks the draft to it. There is nothing to mirror (no production
        // entries), but staging is still isolated.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let marker = dir.path().join("twin-created");
        let script_path = dir.path().join("fastly");
        // No `edgezero_runtime_env` ever; the twin appears only after create.
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{record}'\n\
             if [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  \
               printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"create\" ]; then\n  \
               : > '{marker}'\n\
             elif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  \
               if [ -f '{marker}' ]; then\n    \
                 printf '%s\\n' '[{{\"id\":\"STAGEID1\",\"name\":\"edgezero_runtime_env_staging_SVC1\"}}]'\n  \
               else\n    \
                 printf '%s\\n' '[]'\n  \
               fi\n\
             elif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             elif [ \"$1\" = \"resource-link\" ] && [ \"$2\" = \"list\" ]; then\n  \
               printf '%s\\n' '[]'\n\
             fi\n\
             exit 0\n",
            record = record.display(),
            marker = marker.display(),
        );
        fs::write(&script_path, script).expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect("must isolate staging even with no production override store");

        let argv = fs::read_to_string(&record).unwrap_or_default();
        assert!(
            argv.lines().any(|line| line.starts_with(
                "config-store-entry update --store-id=STAGEID1 --key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"
            )),
            "the staging selector must be written even with no production store: {argv}"
        );
        assert!(
            argv.lines().any(|line| line.starts_with(
                "resource-link create --service-id=SVC1 --version=7 --resource-id=STAGEID1 --name=edgezero_runtime_env"
            )),
            "the draft must be relinked to the staging twin: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_config_store_list_is_unreadable() {
        use std::os::unix::fs::PermissionsExt as _;

        // If the store listing can't be parsed (a CLI schema change), we cannot
        // tell whether production config exists — refuse rather than risk a
        // staged version that silently serves PRODUCTION config.
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("fastly");
        fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"compute\" ] && [ \"$2\" = \"update\" ]; then\n  printf '%s\\n' 'SUCCESS: Updated package (service SVC1, version 7)'\nelif [ \"$1\" = \"config-store\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' 'not json at all'\nfi\nexit 0\n",
        )
        .expect("write fake");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        let _path = PathPrepend::new(dir.path());

        let app = tempdir().expect("app dir");
        fs::write(app.path().join("fastly.toml"), "name = \"app\"\n").expect("write fastly.toml");
        let _token = EnvOverride::set(FASTLY_API_TOKEN_ENV, "test-token");

        let err = deploy_staged(&[
            "--service-id".to_owned(),
            "SVC1".to_owned(),
            "--manifest-path".to_owned(),
            app.path().join("fastly.toml").display().to_string(),
            "--edgezero-staging-config=app_config".to_owned(),
        ])
        .expect_err("an unreadable config-store listing must fail closed");
        assert!(
            err.contains("Refusing to stage") || err.contains("could not parse"),
            "the error must explain the refusal: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_without_comment_makes_no_version_comment_call() {
        let (result, argv) =
            run_deploy_staged_with_fake("SUCCESS: Updated package (service SVC1, version 7)", &[]);
        result.expect("staged deploy must succeed");
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version update")),
            "no comment => no `service-version update` call: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_fails_closed_when_version_is_unparseable() {
        // The old code fell back to the service's HIGHEST version here, which
        // could silently adopt a version created by a CONCURRENT deploy. We
        // must error out instead of guessing.
        let (result, argv) = run_deploy_staged_with_fake("uploaded, but nothing parseable", &[]);
        let err = result.expect_err("unparseable version must fail closed");
        assert!(
            err.contains("could not determine the staged version"),
            "unexpected error: {err}"
        );
        assert!(
            !argv
                .iter()
                .any(|line| line.starts_with("service-version stage")),
            "must not stage a guessed version: {argv:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deploy_staged_does_not_duplicate_non_interactive_from_passthrough() {
        // `--non-interactive` is an allowlisted `compute update` flag, so a
        // caller-supplied one is FORWARDED. We must not then append our own:
        // passing the switch twice makes the Fastly CLI exit non-zero.
        let (result, argv) = run_deploy_staged_with_fake(
            "SUCCESS: Updated package (service SVC1, version 7)",
            &["--non-interactive"],
        );
        result.expect("staged deploy with a passthrough --non-interactive must succeed");
        let update = argv
            .iter()
            .find(|line| line.starts_with("compute update"))
            .expect("compute update was invoked");
        assert_eq!(
            update.matches("--non-interactive").count(),
            1,
            "the non-interactive switch must appear exactly once: {update}"
        );
    }

    /// Fake `fastly` on `$PATH` that records `<cwd>\t<argv>` for every
    /// invocation. Used to prove the production deploy runs in the
    /// manifest-selected app directory.
    #[cfg(unix)]
    fn fake_fastly_cwd_recorder() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().expect("tempdir");
        let record = dir.path().join("argv.log");
        let script_path = dir.path().join("fastly");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\t%s\\n' \"$PWD\" \"$*\" >> '{}'\nexit 0\n",
            record.display(),
        );
        fs::write(&script_path, script).expect("write fake fastly");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        (dir, record)
    }

    #[cfg(unix)]
    #[test]
    fn deploy_honours_threaded_manifest_path_and_strips_it_from_the_fastly_argv() {
        // Production deploys used to ignore the CLI-threaded
        // `--manifest-path` and fall back to `find_fastly_manifest(cwd)`,
        // which in a monorepo picks the CLOSEST fastly.toml — the wrong
        // app. The threaded path must select the app directory, and must
        // be STRIPPED from the argv (`fastly compute deploy` has no such
        // flag and would exit non-zero).
        let _lock = path_mutation_guard().lock().expect("guard");
        let (fake, record) = fake_fastly_cwd_recorder();
        let _path = PathPrepend::new(fake.path());

        let app = tempdir().expect("app dir");
        let manifest = app.path().join("fastly.toml");
        fs::write(&manifest, "name = \"app\"\n").expect("write fastly.toml");

        let args = vec![
            "--manifest-path".to_owned(),
            manifest.display().to_string(),
            "--service-id".to_owned(),
            "SVC1".to_owned(),
        ];
        deploy(&args).expect("deploy must run against the threaded manifest");

        let recorded = fs::read_to_string(&record).expect("fastly was invoked");
        let (cwd, recorded_argv) = recorded
            .trim_end()
            .split_once('\t')
            .expect("recorded `<cwd>\\t<argv>`");
        assert_eq!(
            fs::canonicalize(cwd).expect("cwd"),
            fs::canonicalize(app.path()).expect("app dir"),
            "deploy must run in the manifest-selected app directory"
        );
        assert_eq!(
            recorded_argv,
            "compute deploy --service-id SVC1 --non-interactive"
        );
    }
}
