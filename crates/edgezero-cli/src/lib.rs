//! `EdgeZero` CLI library.
//!
//! Exposes the built-in command handlers (`run_build`, `run_deploy`,
//! `run_new`, `run_serve`, `run_config_validate*`) and their argument
//! structs so downstream projects can build their own CLI binary that
//! reuses any subset of edgezero's built-in commands. The default
//! `edgezero` binary (`main.rs`) is a thin wrapper over this library.
//!
//! `run_demo` is an additional contributor-only handler, available only
//! under the `demo-example` feature — it runs the in-repo `app-demo`
//! example and is not meant for downstream CLIs.

// `pub use config::*` re-exports `run_config_validate*` at the crate
// root. The lint is module-scoped (cannot be `#[expect]`-ed per-item);
// downstream CLIs already call `edgezero_cli::run_build` / `run_serve`
// at the crate root, so the new validators follow the same convention.
#![expect(
    clippy::pub_use,
    reason = "config-validate entry points re-export at the crate root to match the existing run_* surface downstream CLIs already use"
)]

#[cfg(feature = "cli")]
mod adapter;
#[cfg(feature = "cli")]
mod auth;
#[cfg(feature = "cli")]
mod config;
#[cfg(feature = "cli")]
mod copy_tree;
#[cfg(all(feature = "cli", feature = "demo-example"))]
mod demo_server;
#[cfg(feature = "cli")]
mod diff;
#[cfg(feature = "cli")]
mod env_file;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod path_safety;
#[cfg(feature = "cli")]
mod provision;
#[cfg(feature = "cli")]
mod provision_lock;
#[cfg(feature = "cli")]
mod scaffold;
#[cfg(all(test, feature = "cli"))]
mod shared_test_guards;
/// CLI stream-discipline helpers -- `stdout_line`, `info_line`,
/// `prompt`. Every stdout/stderr write in the `edgezero` binary
/// (and in the scaffolded downstream binaries that reuse this
/// crate) MUST go through these so the workspace
/// `clippy::print_stderr` / `clippy::print_stdout` restrictions
/// still catch accidental prints elsewhere as real bugs.
#[cfg(feature = "cli")]
pub mod stream;
#[cfg(all(test, feature = "cli"))]
mod test_support;

/// CLI argument structs (`Args`, `Command`, and the per-command `*Args`
/// types). A `pub mod` so downstream binaries can reuse the built-in
/// command argument types — e.g. `edgezero_cli::args::BuildArgs`.
#[cfg(feature = "cli")]
pub mod args;

#[cfg(feature = "cli")]
pub use auth::run_auth;
#[cfg(feature = "cli")]
pub use config::{
    run_config_diff_typed, run_config_push, run_config_push_typed, run_config_validate,
    run_config_validate_typed, DiffExit,
};
#[cfg(feature = "cli")]
pub use provision::{run_provision, run_provision_typed};

#[cfg(feature = "cli")]
use args::{BuildArgs, DeployArgs, NewArgs, ServeArgs};
#[cfg(feature = "cli")]
use edgezero_core::manifest::{Manifest, ManifestLoader};
#[cfg(feature = "cli")]
use path_safety::assert_provision_paths_safe;
#[cfg(feature = "cli")]
use std::env;
#[cfg(feature = "cli")]
use std::io::ErrorKind;
#[cfg(feature = "cli")]
use std::path::{Path, PathBuf};

/// CLI output logger: prints `record.args()` verbatim with no
/// timestamps, levels, or module prefixes — the CLI's output IS
/// the user-facing UX, not a debug log. `info` goes to stdout;
/// `warn`/`error` go to stderr. `debug` and `trace` are filtered
/// out by `enabled()` and `LevelFilter::Info`; there is no
/// verbosity flag yet — adding one is a follow-up that would
/// route debug/trace alongside info.
///
/// Replaces the previous `SimpleLogger`-based init: `SimpleLogger`
/// always emitted `INFO [edgezero_cli::xxx] ...` prefixes even
/// with `without_timestamps()`, regressing the user-facing CLI UX
/// the surrounding doc comment promised.
#[cfg(feature = "cli")]
struct CliLogger;

#[cfg(feature = "cli")]
impl log::Log for CliLogger {
    #[inline]
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    #[inline]
    fn flush(&self) {}

    #[inline]
    fn log(&self, record: &log::Record<'_>) {
        use crate::stream::{info_line, stdout_line};
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            log::Level::Error | log::Level::Warn => {
                info_line(&format!("{}", record.args()));
            }
            log::Level::Info => {
                stdout_line(&format!("{}", record.args()));
            }
            log::Level::Debug | log::Level::Trace => {}
        }
    }
}

/// Initialize a CLI logger that prints messages without timestamps
/// or level prefixes — the CLI's output IS the user-facing UX, not
/// a debug log. See [`CliLogger`] for the routing rules.
#[cfg(feature = "cli")]
#[inline]
pub fn init_cli_logger() {
    static CLI_LOGGER: CliLogger = CliLogger;
    let _logger_init =
        log::set_logger(&CLI_LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
}

/// Build the project for a target edge adapter.
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is not
/// configured, or the adapter build command fails.
#[cfg(feature = "cli")]
#[inline]
pub fn run_build(args: &BuildArgs) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(&args.adapter, manifest.as_ref())?;
    if let Some(loader) = &manifest {
        log_store_bindings(&args.adapter, loader);
    }
    adapter::execute(
        &args.adapter,
        adapter::Action::Build,
        manifest.as_ref(),
        &args.adapter_args,
    )
}

/// Deploy the project to a target edge adapter.
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is not
/// configured, or the adapter deploy command fails.
#[cfg(feature = "cli")]
#[inline]
pub fn run_deploy(args: &DeployArgs) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(&args.adapter, manifest.as_ref())?;
    adapter::execute(
        &args.adapter,
        adapter::Action::Deploy,
        manifest.as_ref(),
        &args.adapter_args,
    )
}

/// Run a local simulation for a target edge adapter.
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is not
/// configured, or the adapter serve command fails.
#[cfg(feature = "cli")]
#[inline]
pub fn run_serve(args: &ServeArgs) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(&args.adapter, manifest.as_ref())?;

    // Adapter-scoped env-file load: `axum` reads `.edgezero/.env`,
    // `spin` reads the `.env` next to the resolved `spin.toml`.
    // `cloudflare` and `fastly` read their own files (`.dev.vars`,
    // `[local_server.*]`) via their emulators and need no CLI-side
    // help.
    //
    // Spin's env path is derived from
    // `[adapters.spin.adapter].manifest` (see
    // `resolve_serve_env_file`), so an operator-authored poisoned
    // manifest string like `manifest = "/etc/spin.toml"` or
    // `manifest = "../../../secrets/env"` would resolve to an
    // out-of-tree `.env` that `env_file::load_into_process_env`
    // would happily read and inject into the process env
    // (subsequently inherited by the spawned adapter). Run the
    // same absolute-path + `..` traversal guard provision and
    // config already use before touching the resolved path.
    if let Some(loader) = manifest.as_ref() {
        if let Some(root) = loader.manifest().root() {
            let manifest_data = loader.manifest();
            if let Some((_key, adapter_cfg)) = manifest_data.adapter_entry(&args.adapter) {
                assert_provision_paths_safe(
                    root,
                    adapter_cfg.adapter.manifest.as_deref(),
                    adapter_cfg.adapter.crate_path.as_deref(),
                )?;
            }
            if let Some(env_path) = resolve_serve_env_file(manifest_data, &args.adapter, root) {
                if env_path.exists() {
                    env_file::load_into_process_env(&env_path)?;
                }
            }
        }
    }

    adapter::execute(
        &args.adapter,
        adapter::Action::Serve,
        manifest.as_ref(),
        &[],
    )
}

/// Return the `.env` file `run_serve` should pre-load into the
/// process environment for the selected adapter, or `None` if the
/// adapter reads its env file directly (cloudflare, fastly) or the
/// adapter is unknown.
///
/// `axum` maps to `<manifest_root>/.edgezero/.env` (Task 27's
/// writer target). `spin` maps to `<crate>/.env` where `<crate>` is
/// the `[adapters.spin.adapter] crate = "..."` sub-path joined with
/// `manifest_root` (Task 25's writer target); a missing `crate`
/// falls back to `manifest_root`.
///
/// The adapter name is matched case-insensitively so `--adapter Spin`
/// or `SPIN` resolves the same as `spin`.
#[cfg(feature = "cli")]
fn resolve_serve_env_file(
    manifest: &Manifest,
    adapter_name: &str,
    manifest_root: &Path,
) -> Option<PathBuf> {
    let adapter_lower = adapter_name.to_ascii_lowercase();
    match adapter_lower.as_str() {
        "axum" => Some(manifest_root.join(".edgezero").join(".env")),
        "spin" => {
            // Spin provision writes `.env` next to the resolved
            // `spin.toml` (see
            // `edgezero-adapter-spin/src/cli/provision_local.rs`:
            // `env_path = spin_dir.join(".env")`). Derive the same
            // path here from `[adapters.spin.adapter].manifest`.
            // A nested manifest like
            // `[adapters.spin.adapter].manifest = "crates/spin/config/spin.toml"`
            // places `.env` at `crates/spin/config/.env`, NOT at
            // `crates/spin/.env` — deriving from `.crate` would miss
            // the runtime env-label and typed `SPIN_VARIABLE_*` lines
            // provision just wrote.
            let (_key, adapter_cfg) = manifest.adapter_entry(adapter_name)?;
            let env_dir = adapter_cfg.adapter.manifest.as_deref().map_or_else(
                || {
                    // No `.manifest` declared (e.g. an out-of-tree
                    // fixture that predates the 2026-07 both-required
                    // rule). Fall back to `.crate`; if that's also
                    // unset, `manifest_root`. Match the historical
                    // resolution shape so this branch is a strict
                    // superset of the pre-fix behaviour.
                    adapter_cfg
                        .adapter
                        .crate_path
                        .as_deref()
                        .map_or_else(|| manifest_root.to_path_buf(), |cp| manifest_root.join(cp))
                },
                |mp| {
                    let manifest_abs = manifest_root.join(mp);
                    manifest_abs
                        .parent()
                        .map_or_else(|| manifest_root.to_path_buf(), Path::to_path_buf)
                },
            );
            Some(env_dir.join(".env"))
        }
        _ => None,
    }
}

/// Create a new `EdgeZero` app skeleton.
///
/// # Errors
///
/// Returns an error if the project cannot be scaffolded.
#[cfg(feature = "cli")]
#[inline]
pub fn run_new(args: &NewArgs) -> Result<(), String> {
    generator::generate_new(args).map_err(|err| err.to_string())
}

/// Run the bundled `app-demo` example locally on the axum dev server.
///
/// Contributor-only: available only under the `demo-example` feature,
/// which pulls in the in-repo `examples/app-demo` crate.
///
/// # Errors
///
/// Returns an error if the demo server fails to start.
#[cfg(all(feature = "cli", feature = "demo-example"))]
#[inline]
pub fn run_demo() -> Result<(), String> {
    demo_server::run_demo()
}

#[cfg(feature = "cli")]
fn store_bindings_message(adapter_name: &str, manifest: &ManifestLoader) -> Option<String> {
    let manifest_data = manifest.manifest();
    if !manifest_data.secret_store_enabled(adapter_name) {
        return None;
    }

    // Note: the configured binding identifier is intentionally NOT included in
    // this log line. CodeQL's `rust/cleartext-logging` rule taints any value
    // returned by a function whose name contains "secret" (it can't tell
    // metadata from secret material), and adapters/operators can read the
    // binding name from their own `edgezero.toml` if they need to verify it.
    let message = match adapter_name {
        "axum" => "[edgezero] secrets enabled for axum -- ensure the required environment variables are set for local runs",
        "cloudflare" => "[edgezero] secrets enabled for cloudflare -- ensure the required secret bindings exist in wrangler",
        _ => "[edgezero] secrets enabled -- ensure the configured secret store is provisioned on the target platform",
    };

    Some(message.to_owned())
}

#[cfg(feature = "cli")]
fn log_store_bindings(adapter_name: &str, manifest: &ManifestLoader) {
    if let Some(message) = store_bindings_message(adapter_name, manifest) {
        log::info!("{message}");
    }
}

#[cfg(feature = "cli")]
fn ensure_adapter_defined(
    adapter_name: &str,
    manifest_loader: Option<&ManifestLoader>,
) -> Result<(), String> {
    if let Some(loader) = manifest_loader {
        if loader.manifest().adapter_entry(adapter_name).is_some() {
            return Ok(());
        }
        let available: Vec<String> = loader.manifest().adapters.keys().cloned().collect();
        if available.is_empty() {
            Err(format!(
                "adapter `{adapter_name}` is not configured in edgezero.toml (no adapters defined)"
            ))
        } else {
            Err(format!(
                "adapter `{}` is not configured in edgezero.toml (available: {})",
                adapter_name,
                available.join(", ")
            ))
        }
    } else {
        Ok(())
    }
}

#[cfg(feature = "cli")]
fn load_manifest_optional() -> Result<Option<ManifestLoader>, String> {
    let (path, explicit) = env::var("EDGEZERO_MANIFEST").map_or_else(
        |_| (PathBuf::from("edgezero.toml"), false),
        |raw| (PathBuf::from(raw), true),
    );

    match ManifestLoader::from_path(&path) {
        Ok(loader) => Ok(Some(loader)),
        // A missing default `edgezero.toml` is permissive — built-in adapters
        // can still serve the request. An explicitly set `EDGEZERO_MANIFEST`
        // that points at a missing file is a hard error so typos surface
        // instead of silently falling back.
        Err(err) if err.kind() == ErrorKind::NotFound && !explicit => Ok(None),
        Err(err) => Err(format!("failed to load {}: {err}", path.display())),
    }
}

#[cfg(test)]
#[cfg(feature = "cli")]
mod tests {
    use super::*;
    use crate::test_support::{manifest_guard, EnvOverride, BASIC_MANIFEST};
    use edgezero_core::manifest::ManifestLoader;
    use std::fs;
    use tempfile::TempDir;

    const SPIN_MANIFEST_LOWER: &str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;

    const SPIN_MANIFEST_MIXED_CASE: &str = r#"
[app]
name = "demo-app"

[adapters.Spin.adapter]
crate = "crates/spin"

[adapters.Spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;

    /// Nested-manifest fixture: `[adapters.spin.adapter].manifest`
    /// points at a sub-directory inside `.crate`. Provision writes
    /// `.env` next to the resolved `spin.toml`
    /// (`crates/spin/config/.env`) — `run_serve` must load from
    /// the same path.
    const SPIN_MANIFEST_NESTED: &str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/config/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;

    #[test]
    fn load_manifest_optional_hard_errors_when_explicit_env_path_missing() {
        // An explicit `EDGEZERO_MANIFEST` pointing at a missing file must
        // fail loudly so typos surface instead of silently falling back to
        // the built-in adapters.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("missing.toml");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        match load_manifest_optional() {
            Err(err) => assert!(
                err.contains("missing.toml"),
                "error should name the bad path: {err}"
            ),
            Ok(_) => panic!("expected hard error for missing explicit EDGEZERO_MANIFEST"),
        }
    }

    #[test]
    fn load_manifest_optional_returns_none_when_default_missing() {
        // Default `edgezero.toml` missing is the no-manifest case — built-in
        // adapters can still serve the request, so this remains permissive.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let _env = EnvOverride::remove("EDGEZERO_MANIFEST");
        let original_cwd = env::current_dir().expect("cwd");
        env::set_current_dir(temp.path()).expect("cd temp");
        let result = load_manifest_optional();
        env::set_current_dir(original_cwd).expect("restore cwd");
        match result {
            Ok(None) => {}
            Ok(Some(_)) => panic!("expected no manifest in a temp dir"),
            Err(err) => panic!("default missing edgezero.toml should be permissive: {err}"),
        }
    }

    #[test]
    fn load_manifest_optional_reads_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let manifest = load_manifest_optional()
            .expect("load result")
            .expect("manifest present");
        assert!(manifest.manifest().adapters.contains_key("fastly"));
    }

    #[test]
    fn ensure_adapter_defined_accepts_known_adapter() {
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        ensure_adapter_defined("fastly", Some(&loader)).expect("known adapter");
    }

    #[test]
    fn ensure_adapter_defined_reports_unknown_adapter() {
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let err = ensure_adapter_defined("cloudflare", Some(&loader)).expect_err("should err");
        assert!(err.contains("available"));
        assert!(err.contains("fastly"));
    }

    #[test]
    fn ensure_adapter_defined_allows_when_manifest_missing() {
        ensure_adapter_defined("fastly", None).expect("manifest missing -> permissive");
    }

    #[cfg(not(windows))]
    #[test]
    fn run_build_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = BuildArgs {
            adapter: "fastly".to_owned(),
            adapter_args: Vec::new(),
        };
        run_build(&args).expect("build command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn run_deploy_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = DeployArgs {
            adapter: "fastly".to_owned(),
            adapter_args: Vec::new(),
        };
        run_deploy(&args).expect("deploy command runs");
    }

    #[cfg(not(windows))]
    #[test]
    fn run_serve_executes_manifest_command() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        fs::write(&manifest_path, BASIC_MANIFEST).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "fastly".to_owned(),
        };
        run_serve(&args).expect("serve command runs");
    }

    #[test]
    fn secret_store_binding_is_readable_from_manifest() {
        let manifest_with_secrets = r#"
[app]
name = "demo-app"
entry = "crates/demo-core"

[stores.secrets]
ids = ["MY_SECRETS"]

[adapters.fastly.commands]
build = "echo build"
deploy = "echo deploy"
serve = "echo serve"
"#;
        let loader = ManifestLoader::load_from_str(manifest_with_secrets);
        let declared = loader
            .manifest()
            .stores
            .secrets
            .as_ref()
            .expect("[stores.secrets] declared");
        assert_eq!(declared.ids, vec!["MY_SECRETS".to_owned()]);
        assert_eq!(declared.default_id(), "MY_SECRETS");
    }

    #[test]
    fn store_bindings_message_is_adapter_specific() {
        let loader = ManifestLoader::load_from_str(
            r#"
[stores.secrets]
ids = ["MY_SECRETS"]
"#,
        );

        let axum = store_bindings_message("axum", &loader).expect("axum message");
        assert!(axum.contains("environment variables"));

        let cloudflare = store_bindings_message("cloudflare", &loader).expect("cloudflare message");
        assert!(cloudflare.contains("wrangler"));

        let fastly = store_bindings_message("fastly", &loader).expect("fastly message");
        assert!(fastly.contains("secrets enabled"));
    }

    #[test]
    fn store_bindings_message_is_absent_without_secret_store() {
        let loader = ManifestLoader::load_from_str("[app]\nname = \"x\"\n");
        assert!(store_bindings_message("fastly", &loader).is_none());
    }

    #[test]
    fn resolve_serve_env_file_axum_returns_dot_edgezero_dot_env() {
        // axum's `.env` lives under `<manifest_root>/.edgezero/.env`
        // — the target Task 27's line writer produces.
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let root = PathBuf::from("/tmp/proj");
        let resolved = resolve_serve_env_file(loader.manifest(), "axum", &root)
            .expect("axum arm returns Some");
        assert_eq!(resolved, root.join(".edgezero").join(".env"));
    }

    #[test]
    fn resolve_serve_env_file_spin_returns_spin_crate_dot_env() {
        // spin's `.env` lives under `<spin_crate>/.env` — the target
        // Task 25's line writer produces.
        let loader = ManifestLoader::load_from_str(SPIN_MANIFEST_LOWER);
        let root = PathBuf::from("/tmp/proj");
        let resolved = resolve_serve_env_file(loader.manifest(), "spin", &root)
            .expect("spin arm returns Some");
        assert_eq!(resolved, root.join("crates/spin").join(".env"));
    }

    #[test]
    fn resolve_serve_env_file_spin_honors_nested_manifest_parent() {
        // Regression: provision writes `.env` next to the resolved
        // `spin.toml` (`spin_dir.join(".env")` in
        // `edgezero-adapter-spin/src/cli/provision_local.rs`).
        // With `[adapters.spin.adapter].manifest = "crates/spin/config/spin.toml"`,
        // provision writes `crates/spin/config/.env`. run_serve
        // MUST load from the same path — deriving from
        // `.crate = "crates/spin"` alone would look at
        // `crates/spin/.env` and miss every EDGEZERO__STORES__…__NAME
        // + typed SPIN_VARIABLE_* line provision just wrote.
        let loader = ManifestLoader::load_from_str(SPIN_MANIFEST_NESTED);
        let root = PathBuf::from("/tmp/proj");
        let resolved = resolve_serve_env_file(loader.manifest(), "spin", &root)
            .expect("spin arm returns Some");
        assert_eq!(
            resolved,
            root.join("crates/spin/config").join(".env"),
            "spin serve MUST load .env from the manifest's parent dir (matches provision's writeback path), NOT from the adapter crate root"
        );
    }

    #[test]
    fn resolve_serve_env_file_cloudflare_returns_none() {
        // Wrangler reads `.dev.vars` itself; run_serve does not touch it.
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let root = PathBuf::from("/tmp/proj");
        assert!(resolve_serve_env_file(loader.manifest(), "cloudflare", &root).is_none());
    }

    #[test]
    fn resolve_serve_env_file_fastly_returns_none() {
        // Fastly's emulator reads `[local_server.*]` blocks in
        // `fastly.toml`; run_serve does not touch it.
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let root = PathBuf::from("/tmp/proj");
        assert!(resolve_serve_env_file(loader.manifest(), "fastly", &root).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn run_serve_rejects_absolute_spin_adapter_manifest_path() {
        // Regression: `resolve_serve_env_file` derives Spin's `.env`
        // path from `[adapters.spin.adapter].manifest.parent()` and
        // `run_serve` then reads that file. A poisoned absolute
        // manifest string like `/etc/spin.toml` would resolve to
        // `/etc/.env` — `env_file::load_into_process_env` would
        // read it and inject its lines into the process env,
        // subsequently inherited by the spawned adapter. The path
        // safety guard must fire BEFORE the read.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let poisoned = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "/etc/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;
        fs::write(&manifest_path, poisoned).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "spin".to_owned(),
        };
        let err = run_serve(&args).expect_err(
            "run_serve MUST refuse to resolve an absolute [adapters.spin.adapter].manifest",
        );
        assert!(
            err.contains("must be a project-relative path"),
            "path-safety guard must fire before the .env read: {err}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn run_serve_rejects_parent_traversal_in_spin_adapter_manifest() {
        // Symmetric to the absolute-path guard: `..` in the
        // manifest string would resolve `.env` above the project
        // root. Must fire BEFORE the read.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let poisoned = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "../../../outside/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = "echo"
"#;
        fs::write(&manifest_path, poisoned).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "spin".to_owned(),
        };
        let err = run_serve(&args)
            .expect_err("run_serve MUST refuse a `..` traversal in the manifest string");
        assert!(
            err.contains("must not contain `..` traversal"),
            "traversal guard must fire before the .env read: {err}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn run_serve_loads_env_file_into_process_env_before_spawning_child() {
        // Contract test (spec §"Adapter-scoped env-file load"): the
        // `.env` next to the resolved `spin.toml` must have its
        // `KEY=VALUE` lines set into the process env BEFORE
        // `adapter::execute` runs the manifest's serve command, so
        // the SPAWNED CHILD sees the values.
        //
        // Test fidelity note: proving the parent process loaded the
        // .env (via `env::var(marker_key)` after `run_serve`) is
        // necessary but not sufficient — the spec asks for
        // intercepting the spawned child's env. We achieve that
        // here by making the serve command a small shell script
        // that writes its OWN observed value of the marker to a
        // file. The test then reads that file and asserts the
        // child saw the same value the parent set.
        //
        // Prior to c38cb54 the resolver looked at `<crate>/.env`;
        // the manifest below deliberately places `.env` under a
        // nested manifest parent to prove the fix loads THAT file
        // (not a same-named file under `.crate`).
        let marker_key = "EDGEZERO_TEST_SERVE_ENV_LOADED_MARKER";
        let marker_value = "spin-nested-manifest-parent";
        let _lock = manifest_guard().lock().expect("manifest guard");
        let _pre = EnvOverride::remove(marker_key);

        let temp = TempDir::new().expect("temp dir");

        // The child writes its observed marker value here. We pick
        // a path INSIDE the tempdir so parallel test runs never
        // collide, and thread it into the manifest's serve command
        // string via `format!`.
        let observed_path = temp.path().join("child_observed.txt");
        let observed_path_display = observed_path.to_string_lossy();

        let manifest_path = temp.path().join("edgezero.toml");
        // Serve command: a `sh -c` script that writes the child's
        // OWN view of $MARKER_KEY to `observed_path`. If the
        // spawned child inherits nothing (i.e. `run_serve` did NOT
        // load the .env before dispatch), the file will contain an
        // empty value and the assertion below will fail with a
        // useful diff.
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/config/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = 'sh -c "printf %s \"${{{marker_key}:-<unset>}}\" > {observed_path_display}"'
"#,
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");

        // Provision writes `.env` next to the RESOLVED spin.toml
        // (`crates/spin/config/.env` here). Seed one directly.
        let env_dir = temp.path().join("crates/spin/config");
        fs::create_dir_all(&env_dir).expect("mkdir nested spin dir");
        let env_path = env_dir.join(".env");
        fs::write(&env_path, format!("{marker_key}={marker_value}\n")).expect("seed nested .env");

        // Also seed a decoy `.env` at the pre-fix location
        // (`crates/spin/.env`) with a DIFFERENT value so a
        // regression to the crate-based lookup surfaces as a
        // wrong-value assertion in the CHILD-observed file, not a
        // silent pass.
        let decoy_dir = temp.path().join("crates/spin");
        fs::create_dir_all(&decoy_dir).expect("mkdir spin crate");
        let decoy_env = decoy_dir.join(".env");
        fs::write(
            &decoy_env,
            format!("{marker_key}=THIS_MUST_NOT_BE_LOADED_from_crate_root\n"),
        )
        .expect("seed decoy .env");

        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "spin".to_owned(),
        };
        run_serve(&args).expect("run_serve must succeed with the sh-echo serve command");

        // 1. Parent-side check: the process env has the value from
        //    the nested-manifest-parent .env. Necessary for the
        //    child to inherit it.
        let parent_observed = env::var(marker_key).ok();
        assert_eq!(
            parent_observed.as_deref(),
            Some(marker_value),
            "run_serve MUST load the nested-manifest-parent .env into process env — got {parent_observed:?}"
        );

        // 2. Child-side check (the spec's real ask): the spawned
        //    shell wrote its OWN view of $MARKER_KEY to disk. That
        //    value MUST match the .env's value — proving the child
        //    inherited the load before executing.
        let child_observed =
            fs::read_to_string(&observed_path).expect("child wrote observed marker");
        assert_eq!(
            child_observed,
            marker_value,
            "spawned serve child MUST see the marker from the nested-manifest-parent .env — \
             got {child_observed:?} at {}",
            observed_path.display()
        );

        // Sanity: this test intentionally poisons the decoy path;
        // the child-observed assertion above proves the resolver
        // preferred the manifest-derived nested path over it.
        assert!(env_path.exists() && decoy_env.exists());
    }

    #[test]
    fn resolve_serve_env_file_adapter_name_is_case_insensitive() {
        // Manifest declares `[adapters.Spin]` (mixed case). Passing
        // `--adapter spin` (or SPIN) must still resolve to the Spin
        // arm's `<crate>/.env` — the arm lowercases once and matches
        // on the lowercase form.
        let loader = ManifestLoader::load_from_str(SPIN_MANIFEST_MIXED_CASE);
        let root = PathBuf::from("/tmp/proj");
        let expected = root.join("crates/spin").join(".env");
        assert_eq!(
            resolve_serve_env_file(loader.manifest(), "spin", &root),
            Some(expected.clone())
        );
        assert_eq!(
            resolve_serve_env_file(loader.manifest(), "SPIN", &root),
            Some(expected)
        );
    }

    #[test]
    fn load_into_process_env_reads_key_equals_value_lines() {
        // Process-env is global; serialise with the manifest guard
        // (which every other env-mutating test in this module already
        // uses) and rely on EnvOverride::remove's Drop to restore any
        // prior value the parent shell may have set.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let _foo = EnvOverride::remove("EDGEZERO_TEST_ENV_LOAD_FOO");
        let _baz = EnvOverride::remove("EDGEZERO_TEST_ENV_LOAD_BAZ");
        let temp = TempDir::new().expect("temp dir");
        let env_path = temp.path().join(".env");
        fs::write(
            &env_path,
            "EDGEZERO_TEST_ENV_LOAD_FOO=bar\n\
             # comment line -- ignored\n\
             \n\
             EDGEZERO_TEST_ENV_LOAD_BAZ=\"quoted value\"\n\
             malformed line without equals sign\n",
        )
        .expect("write env file");
        env_file::load_into_process_env(&env_path).expect("load ok");
        assert_eq!(
            env::var("EDGEZERO_TEST_ENV_LOAD_FOO").ok().as_deref(),
            Some("bar")
        );
        assert_eq!(
            env::var("EDGEZERO_TEST_ENV_LOAD_BAZ").ok().as_deref(),
            Some("quoted value")
        );
    }

    #[test]
    fn load_into_process_env_existing_env_wins() {
        // Pre-set the key to a caller value; a `.env` line with the
        // same key must NOT overwrite it. The `.env` file supplies
        // defaults; the caller's env is the source of truth.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let _keep = EnvOverride::set("EDGEZERO_TEST_ENV_LOAD_KEEP", "caller_value");
        let temp = TempDir::new().expect("temp dir");
        let env_path = temp.path().join(".env");
        fs::write(&env_path, "EDGEZERO_TEST_ENV_LOAD_KEEP=file_value\n").expect("write env file");
        env_file::load_into_process_env(&env_path).expect("load ok");
        assert_eq!(
            env::var("EDGEZERO_TEST_ENV_LOAD_KEEP").ok().as_deref(),
            Some("caller_value")
        );
    }
}
