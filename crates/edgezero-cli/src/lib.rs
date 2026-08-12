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
    DiffExit, run_config_diff_typed, run_config_push, run_config_push_typed, run_config_validate,
    run_config_validate_typed,
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
    // Same absolute-path / `..` / symlink guard `serve` and provision
    // apply -- `build` and `deploy` dispatch the declared adapter
    // manifest too, so a poisoned `[adapters.<name>.adapter].manifest`
    // must not steer them at an out-of-tree project either.
    assert_adapter_declared_paths_safe(manifest.as_ref(), &args.adapter)?;
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

/// Run the shared absolute-path / `..` / symlink guard on the
/// `[adapters.<name>.adapter]` `manifest` + `crate` strings before any
/// dispatch that resolves them. No-op when no manifest is loaded or the
/// adapter isn't declared.
#[cfg(feature = "cli")]
fn assert_adapter_declared_paths_safe(
    manifest: Option<&ManifestLoader>,
    adapter: &str,
) -> Result<(), String> {
    if let Some(loader) = manifest
        && let Some(root) = loader.manifest().root()
        && let Some((_key, adapter_cfg)) = loader.manifest().adapter_entry(adapter)
    {
        assert_provision_paths_safe(
            root,
            adapter_cfg.adapter.manifest.as_deref(),
            adapter_cfg.adapter.crate_path.as_deref(),
        )?;
    }
    Ok(())
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
    assert_adapter_declared_paths_safe(manifest.as_ref(), &args.adapter)?;
    // Hold the cross-process provision lock across the deploy so a vendor
    // `service_id` writeback (e.g. `fastly compute deploy` rewriting the
    // gitignored `fastly.toml`) can't interleave with a concurrent
    // `provision` / `config push` and lose one side's edits -- the
    // reconcile-on-next-run fallback can't recover a value already
    // discarded. The deploy command may itself invoke `<app>-cli provision`
    // in a child process; `acquire_for_deploy` advertises the lock via an
    // inherited env var so that child BORROWS it instead of self-dead-
    // locking, while an unrelated concurrent provision still serialises.
    let lock_root = manifest_root_for_lock();
    // Held (via its `Drop`) until the end of this function, so the lock
    // stays taken for the whole deploy even though it's last *named* below.
    let lock = provision_lock::ProvisionLock::acquire_for_deploy(&lock_root)?;
    // Advertise the held lock to the deploy subprocess (and its children)
    // via the child's own environment -- NOT the parent's global env, which
    // edition-2024 `set_var` would make unsafe and the workspace forbids.
    // A nested `<app>-cli provision` / `config push` inherits this and
    // BORROWS the lock instead of self-dead-locking.
    // Advertise BOTH the lock path and the per-holder token so a nested
    // provision can prove the advertised holder is still the one holding
    // the lock (a stale/leaked advertisement carries a token that no longer
    // matches the lock file and is refused). Both keys OVERRIDE any
    // inherited ancestor advertisement (see `build_child_env`) so this
    // deploy always advertises ITS OWN lock.
    let advert = provision_lock::ProvisionLock::lock_path_for(&lock_root);
    let overlay = vec![
        (
            provision_lock::LOCK_ENV.to_owned(),
            advert.to_string_lossy().into_owned(),
        ),
        (
            provision_lock::LOCK_TOKEN_ENV.to_owned(),
            lock.token().to_owned(),
        ),
    ];
    adapter::execute_with_env_overlay(
        &args.adapter,
        adapter::Action::Deploy,
        manifest.as_ref(),
        &args.adapter_args,
        &overlay,
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
    // out-of-tree `.env` that the parser would happily read and
    // hand to the spawned adapter. Run the same absolute-path +
    // `..` traversal + symlink-component guard provision and
    // config already use before touching the resolved path.
    //
    // The overlay is threaded into the spawned child via
    // `Command::env` (see `adapter::execute_with_env_overlay`)
    // rather than `std::env::set_var`. On Unix `setenv` /
    // `getenv` are not thread-safe: a downstream multithreaded
    // process calling `run_serve` from one thread while another
    // thread reads `std::env::var` observes a torn read. Passing
    // the overlay through `Command::env` keeps every mutation on
    // the `Command`'s private map — no shared state, no race.
    assert_adapter_declared_paths_safe(manifest.as_ref(), &args.adapter)?;
    let mut env_overlay: Vec<(String, String)> = Vec::new();
    if let Some(loader) = manifest.as_ref()
        && let Some(root) = loader.manifest().root()
    {
        let manifest_data = loader.manifest();
        if let Some(env_path) = resolve_serve_env_file(manifest_data, &args.adapter, root) {
            // The env-file chain (e.g. `<root>/.edgezero/.env`) is NOT
            // covered by `assert_adapter_declared_paths_safe`, which only
            // guards the declared `.manifest` / `.crate`. Guard it
            // UNCONDITIONALLY -- the walk stops at the first missing
            // component, so even with NO `.env` it still rejects a
            // symlinked `.edgezero` directory, which Axum reads
            // `local-config-*.json` from at request time. Gating this on
            // `.env` existing left that config read able to follow a
            // symlink off-tree and inject externally-controlled values.
            use edgezero_adapter::env_file::reject_symlink_components;
            reject_symlink_components(root, &env_path)?;
            if env_path.exists() {
                env_overlay = env_file::parse_env_overlay(&env_path)?;
            }
        }
    }

    adapter::execute_with_env_overlay(
        &args.adapter,
        adapter::Action::Serve,
        manifest.as_ref(),
        &[],
        &env_overlay,
    )
}

/// Return the `.env` file `run_serve` should pre-load into the
/// process environment for the selected adapter, or `None` if the
/// adapter reads its env file directly (cloudflare, fastly) or the
/// adapter is unknown.
///
/// `axum` maps to `<manifest_root>/.edgezero/.env` (the writer
/// target). `spin` maps to `<spin.toml parent>/.env`, derived from the
/// required `[adapters.spin.adapter].manifest` (the same directory
/// provision writes `.env` into); a spin adapter without `.manifest`
/// resolves to `None` — there is no `.crate`/root fallback.
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
            // `env_path = spin_dir.join(".env")`). Derive the same path
            // here from the REQUIRED `[adapters.spin.adapter].manifest`.
            // A nested manifest like
            // `[adapters.spin.adapter].manifest = "crates/spin/config/spin.toml"`
            // places `.env` at `crates/spin/config/.env`.
            //
            // There is deliberately NO `.crate`/root fallback: build,
            // deploy, provision, and config all reject a spin adapter
            // whose `.manifest` is unset, so an absent `.manifest` is a
            // malformed manifest, not a legacy shape to accommodate.
            // Deriving from `.crate` would look in the wrong directory
            // and miss the runtime env-label + typed `SPIN_VARIABLE_*`
            // lines provision wrote. Absent `.manifest` => no overlay.
            let (_key, adapter_cfg) = manifest.adapter_entry(adapter_name)?;
            let manifest_rel = adapter_cfg.adapter.manifest.as_deref()?;
            let manifest_abs = manifest_root.join(manifest_rel);
            let env_dir = manifest_abs
                .parent()
                .map_or_else(|| manifest_root.to_path_buf(), Path::to_path_buf);
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
        "axum" => {
            "[edgezero] secrets enabled for axum -- ensure the required environment variables are set for local runs"
        }
        "cloudflare" => {
            "[edgezero] secrets enabled for cloudflare -- ensure the required secret bindings exist in wrangler"
        }
        _ => {
            "[edgezero] secrets enabled -- ensure the configured secret store is provisioned on the target platform"
        }
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
/// Directory whose `.edgezero/provision.lock` guards this project, derived
/// from the same manifest source `load_manifest_optional` uses so the
/// deploy lock and the provision/push locks target the same sentinel.
#[cfg(feature = "cli")]
fn manifest_root_for_lock() -> PathBuf {
    let path = env::var("EDGEZERO_MANIFEST")
        .map_or_else(|_| PathBuf::from("edgezero.toml"), PathBuf::from);
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

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
    use crate::test_support::{BASIC_MANIFEST, EnvOverride, manifest_guard};
    use edgezero_core::manifest::ManifestLoader;
    use std::fs;
    use tempfile::TempDir;

    const SPIN_MANIFEST_LOWER: &str = r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

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
manifest = "crates/spin/spin.toml"

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
        // — the target the line writer produces.
        let loader = ManifestLoader::load_from_str(BASIC_MANIFEST);
        let root = PathBuf::from("/tmp/proj");
        let resolved = resolve_serve_env_file(loader.manifest(), "axum", &root)
            .expect("axum arm returns Some");
        assert_eq!(resolved, root.join(".edgezero").join(".env"));
    }

    #[test]
    fn resolve_serve_env_file_spin_derives_env_from_manifest_parent() {
        // spin's `.env` lives next to the resolved `spin.toml` — the
        // target the line writer produces — derived from the required
        // `[adapters.spin.adapter].manifest`.
        let loader = ManifestLoader::load_from_str(SPIN_MANIFEST_LOWER);
        let root = PathBuf::from("/tmp/proj");
        let resolved = resolve_serve_env_file(loader.manifest(), "spin", &root)
            .expect("spin arm returns Some");
        assert_eq!(resolved, root.join("crates/spin").join(".env"));
    }

    #[test]
    fn resolve_serve_env_file_spin_without_manifest_returns_none() {
        // `.manifest` is required for spin (build/deploy/provision/config
        // all reject its absence). run_serve must NOT fall back to
        // `.crate`/root -- that would look for `.env` in the wrong
        // directory. A crate-only spin adapter resolves to None.
        let loader = ManifestLoader::load_from_str(
            "[app]\nname = \"demo-app\"\n\n[adapters.spin.adapter]\ncrate = \"crates/spin\"\n\n[adapters.spin.commands]\nbuild = \"echo\"\ndeploy = \"echo\"\nserve = \"echo\"\n",
        );
        let root = PathBuf::from("/tmp/proj");
        assert!(
            resolve_serve_env_file(loader.manifest(), "spin", &root).is_none(),
            "no `.manifest` => no serve env overlay (no legacy `.crate` fallback)"
        );
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
    fn run_build_rejects_absolute_adapter_manifest_path() {
        // `build`/`deploy` dispatch the declared adapter manifest just
        // like `serve`, so the same path-safety guard must fire.
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let poisoned = "[app]\nname = \"demo-app\"\n\n[adapters.spin.adapter]\ncrate = \"crates/spin\"\nmanifest = \"/etc/spin.toml\"\n";
        fs::write(&manifest_path, poisoned).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = BuildArgs {
            adapter: "spin".to_owned(),
            adapter_args: Vec::new(),
        };
        let err = run_build(&args)
            .expect_err("run_build MUST refuse an absolute [adapters.spin.adapter].manifest");
        assert!(
            err.contains("must be a project-relative path"),
            "build path-safety guard must fire: {err}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn run_deploy_rejects_parent_traversal_in_adapter_manifest() {
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let manifest_path = temp.path().join("edgezero.toml");
        let poisoned = "[app]\nname = \"demo-app\"\n\n[adapters.spin.adapter]\ncrate = \"crates/spin\"\nmanifest = \"../../../outside/spin.toml\"\n";
        fs::write(&manifest_path, poisoned).expect("write manifest");
        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = DeployArgs {
            adapter: "spin".to_owned(),
            adapter_args: Vec::new(),
        };
        let err =
            run_deploy(&args).expect_err("run_deploy MUST refuse a `..`-traversing manifest path");
        assert!(
            err.contains("`..`"),
            "deploy path-safety guard must fire: {err}"
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

        // Child-side check (the spec's real ask): the spawned
        // shell wrote its OWN view of $MARKER_KEY to disk. That
        // value MUST match the .env's value — proving the child
        // inherited the Command::env overlay before executing.
        //
        // Note: the load path
        // now threads the overlay through Command::env instead of
        // std::env::set_var, so the PARENT's env stays unchanged.
        // A dedicated no-parent-mutation assertion lives in
        // `run_serve_does_not_mutate_parent_process_env_yet_child_sees_env_file_value`
        // below; this test intentionally stays focused on the
        // manifest-parent .env resolution regression.
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
        // arm's `<spin.toml parent>/.env` — the arm lowercases once and
        // matches on the lowercase form.
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

    #[cfg(not(windows))]
    #[test]
    fn run_serve_does_not_mutate_parent_process_env_yet_child_sees_env_file_value() {
        // Regression: the pre-fix
        // `env_file::load_into_process_env` wrote every KEY=VALUE
        // pair via `std::env::set_var`. That's not thread-safe on
        // Unix and can race with any concurrent reader in a
        // multithreaded downstream process embedding
        // `edgezero_cli::run_serve`.
        //
        // The current design threads the `.env` overlay through
        // `Command::env` (see `env_file::parse_env_overlay` +
        // `adapter::execute_with_env_overlay`), so the CHILD sees
        // the file's values at exec time while the PARENT's shared
        // `environ` stays untouched.
        //
        // This test seeds a nested Spin `.env` with a marker, runs
        // `run_serve`, and asserts BOTH:
        //   (a) the child's observed marker equals the .env value
        //       (proving the overlay reached the spawned process),
        //   (b) the parent's `env::var(marker_key)` is still None
        //       (proving `run_serve` did NOT call `set_var`).
        let marker_key = "EDGEZERO_TEST_SERVE_ENV_NO_PARENT_MUTATION_MARKER";
        let marker_value = "child-only-do-not-leak-to-parent";
        let _lock = manifest_guard().lock().expect("manifest guard");
        // Pre-condition: the marker MUST be unset in the parent, or
        // `existing env wins` would drop the overlay and the test
        // could pass vacuously.
        let _pre = EnvOverride::remove(marker_key);

        let temp = TempDir::new().expect("temp dir");
        let observed_path = temp.path().join("child_observed.txt");
        let observed_path_display = observed_path.to_string_lossy();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = 'sh -c "printf %s \"${{{marker_key}:-<unset>}}\" > {observed_path_display}"'
"#,
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");
        let env_dir = temp.path().join("crates/spin");
        fs::create_dir_all(&env_dir).expect("mkdir spin");
        fs::write(
            env_dir.join(".env"),
            format!("{marker_key}={marker_value}\n"),
        )
        .expect("seed .env");

        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "spin".to_owned(),
        };
        run_serve(&args).expect("run_serve must succeed");

        // (a) CHILD side — must see the marker.
        let child_observed =
            fs::read_to_string(&observed_path).expect("child wrote observed marker");
        assert_eq!(
            child_observed, marker_value,
            "spawned child MUST inherit the .env overlay: got {child_observed:?}"
        );

        // (b) PARENT side — must NOT see the marker. This is the
        // load-bearing thread-safety assertion.
        let parent_observed = env::var(marker_key).ok();
        assert!(
            parent_observed.is_none(),
            "run_serve MUST NOT mutate the parent process env; found `{marker_key}={parent_observed:?}`. \
             Regression: `env_file::parse_env_overlay` bypassed and someone re-introduced \
             `std::env::set_var` on the load path."
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_serve_refuses_symlinked_env_file() {
        // A symlinked `.env` in the serve env-file chain could inject
        // externally-controlled values into the spawned child. run_serve
        // must refuse it before parsing, and before the child spawns.
        use std::os::unix::fs::symlink;
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let observed_path = temp.path().join("child_observed.txt");
        let observed_path_display = observed_path.to_string_lossy();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = 'sh -c "printf ran > {observed_path_display}"'
"#,
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");
        let env_dir = temp.path().join("crates/spin");
        fs::create_dir_all(&env_dir).expect("mkdir spin");
        // `.env` is a symlink to an out-of-tree file the operator does
        // not control.
        let outside = temp.path().join("attacker.env");
        fs::write(&outside, "INJECTED=1\n").expect("write outside env");
        symlink(&outside, env_dir.join(".env")).expect("symlink .env");

        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "spin".to_owned(),
        };
        let err = run_serve(&args).expect_err("a symlinked .env must be refused");
        assert!(err.contains("symlink"), "error names the symlink: {err}");
        assert!(
            !observed_path.exists(),
            "the child must NOT have spawned once the env chain was rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_serve_refuses_symlinked_edgezero_dir_even_without_env_file() {
        // Regression: the symlink guard used to run only when `.env`
        // existed. Axum reads `<root>/.edgezero/local-config-*.json` at
        // request time, so a symlinked `.edgezero` must be refused even
        // with NO `.env` inside it.
        use std::os::unix::fs::symlink;
        let _lock = manifest_guard().lock().expect("manifest guard");
        let temp = TempDir::new().expect("temp dir");
        let observed_path = temp.path().join("child_observed.txt");
        let observed_path_display = observed_path.to_string_lossy();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/server"
manifest = "crates/server/axum.toml"

[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = 'sh -c "printf ran > {observed_path_display}"'
"#,
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");
        // `.edgezero` is a symlink to an out-of-tree dir holding config
        // the operator does not control. Note: NO `.env` inside.
        let outside = temp.path().join("attacker-edgezero");
        fs::create_dir_all(&outside).expect("mkdir outside");
        fs::write(
            outside.join("local-config-app_config.json"),
            "{\"greeting\":\"pwned\"}\n",
        )
        .expect("write outside config");
        symlink(&outside, temp.path().join(".edgezero")).expect("symlink .edgezero");

        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        let args = ServeArgs {
            adapter: "axum".to_owned(),
        };
        let err = run_serve(&args).expect_err("a symlinked .edgezero must be refused");
        assert!(err.contains("symlink"), "error names the symlink: {err}");
        assert!(
            !observed_path.exists(),
            "the child must NOT have spawned once the .edgezero chain was rejected"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn run_serve_existing_parent_env_wins_over_env_file() {
        // Same contract, opposite direction: when the parent env
        // already has the key, the .env's value must NOT overlay.
        // Serialised access to `EDGEZERO_TEST_SERVE_ENV_KEEP` via
        // the manifest guard so a concurrent test can't observe
        // torn state.
        let marker_key = "EDGEZERO_TEST_SERVE_ENV_KEEP";
        let parent_value = "from-parent";
        let file_value = "from-dot-env-do-not-leak";
        let _lock = manifest_guard().lock().expect("manifest guard");
        let _pre = EnvOverride::set(marker_key, parent_value);

        let temp = TempDir::new().expect("temp dir");
        let observed_path = temp.path().join("child_observed.txt");
        let observed_path_display = observed_path.to_string_lossy();
        let manifest_path = temp.path().join("edgezero.toml");
        let manifest_body = format!(
            r#"
[app]
name = "demo-app"

[adapters.spin.adapter]
crate = "crates/spin"
manifest = "crates/spin/spin.toml"

[adapters.spin.commands]
build = "echo"
deploy = "echo"
serve = 'sh -c "printf %s \"${{{marker_key}:-<unset>}}\" > {observed_path_display}"'
"#,
        );
        fs::write(&manifest_path, &manifest_body).expect("write manifest");
        let env_dir = temp.path().join("crates/spin");
        fs::create_dir_all(&env_dir).expect("mkdir spin");
        fs::write(env_dir.join(".env"), format!("{marker_key}={file_value}\n")).expect("seed .env");

        let manifest_str = manifest_path.to_string_lossy().into_owned();
        let _env = EnvOverride::set("EDGEZERO_MANIFEST", &manifest_str);
        run_serve(&ServeArgs {
            adapter: "spin".to_owned(),
        })
        .expect("run_serve must succeed");

        let child_observed = fs::read_to_string(&observed_path).expect("child wrote observed");
        assert_eq!(
            child_observed, parent_value,
            "parent env MUST win over .env overlay: got {child_observed:?}"
        );
    }
}
