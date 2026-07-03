use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
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
use edgezero_core::addr;
use edgezero_core::manifest::ManifestLoader;
use toml::Value;
use walkdir::WalkDir;

static AXUM_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "axum_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "axum_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "axum_axum_toml",
        contents: include_str!("templates/axum.toml.hbs"),
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
    AdapterFileSpec {
        template: "axum_axum_toml",
        output: "axum.toml",
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

#[derive(Debug)]
struct AxumProject {
    addr: SocketAddr,
    axum_host: Option<String>,
    axum_manifest: PathBuf,
    axum_port: Option<u16>,
    cargo_manifest: PathBuf,
    crate_dir: PathBuf,
    crate_name: String,
    env_host: Option<String>,
    env_port: Option<String>,
}

#[derive(Debug, Default)]
struct EdgezeroAxumConfig {
    host: Option<String>,
    port: Option<u16>,
}

#[expect(
    clippy::missing_trait_methods,
    reason = "axum has no validate_app_config_keys / validate_adapter_manifest / validate_typed_secrets requirements; those three trait defaults are intentionally inherited. `read_config_entry` delegates to `read_config_entry_local` (axum is local-only). `single_store_kinds` IS overridden below (returns `&[\"secrets\"]`). `provision_typed` IS overridden below (Local mode appends `<key_value>=` secret placeholders to `.edgezero/.env`; Cloud is a no-op — axum has no cloud secret store)."
)]
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
            AdapterAction::Build => build(args),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("axum adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "axum"
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
            ProvisionMode::Local => return provision_local(manifest_root, stores, dry_run),
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
        Ok(ProvisionOutcome {
            status_lines: out,
            deployed: None,
        })
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
        Ok(ProvisionOutcome {
            status_lines,
            deployed: None,
        })
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

fn build(extra_args: &[String]) -> Result<(), String> {
    let project = locate_project()?;
    run_cargo(&project, "build", extra_args)
}

fn serve(extra_args: &[String]) -> Result<(), String> {
    let project = locate_project()?;
    run_cargo(&project, "run", extra_args)
}

fn deploy(_extra_args: &[String]) -> Result<(), String> {
    Err("Axum adapter does not define a deploy command. Extend your workspace manifest with one if needed.".into())
}

fn locate_project() -> Result<AxumProject, String> {
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    let manifest = find_axum_manifest(&cwd)?;
    read_axum_project(&manifest)
}

fn run_cargo(project: &AxumProject, subcommand: &str, extra_args: &[String]) -> Result<(), String> {
    let resolution = resolve_subprocess_addr(project)?;
    for warning in &resolution.warnings {
        log::warn!("[edgezero] {warning}");
    }

    let bind_addr = resolution.addr;
    let display = project.crate_dir.display();
    log::info!(
        "[edgezero] Axum {subcommand} ({}) in {display} ({bind_addr})",
        project.crate_name
    );
    let mut command = Command::new("cargo");
    command.arg(subcommand);
    command.arg("--manifest-path");
    command.arg(
        project
            .cargo_manifest
            .to_str()
            .ok_or_else(|| format!("invalid manifest path {}", project.cargo_manifest.display()))?,
    );
    command.args(extra_args);
    command.current_dir(&project.crate_dir);
    // Canonical env vars. The runtime's `EnvConfig` reads only the
    // `EDGEZERO__*` form (see `crates/edgezero-core/src/env_config.rs`);
    // setting the legacy `EDGEZERO_HOST` / `EDGEZERO_PORT` here would be a
    // no-op for the child process.
    command.env("EDGEZERO__ADAPTER__HOST", bind_addr.ip().to_string());
    command.env("EDGEZERO__ADAPTER__PORT", bind_addr.port().to_string());
    let status = command
        .status()
        .map_err(|err| format!("failed to run cargo {subcommand}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {subcommand} failed with status {status}"))
    }
}

fn resolve_subprocess_addr(project: &AxumProject) -> Result<addr::BindAddrResolution, String> {
    let axum_only = resolve_subprocess_addr_from_parts(
        project.env_host.as_deref(),
        project.env_port.as_deref(),
        None,
        None,
        project.axum_host.as_deref(),
        project.axum_port,
    );
    debug_assert_eq!(
        project.addr, axum_only.addr,
        "cached AxumProject.addr must match a fresh axum-only resolution"
    );

    let edgezero = load_edgezero_axum_config(&project.axum_manifest)?;
    Ok(resolve_subprocess_addr_from_parts(
        project.env_host.as_deref(),
        project.env_port.as_deref(),
        edgezero.as_ref().and_then(|cfg| cfg.host.as_deref()),
        edgezero.as_ref().and_then(|cfg| cfg.port),
        project.axum_host.as_deref(),
        project.axum_port,
    ))
}

fn resolve_subprocess_addr_from_parts(
    env_host: Option<&str>,
    env_port: Option<&str>,
    edgezero_host: Option<&str>,
    edgezero_port: Option<u16>,
    axum_host: Option<&str>,
    axum_port: Option<u16>,
) -> addr::BindAddrResolution {
    let mut warnings = Vec::new();
    let host = resolve_subprocess_host(env_host, edgezero_host, axum_host, &mut warnings);
    let port = resolve_subprocess_port(env_port, edgezero_port, axum_port, &mut warnings);

    addr::BindAddrResolution {
        addr: SocketAddr::from((host, port)),
        warnings,
    }
}

fn resolve_subprocess_host(
    env_host: Option<&str>,
    edgezero_host: Option<&str>,
    axum_host: Option<&str>,
    warnings: &mut Vec<String>,
) -> IpAddr {
    if let Some(value) = env_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "EDGEZERO__ADAPTER__HOST={value:?} is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    if let Some(value) = edgezero_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "configured host={value:?} in edgezero.toml is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    if let Some(value) = axum_host {
        match value.parse() {
            Ok(host) => return host,
            Err(_) => warnings.push(format!(
                "configured host={value:?} in axum.toml is not a valid IP address (hostnames like \"localhost\" are not supported); falling back"
            )),
        }
    }

    addr::DEFAULT_HOST
}

fn resolve_subprocess_port(
    env_port: Option<&str>,
    edgezero_port: Option<u16>,
    axum_port: Option<u16>,
    warnings: &mut Vec<String>,
) -> u16 {
    if let Some(value) = env_port {
        match value.parse::<u16>() {
            Ok(0) => warnings.push(
                "EDGEZERO__ADAPTER__PORT=\"0\" is not supported (would bind to a random OS port); falling back".to_owned(),
            ),
            Ok(port) => return port,
            Err(_) => warnings.push(format!(
                "EDGEZERO__ADAPTER__PORT={value:?} is not a valid port number; falling back"
            )),
        }
    }

    match edgezero_port {
        Some(0) => warnings.push(
            "configured port=0 in edgezero.toml is not supported (would bind to a random OS port); falling back".to_owned(),
        ),
        Some(port) => return port,
        None => {}
    }

    match axum_port {
        Some(0) => warnings.push(
            "configured port=0 in axum.toml is not supported (would bind to a random OS port); falling back".to_owned(),
        ),
        Some(port) => return port,
        None => {}
    }

    addr::DEFAULT_PORT
}

fn load_edgezero_axum_config(axum_manifest: &Path) -> Result<Option<EdgezeroAxumConfig>, String> {
    let Some(start_dir) = axum_manifest.parent() else {
        return Ok(None);
    };

    let Some(manifest_path) = find_manifest_upwards(start_dir, "edgezero.toml") else {
        return Ok(None);
    };

    let manifest = ManifestLoader::from_path(&manifest_path)
        .map_err(|err| format!("failed to load {}: {err}", manifest_path.display()))?;
    let Some(adapter) = manifest.manifest().adapters.get("axum") else {
        return Ok(None);
    };

    Ok(Some(EdgezeroAxumConfig {
        host: adapter.adapter.host.clone(),
        port: adapter.adapter.port,
    }))
}

fn find_axum_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "axum.toml") {
        return Ok(found);
    }

    let root = find_workspace_root(start);
    let mut candidates: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(true)
        .max_depth(8)
        .into_iter()
        .filter_map(Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "axum.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate axum.toml".into());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn read_axum_project(manifest: &Path) -> Result<AxumProject, String> {
    // Per the spec hard-cutoff: only the canonical
    // `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT` env
    // vars are honoured. The pre-rewrite `EDGEZERO_HOST` /
    // `EDGEZERO_PORT` shim is gone -- the core runtime stopped
    // reading those names, and keeping the axum wrapper compatible
    // with them silently revived a precedence path the rest of
    // the codebase had cut. Operators with legacy CI scripts must
    // rename to the canonical form.
    let env_host = env::var("EDGEZERO__ADAPTER__HOST").ok();
    let env_port = env::var("EDGEZERO__ADAPTER__PORT").ok();
    read_axum_project_with_env(manifest, env_host.as_deref(), env_port.as_deref())
}

fn read_axum_project_with_env(
    manifest: &Path,
    env_host: Option<&str>,
    env_port: Option<&str>,
) -> Result<AxumProject, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|err| format!("failed to read {}: {err}", manifest.display()))?;
    let value: Value = toml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.display()))?;

    let adapter = value
        .get("adapter")
        .and_then(Value::as_table)
        .ok_or_else(|| format!("adapter table missing in {}", manifest.display()))?;

    let crate_dir_rel = adapter
        .get("crate_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("adapter.crate_dir missing in {}", manifest.display()))?;

    let manifest_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let crate_dir = manifest_dir.join(crate_dir_rel);
    let cargo_manifest = crate_dir.join("Cargo.toml");
    if !cargo_manifest.exists() {
        return Err(format!(
            "Cargo.toml missing at {} referenced by {}",
            cargo_manifest.display(),
            manifest.display()
        ));
    }

    let crate_name = adapter.get("crate").and_then(Value::as_str).map_or_else(
        || {
            read_package_name(&cargo_manifest).unwrap_or_else(|_| {
                crate_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("axum-adapter")
                    .to_owned()
            })
        },
        ToOwned::to_owned,
    );

    let config_host = adapter
        .get("host")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let config_port = match adapter.get("port").and_then(Value::as_integer) {
        Some(port_value) => Some(u16::try_from(port_value).map_err(|err| {
            format!(
                "adapter.port in {} must be between 0 and 65535 ({err})",
                manifest.display()
            )
        })?),
        None => None,
    };

    let resolution =
        addr::resolve_bind_addr(env_host, env_port, config_host.as_deref(), config_port);
    for warning in &resolution.warnings {
        log::warn!("[edgezero] {warning} (in {})", manifest.display());
    }

    Ok(AxumProject {
        addr: resolution.addr,
        axum_host: config_host,
        axum_manifest: manifest.to_path_buf(),
        axum_port: config_port,
        cargo_manifest,
        crate_dir,
        crate_name,
        env_host: env_host.map(str::to_owned),
        env_port: env_port.map(str::to_owned),
    })
}

/// Local-mode `provision` arm.
///
/// Axum is the odd one out: its adapter manifest (`axum.toml`) stays
/// tracked and operator-owned, so provision must NEVER edit it. The
/// only thing to synthesise is the `.edgezero/.env` file the runtime
/// reads at boot: `__NAME` lines seed the store->platform-name map
/// for every declared kind (KV / CONFIG / SECRETS), and commented
/// `__KEY` placeholders for CONFIG stores let the operator uncomment
/// them to switch to a staging blob without hand-remembering the
/// full env-var name.
///
/// The `.edgezero/` directory anchors at `manifest_root` — Axum has
/// no adapter-specific manifest worth anchoring on (there is one, but
/// it's operator-owned and we've promised not to touch it).
///
/// Dedup — including commented/uncommented cross-form dedup — is
/// delegated to [`append_lines_dedup`] so operator overrides survive
/// re-runs.
fn provision_local(
    manifest_root: &Path,
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    let dot_edgezero = manifest_root.join(".edgezero");
    if !dry_run {
        fs::create_dir_all(&dot_edgezero)
            .map_err(|err| format!("create {}: {err}", dot_edgezero.display()))?;
    }
    let env_path = dot_edgezero.join(".env");
    let env_lines = build_axum_env_lines(stores);
    append_lines_dedup_with_header(
        &env_path,
        Some(EDGEZERO_PROVISION_HEADER),
        &env_lines,
        dry_run,
    )
    .map_err(|err| format!("write {}: {err}", env_path.display()))?;
    let status_lines = vec![format!(
        "axum: ensured {} + appended {} .env lines",
        dot_edgezero.display(),
        env_lines.len()
    )];
    Ok(ProvisionOutcome {
        status_lines,
        deployed: None,
    })
}

/// Build the `.env` line set emitted by [`provision_local`].
///
/// - One `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME=<platform>`
///   line per store, for every kind (KV, CONFIG, SECRETS).
/// - One commented `# EDGEZERO__STORES__CONFIG__<LOGICAL_UPPER>__KEY=<logical>_staging`
///   placeholder per CONFIG store, so the operator can uncomment to
///   switch blobs without remembering the exact env-var name.
///
/// Env-var KEY uses the LOGICAL id upper-cased so the runtime env
/// overlay finds it regardless of a teammate's per-store platform
/// override. Env-var VALUE uses the PLATFORM name so the runtime
/// resolves the same backend the rest of the toolchain (Cloudflare,
/// Fastly, Spin, and here the Axum local file store) points at.
fn build_axum_env_lines(stores: &ProvisionStores<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (kind, kind_stores) in [
        ("KV", stores.kv),
        ("CONFIG", stores.config),
        ("SECRETS", stores.secrets),
    ] {
        for store in kind_stores {
            let logical_upper = store.logical.to_ascii_uppercase();
            let platform = &store.platform;
            lines.push(format!(
                "EDGEZERO__STORES__{kind}__{logical_upper}__NAME={platform}"
            ));
        }
    }
    for store in stores.config {
        let logical_upper = store.logical.to_ascii_uppercase();
        let logical = &store.logical;
        lines.push(format!(
            "# EDGEZERO__STORES__CONFIG__{logical_upper}__KEY={logical}_staging"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_adapter::cli_support::find_manifest_upwards;
    use std::net::Ipv6Addr;
    use tempfile::tempdir;

    #[test]
    fn read_axum_project_loads_defaults() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.crate_name, "demo");
        assert_eq!(project.crate_dir, root);
        assert_eq!(project.cargo_manifest, root.join("Cargo.toml"));
        assert_eq!(project.addr.port(), addr::DEFAULT_PORT);
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
    }

    #[test]
    fn find_manifest_upwards_locates_parent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("nested/level");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        let found = find_manifest_upwards(&nested, "axum.toml").expect("manifest");
        assert_eq!(found, root.join("axum.toml"));
    }

    #[test]
    fn read_axum_project_uses_custom_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 4001\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), 4001);
    }

    #[test]
    fn read_axum_project_rejects_invalid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 70000\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("must be between 0 and 65535")),
        }
    }

    #[test]
    fn read_axum_project_zero_port_falls_back_to_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 0\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), addr::DEFAULT_PORT);
    }

    #[test]
    fn read_axum_project_rejects_negative_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = -1\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        result.unwrap_err();
    }

    #[test]
    fn read_axum_project_rejects_missing_adapter_table() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("axum.toml"), "[other]\nkey = \"value\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("adapter table missing")),
        }
    }

    #[test]
    fn read_axum_project_rejects_missing_crate_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("axum.toml"), "[adapter]\ncrate = \"demo\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("crate_dir missing")),
        }
    }

    #[test]
    fn read_axum_project_rejects_missing_cargo_toml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \"subdir\"\n",
        )
        .unwrap();
        // No Cargo.toml in subdir

        let result = read_axum_project_with_env(&root.join("axum.toml"), None, None);
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => assert!(err.contains("Cargo.toml missing")),
        }
    }

    #[test]
    fn read_axum_project_falls_back_to_package_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No crate key in adapter table
        fs::write(root.join("axum.toml"), "[adapter]\ncrate_dir = \".\"\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"my-package\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.crate_name, "my-package");
    }

    #[test]
    fn read_axum_project_with_relative_crate_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let adapter_dir = root.join("crates/my-adapter");
        fs::create_dir_all(&adapter_dir).unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"my-adapter\"\ncrate_dir = \"crates/my-adapter\"\n",
        )
        .unwrap();
        fs::write(
            adapter_dir.join("Cargo.toml"),
            "[package]\nname = \"my-adapter\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.crate_name, "my-adapter");
        assert_eq!(project.crate_dir, adapter_dir);
    }

    #[test]
    fn read_axum_project_accepts_max_valid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 65535\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), u16::MAX);
    }

    #[test]
    fn read_axum_project_accepts_min_valid_port() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nport = 1\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.port(), 1);
    }

    #[test]
    fn read_axum_project_defaults_host_to_localhost() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
    }

    #[test]
    fn read_axum_project_uses_custom_host() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"0.0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), IpAddr::from([0, 0, 0, 0]));
    }

    #[test]
    fn read_axum_project_invalid_host_falls_back_to_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"not-an-ip\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let project =
            read_axum_project_with_env(&root.join("axum.toml"), None, None).expect("project");
        assert_eq!(project.addr.ip(), addr::DEFAULT_HOST);
    }

    #[test]
    fn find_axum_manifest_returns_error_when_not_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Create an empty directory with a Cargo.toml but no axum.toml
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let result = find_axum_manifest(root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("could not locate axum.toml"));
    }

    #[test]
    fn find_axum_manifest_finds_in_current_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        let found = find_axum_manifest(root).expect("manifest");
        assert_eq!(found, root.join("axum.toml"));
    }

    #[test]
    fn find_axum_manifest_finds_closest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("level1/level2");
        fs::create_dir_all(&nested).unwrap();

        // Create axum.toml at root
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"root\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        // Create axum.toml at level1
        fs::write(
            root.join("level1/Cargo.toml"),
            "[package]\nname = \"level1\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("level1/axum.toml"),
            "[adapter]\ncrate = \"level1\"\ncrate_dir = \".\"\n",
        )
        .unwrap();

        // Search from level2, should find level1's axum.toml (closer)
        let found = find_axum_manifest(&nested).expect("manifest");
        assert_eq!(found, root.join("level1/axum.toml"));
    }

    #[test]
    fn deploy_returns_error() {
        let result = deploy(&[]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("does not define a deploy command"));
    }

    #[test]
    fn adapter_name_is_axum() {
        assert_eq!(AXUM_ADAPTER.name(), "axum");
    }

    #[test]
    fn read_axum_project_env_overrides_config() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("axum.toml"),
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"127.0.0.1\"\nport = 3000\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result =
            read_axum_project_with_env(&root.join("axum.toml"), Some("0.0.0.0"), Some("9999"));

        let project = result.expect("project");
        assert_eq!(project.addr.ip(), IpAddr::from([0, 0, 0, 0]));
        assert_eq!(project.addr.port(), 9999);
    }

    #[test]
    fn resolve_subprocess_addr_prefers_edgezero_manifest_over_axum_manifest() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("0.0.0.0"),
            Some(4000),
            Some("127.0.0.1"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 4000)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_falls_back_to_axum_manifest_when_edgezero_missing() {
        let resolution =
            resolve_subprocess_addr_from_parts(None, None, None, None, Some("0.0.0.0"), Some(3000));

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 3000)));
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_env_overrides_both_manifests() {
        let resolution = resolve_subprocess_addr_from_parts(
            Some("::1"),
            Some("9000"),
            Some("0.0.0.0"),
            Some(4000),
            Some("127.0.0.1"),
            Some(3000),
        );

        assert_eq!(
            resolution.addr,
            SocketAddr::from((Ipv6Addr::LOCALHOST, 9000))
        );
        assert!(resolution.warnings.is_empty());
    }

    #[test]
    fn resolve_subprocess_addr_invalid_edgezero_host_falls_back_to_axum_host() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("invalid-host"),
            Some(4000),
            Some("0.0.0.0"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([0, 0, 0, 0], 4000)));
        assert_eq!(resolution.warnings.len(), 1);
    }

    #[test]
    fn resolve_subprocess_addr_edgezero_zero_port_falls_back_to_axum_port() {
        let resolution = resolve_subprocess_addr_from_parts(
            None,
            None,
            Some("127.0.0.1"),
            Some(0),
            Some("0.0.0.0"),
            Some(3000),
        );

        assert_eq!(resolution.addr, SocketAddr::from(([127, 0, 0, 1], 3000)));
        assert_eq!(resolution.warnings.len(), 1);
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

    // ---------- provision (Local mode) ----------

    #[test]
    fn axum_local_provision_creates_dot_edgezero_dir() {
        // Empty fixture — no `.edgezero/` yet, no stores declared.
        // Local provision must still create the directory so the
        // runtime always sees a well-known location for the `.env`
        // file it reads at boot.
        let dir = tempdir().unwrap();
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        assert!(
            dir.path().join(".edgezero").is_dir(),
            ".edgezero/ must exist after local provision"
        );
    }

    #[test]
    fn axum_local_provision_does_not_touch_axum_toml() {
        // Load-bearing invariant: unlike cloudflare/fastly/spin,
        // axum's manifest is operator-owned and tracked. Provision
        // MUST NOT rewrite it. A regression here would silently
        // start editing files the operator manages by hand.
        let dir = tempdir().unwrap();
        let axum_toml = dir.path().join("axum.toml");
        let sentinel =
            "[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n# operator-owned sentinel\n";
        fs::write(&axum_toml, sentinel).unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                Some("axum.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read_to_string(&axum_toml).unwrap();
        assert_eq!(after, sentinel, "axum.toml must be byte-for-byte unchanged");
    }

    #[test]
    fn axum_local_provision_writes_env_name_lines() {
        // For every declared store id (all kinds), a `__NAME` line
        // seeds the runtime store->platform-name map. CONFIG stores
        // also get a commented `__KEY` placeholder the operator can
        // uncomment to switch to a staging blob.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "kv __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default"),
            "secrets __NAME line present: {env}"
        );
        assert!(
            env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "commented __KEY placeholder present for CONFIG only: {env}"
        );
    }

    #[test]
    fn axum_local_provision_dedup_preserves_operator_env_overrides() {
        // Operator already uncommented + edited the __KEY override.
        // A re-provision must NOT re-add the commented placeholder,
        // and must NOT clobber the operator's live value.
        let dir = tempdir().unwrap();
        let dot_edgezero = dir.path().join(".edgezero");
        fs::create_dir_all(&dot_edgezero).unwrap();
        let env_path = dot_edgezero.join(".env");
        fs::write(
            &env_path,
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override\n",
        )
        .unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(&env_path).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override"),
            "operator override preserved: {env}"
        );
        assert!(
            !env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="),
            "commented placeholder must NOT be re-added: {env}"
        );
    }

    #[test]
    fn axum_local_provision_uses_platform_name_when_env_overlay_active() {
        // Simulates
        //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config
        // in effect at CLI time via ResolvedStoreId::new(logical,
        // platform). The emitted __NAME line's VALUE must be the
        // env-resolved platform (`prod_config`); the ENV-VAR KEY
        // must still use the LOGICAL id upper-cased (`APP_CONFIG`)
        // so the runtime env overlay finds it. Same discipline as
        // Cloudflare Task 19.
        let dir = tempdir().unwrap();
        let config_ids = vec![ResolvedStoreId::new("app_config", "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config"),
            "value uses PLATFORM, env-var key uses LOGICAL: {env}"
        );
        assert!(
            !env.contains("EDGEZERO__STORES__CONFIG__PROD_CONFIG__NAME="),
            "platform name must NOT leak into the env-var key: {env}"
        );
    }

    #[test]
    fn axum_local_provision_cloud_mode_is_a_no_op() {
        // Cloud mode: the pre-existing status-line-only arm stays in
        // charge; nothing gets written to disk, and `.edgezero/` must
        // NOT be auto-created. The load-bearing assertion here is
        // the negative one — the Local arm's file work must not leak
        // into Cloud mode.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        let outcome = AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .unwrap();
        assert!(
            !dir.path().join(".edgezero").exists(),
            "cloud mode must NOT auto-create .edgezero/"
        );
        assert!(
            !outcome.status_lines.is_empty(),
            "cloud arm still emits informational status lines"
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

    // ---------- Task 41: per-adapter provision_local_ contract suite ----------
    //
    // These four tests pin the Axum-specific contract for Section 9's
    // "local provision" arm. Unlike the other adapters, Axum has no
    // synthesised adapter manifest to assert against — axum.toml is
    // operator-owned. The load-bearing regression is test #2:
    // `provision_local_does_not_touch_axum_toml`. If a future refactor
    // ever starts synthesising or merging axum.toml, that assertion
    // flips and the CI signal is immediate.

    #[test]
    fn provision_local_creates_dot_edgezero_dir() {
        // Empty fixture: `.edgezero/` does not pre-exist and no stores
        // are declared. Local provision must still create the directory
        // so the runtime has a well-known location to read the `.env`
        // file from at boot.
        let dir = tempdir().unwrap();
        assert!(
            !dir.path().join(".edgezero").exists(),
            "sanity: .edgezero/ must NOT pre-exist"
        );
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        assert!(
            dir.path().join(".edgezero").is_dir(),
            ".edgezero/ must exist as a directory after local provision"
        );
    }

    #[test]
    fn provision_local_does_not_touch_axum_toml() {
        // Load-bearing invariant: unlike cloudflare/fastly/spin, Axum's
        // adapter manifest (`axum.toml`) is operator-owned and tracked.
        // Provision MUST NOT synthesise, merge, or otherwise rewrite
        // it. The assertion is a byte-identical comparison against a
        // distinctive sentinel — a regression that silently starts
        // touching axum.toml will flip this.
        let dir = tempdir().unwrap();
        let axum_toml = dir.path().join("axum.toml");
        let sentinel =
            b"[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\n# operator-authored do not touch\n";
        fs::write(&axum_toml, sentinel).unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                Some("axum.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read(&axum_toml).unwrap();
        assert_eq!(
            after,
            sentinel.to_vec(),
            "axum.toml must be byte-for-byte unchanged (Axum-exception invariant)"
        );
    }

    #[test]
    fn provision_local_writes_env_name_lines() {
        // Fixture: one store per kind. Local provision must:
        //   - write `.edgezero/.env` starting with the provenance
        //     header (Section 5 review fix — `# edgezero-provision: v1`);
        //   - emit one `__NAME` line per kind (KV / CONFIG / SECRETS);
        //   - emit a commented `__KEY` placeholder for CONFIG only.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.starts_with("# edgezero-provision: v1"),
            ".env must start with the provenance header: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "kv __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default"),
            "secrets __NAME line present: {env}"
        );
        assert!(
            env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "commented __KEY placeholder present for CONFIG only: {env}"
        );
    }

    #[test]
    fn re_provision_preserves_operator_env_edits() {
        // First provision writes the base `.edgezero/.env` (including
        // the commented `__KEY` placeholder). The operator uncomments
        // AND edits the line to point at their own override value.
        // Re-running provision must NOT re-add the commented form and
        // MUST leave the operator's uncommented line byte-identical
        // (Task 16c dedup semantics — key-normalised uncommented
        // form wins over any commented sibling).
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env_path = dir.path().join(".edgezero/.env");
        let first = fs::read_to_string(&env_path).unwrap();
        assert!(
            first.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "first-run must seed the commented placeholder: {first}"
        );

        // Operator uncomments AND edits the value.
        let operator_line = "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=my_local_override";
        let edited = first.replace(
            "# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging",
            operator_line,
        );
        fs::write(&env_path, &edited).unwrap();

        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read_to_string(&env_path).unwrap();
        let matching: Vec<&str> = after
            .lines()
            .filter(|line| *line == operator_line)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "operator's uncommented override line must survive byte-identical: {after}"
        );
        assert!(
            !after.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="),
            "commented placeholder must NOT be re-added when uncommented form exists: {after}"
        );
    }
}
