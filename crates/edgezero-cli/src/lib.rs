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
#[cfg(all(feature = "cli", feature = "demo-example"))]
mod demo_server;
#[cfg(feature = "cli")]
mod diff;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod provision;
#[cfg(feature = "cli")]
mod scaffold;
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
pub use provision::run_provision;

#[cfg(feature = "cli")]
use args::{BuildArgs, DeployArgs, HealthcheckArgs, NewArgs, RollbackArgs, ServeArgs};
#[cfg(feature = "cli")]
use edgezero_core::manifest::ManifestLoader;
#[cfg(feature = "cli")]
use std::env;
#[cfg(feature = "cli")]
use std::io::ErrorKind;
#[cfg(feature = "cli")]
use std::path::PathBuf;

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
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            log::Level::Error | log::Level::Warn => {
                #[expect(
                    clippy::print_stderr,
                    reason = "CLI UX output goes to stderr for warn/error"
                )]
                {
                    eprintln!("{}", record.args());
                }
            }
            log::Level::Info => {
                #[expect(clippy::print_stdout, reason = "CLI UX output goes to stdout for info")]
                {
                    println!("{}", record.args());
                }
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

    // Thread `--service-id` (spec §5.4) into the adapter invocation
    // when provided, ahead of any operator passthrough args. Fastly
    // consumes it; adapters that don't need a service id ignore it.
    let mut passthrough: Vec<String> = Vec::new();
    if let Some(service_id) = &args.service_id {
        passthrough.push("--service-id".to_owned());
        passthrough.push(service_id.clone());
    }
    passthrough.extend_from_slice(&args.adapter_args);

    if args.stage {
        // Staged deploy: clone the active version, upload the built
        // package to a new draft, mark it staged, and emit the staged
        // version (spec §5.4). Never runs the manifest `deploy`
        // command, which would activate production.
        //
        // Thread the manifest-configured Fastly manifest path (resolved
        // from `[adapters.<adapter>.adapter].manifest` relative to the
        // `EDGEZERO_MANIFEST`-honoring manifest root) so the staged
        // deploy targets the app the operator selected — not whichever
        // `fastly.toml` a bare working-directory search finds first in a
        // monorepo. The adapter falls back to a cwd search only when the
        // manifest declares no adapter `manifest` key.
        let mut staged: Vec<String> = Vec::new();
        if let Some(manifest_path) = resolve_adapter_manifest_path(manifest.as_ref(), &args.adapter)
        {
            staged.push("--manifest-path".to_owned());
            staged.push(manifest_path);
        }
        staged.extend(passthrough);
        return adapter::execute(
            &args.adapter,
            adapter::Action::DeployStaged,
            manifest.as_ref(),
            &staged,
        );
    }

    // Production deploy also emits the activated version (spec §5.4.2)
    // so the deploy-fastly action can surface `fastly-version` and the
    // deploy→healthcheck→rollback chain has a real version to thread.
    //
    // Resolution precedence (cheapest + most reliable first):
    //   1. The deploy command's OWN output. We tee it (echoed live to
    //      the operator, captured for us) and look for a canonical
    //      `version=<N>` line, then for Fastly's native phrasing
    //      ("... version 12"). The deploy command already knows the
    //      version it activated, so this needs no API round-trip and
    //      works under a manifest `[adapters.fastly.commands].deploy`
    //      override (including test fixtures with dummy credentials).
    //   2. Only when the output yields nothing: the Fastly API lookup
    //      (`EmitVersion`), which needs a live API + a real token.
    //   3. If BOTH fail: a clear `Err`. We never silently emit an empty
    //      version — that was the original finding.
    if args.service_id.is_some() && args.adapter.eq_ignore_ascii_case("fastly") {
        let captured = adapter::execute_capture(
            &args.adapter,
            adapter::Action::Deploy,
            manifest.as_ref(),
            &passthrough,
        )?;
        if let Some(version) = captured.as_deref().and_then(parse_deploy_version) {
            log::info!("version={version}");
            return Ok(());
        }
        return adapter::execute(
            &args.adapter,
            adapter::Action::EmitVersion,
            manifest.as_ref(),
            &passthrough,
        )
        .map_err(|err| {
            format!(
                "deploy succeeded but the activated version could not be resolved: no `version=<N>` \
                 (or Fastly `version <N>`) line in the deploy output, and the Fastly API fallback \
                 failed: {err}"
            )
        });
    }

    adapter::execute(
        &args.adapter,
        adapter::Action::Deploy,
        manifest.as_ref(),
        &passthrough,
    )
}

/// Parse an activated service version out of a deploy command's output.
///
/// Precedence:
///   1. A canonical `version=<N>` line (what a manifest
///      `[adapters.fastly.commands].deploy` override — or a CI fixture —
///      emits, and what `EdgeZero` itself prints).
///   2. Fastly's native phrasing, e.g.
///      `SUCCESS: Deployed package (service abc, version 12)`. The LAST
///      mention wins, which is the version the deploy ended on.
///
/// Returns `None` when neither shape is present, which sends the caller
/// to the Fastly API fallback.
#[cfg(feature = "cli")]
fn parse_deploy_version(output: &str) -> Option<u64> {
    parse_canonical_version_line(output).or_else(|| parse_native_version_mention(output))
}

/// Last `version=<N>` line in `output` (leading/trailing whitespace on
/// the line is ignored).
#[cfg(feature = "cli")]
fn parse_canonical_version_line(output: &str) -> Option<u64> {
    output.lines().rev().find_map(|line| {
        let digits: String = line
            .trim()
            .strip_prefix("version=")?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<u64>().ok()
    })
}

/// Last `, version <N>)` mention in `output` (case-insensitive) — the
/// Fastly CLI's own success line, whose Go format string is
/// `"Deployed package (service %s, version %v)"`.
///
/// Deliberately narrow: it previously accepted ANY digits appearing
/// after the word "version", so `Fastly CLI version 15.2.0` or
/// `... service 12345, version unchanged` parsed as a service version.
/// A misparse here emits a WRONG `version=<N>` line, which the deploy →
/// healthcheck → rollback chain would then act on. When this returns
/// `None`, `run_deploy` falls back to the Fastly API's *active* version
/// (the version the deploy actually activated) rather than guessing.
#[cfg(feature = "cli")]
fn parse_native_version_mention(output: &str) -> Option<u64> {
    let lower = output.to_ascii_lowercase();
    let mut result = None;
    for (idx, _) in lower.match_indices(", version ") {
        let after = idx.saturating_add(", version ".len());
        let Some(rest) = lower.get(after..) else {
            continue;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        // The number must be closed by the success line's `)`.
        if digits.is_empty() || rest.chars().nth(digits.len()) != Some(')') {
            continue;
        }
        if let Ok(parsed) = digits.parse::<u64>() {
            result = Some(parsed);
        }
    }
    result
}

/// Resolve the absolute path of the adapter's platform manifest
/// (`[adapters.<adapter>.adapter].manifest`) relative to the manifest
/// root. Returns `None` when there is no loaded manifest, no root, no
/// entry for the adapter, or no `manifest` key. Used by the Fastly
/// staged deploy to target the operator-selected app in a monorepo.
#[cfg(feature = "cli")]
fn resolve_adapter_manifest_path(loader: Option<&ManifestLoader>, adapter: &str) -> Option<String> {
    let manifest = loader?.manifest();
    let root = manifest.root()?;
    let (_canonical, cfg) = manifest.adapter_entry(adapter)?;
    let rel = cfg.adapter.manifest.as_deref()?;
    Some(root.join(rel).to_string_lossy().into_owned())
}

/// Probe a deployed version's health (Fastly staging lifecycle, spec
/// §5.4) and return `Err` when the probe is unhealthy after retries so
/// the process exits non-zero (letting a CI caller gate rollback on
/// failure).
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is
/// not configured / registered, the adapter does not support
/// healthchecks, or the probe is unhealthy after all retries.
#[cfg(feature = "cli")]
#[inline]
pub fn run_healthcheck(args: &HealthcheckArgs) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(&args.adapter, manifest.as_ref())?;
    let mut passthrough: Vec<String> = vec![
        "--service-id".to_owned(),
        args.service_id.clone(),
        "--version".to_owned(),
        args.version.clone(),
        "--domain".to_owned(),
        args.domain.clone(),
    ];
    if args.staging {
        passthrough.push("--staging".to_owned());
    }
    passthrough.extend([
        "--retry".to_owned(),
        args.retry.to_string(),
        "--retry-delay".to_owned(),
        args.retry_delay.to_string(),
        "--timeout".to_owned(),
        args.timeout.to_string(),
    ]);
    adapter::execute(
        &args.adapter,
        adapter::Action::Healthcheck,
        manifest.as_ref(),
        &passthrough,
    )
}

/// Roll a service back (Fastly staging lifecycle, spec §5.4):
/// production activates the previous version; staging deactivates the
/// staged version.
///
/// # Errors
///
/// Returns an error if the manifest cannot be loaded, the adapter is
/// not configured / registered, the adapter does not support
/// rollback, or the rollback API call fails.
#[cfg(feature = "cli")]
#[inline]
pub fn run_rollback(args: &RollbackArgs) -> Result<(), String> {
    let manifest = load_manifest_optional()?;
    ensure_adapter_defined(&args.adapter, manifest.as_ref())?;
    let mut passthrough: Vec<String> = vec![
        "--service-id".to_owned(),
        args.service_id.clone(),
        "--version".to_owned(),
        args.version.clone(),
    ];
    if args.staging {
        passthrough.push("--staging".to_owned());
    }
    adapter::execute(
        &args.adapter,
        adapter::Action::Rollback,
        manifest.as_ref(),
        &passthrough,
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
    adapter::execute(
        &args.adapter,
        adapter::Action::Serve,
        manifest.as_ref(),
        &[],
    )
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

    // ── deploy-output version parsing (spec §5.4.2) ───────────────────

    #[test]
    fn parse_deploy_version_reads_canonical_line() {
        // What a manifest `[adapters.fastly.commands].deploy` override
        // (or a CI fixture running with dummy creds) emits. Must be
        // parsed WITHOUT any Fastly API round-trip.
        let output = "building...\nversion=7\ndone\n";
        assert_eq!(parse_deploy_version(output), Some(7));
    }

    #[test]
    fn parse_deploy_version_reads_fastly_native_phrasing() {
        let output = "SUCCESS: Deployed package (service abc123, version 12)\n";
        assert_eq!(parse_deploy_version(output), Some(12));
    }

    #[test]
    fn parse_deploy_version_none_when_absent_triggers_fallback() {
        // No version anywhere -> `None`, which routes run_deploy to the
        // Fastly API fallback (and to a clear Err if that also fails).
        let output = "Building package...\nUploading...\nAll good.\n";
        assert_eq!(parse_deploy_version(output), None);
        assert_eq!(parse_deploy_version(""), None);
    }

    #[test]
    fn parse_deploy_version_prefers_canonical_over_native_mention() {
        // A fixture that both narrates a clone AND emits the canonical
        // line: the canonical line is authoritative.
        let output = "Cloning version 3...\nversion=9\n";
        assert_eq!(parse_deploy_version(output), Some(9));
    }

    #[test]
    fn parse_deploy_version_native_takes_last_success_line() {
        let output = "SUCCESS: Deployed package (service abc, version 3)\n\
             SUCCESS: Deployed package (service abc, version 4)\n";
        assert_eq!(parse_deploy_version(output), Some(4));
    }

    #[test]
    fn parse_deploy_version_rejects_confusable_mentions() {
        // Loose `version <N>` narration is NOT a service version. Each of
        // these used to parse (and would have emitted a wrong `version=<N>`
        // for healthcheck/rollback to act on). `None` routes run_deploy to
        // the Fastly API's *active* version instead — the safe answer.
        assert_eq!(parse_deploy_version("Fastly CLI version 15.2.0\n"), None);
        assert_eq!(
            parse_deploy_version("Uploaded to service 12345, version unchanged\n"),
            None
        );
        assert_eq!(
            parse_deploy_version("Cloning version 3... created version 4\n"),
            None
        );
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
            // No service id → the production version-emit step (spec
            // §5.4.2) is skipped, so this test exercises only the
            // manifest `deploy` command path.
            service_id: None,
            stage: false,
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
}
