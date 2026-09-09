use edgezero_adapter::registry::{self as adapter_registry, AdapterAction};
use edgezero_core::manifest::{
    CapabilitySupport, Manifest, ManifestContract, ManifestLoader, ResolvedEnvironment,
};

use std::env;
use std::fmt;
use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

include!(concat!(env!("OUT_DIR"), "/linked_adapters.rs"));

#[derive(Debug, Clone, Copy)]
pub enum Action {
    AuthLogin,
    AuthLogout,
    AuthStatus,
    Build,
    Deploy,
    /// Fastly staging lifecycle: stage a draft version.
    DeployStaged,
    /// Fastly staging lifecycle: emit the active version.
    EmitVersion,
    /// Fastly staging lifecycle: probe health.
    Healthcheck,
    /// Fastly staging lifecycle: activate previous /
    /// deactivate staged.
    Rollback,
    Serve,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Action::AuthLogin => "auth login",
            Action::AuthLogout => "auth logout",
            Action::AuthStatus => "auth status",
            Action::Build => "build",
            Action::Deploy => "deploy",
            Action::Serve => "serve",
            Action::DeployStaged => "deploy --staging",
            Action::EmitVersion => "deploy (version)",
            Action::Healthcheck => "healthcheck",
            Action::Rollback => "rollback",
        };
        f.write_str(label)
    }
}

impl From<Action> for AdapterAction {
    #[inline]
    fn from(value: Action) -> Self {
        match value {
            Action::AuthLogin => AdapterAction::AuthLogin,
            Action::AuthLogout => AdapterAction::AuthLogout,
            Action::AuthStatus => AdapterAction::AuthStatus,
            Action::Build => AdapterAction::Build,
            Action::Deploy => AdapterAction::Deploy,
            Action::Serve => AdapterAction::Serve,
            Action::DeployStaged => AdapterAction::DeployStaged,
            Action::EmitVersion => AdapterAction::EmitVersion,
            Action::Healthcheck => AdapterAction::Healthcheck,
            Action::Rollback => AdapterAction::Rollback,
        }
    }
}

fn apply_environment(
    adapter_name: &str,
    environment: &ResolvedEnvironment,
    command: &mut Command,
) -> Result<(), String> {
    // Precedence: a `[environment.variables].value` in the manifest
    // is a DEFAULT, not an override. If the parent process already
    // exported the same env var (e.g. an operator ran
    // `EDGEZERO__ADAPTER__HOST=parent-env edgezero build`), the
    // parent value must reach the child command unchanged. Calling
    // `cmd.env(...)` unconditionally would shadow the parent value;
    // `Command` doesn't inherit-then-override per key, so we check
    // `env::var_os` first and skip the explicit set when the parent
    // already has one. This mirrors the precedence the plan + the
    // typed-config env-overlay docs both promise.
    for binding in &environment.variables {
        if let Some(value) = &binding.value {
            if env::var_os(&binding.env).is_some() {
                continue;
            }
            command.env(&binding.env, value);
        }
    }

    let mut missing = Vec::new();
    for binding in &environment.secrets {
        if env::var_os(&binding.env).is_none() {
            missing.push(format!("{} (env `{}`)", binding.name, binding.env));
        }
    }

    if !missing.is_empty() {
        return Err(format!(
            "adapter `{}` requires the following secrets to be set: {}",
            adapter_name,
            missing.join(", ")
        ));
    }

    Ok(())
}

pub fn execute(
    adapter_name: &str,
    action: Action,
    manifest_loader: Option<&ManifestLoader>,
    adapter_args: &[String],
) -> Result<(), String> {
    if let Some(loader) = manifest_loader
        && let Some(command) = manifest_command(loader.manifest(), adapter_name, action)
    {
        let root = loader.manifest().root().unwrap_or_else(|| Path::new("."));
        let env = loader.manifest().environment_for(adapter_name);
        let adapter_bind = adapter_bind_from_manifest(loader.manifest(), adapter_name);
        return run_shell(
            command,
            root,
            adapter_name,
            action,
            Some(env),
            adapter_bind,
            adapter_args,
        );
    }

    let adapter = adapter_registry::get_adapter(adapter_name).ok_or_else(|| {
        let available = adapter_registry::registered_adapters();
        if available.is_empty() {
            if manifest_loader.is_none() {
                format!(
                    "adapter `{adapter_name}` is not registered in this build. Provide an `edgezero.toml` (or set `EDGEZERO_MANIFEST`) so the CLI can load adapters, or rebuild `edgezero-cli` with the `{adapter_name}` adapter feature enabled."
                )
            } else {
                format!(
                    "adapter `{adapter_name}` is not registered (no adapters available)"
                )
            }
        } else {
            format!(
                "adapter `{}` is not registered (available: {})",
                adapter_name,
                available.join(", ")
            )
        }
    })?;

    adapter.execute(AdapterAction::from(action), adapter_args)
}

/// Same dispatch as [`execute`], but when the action resolves to a
/// manifest-declared shell command the child's output is echoed AND
/// captured (see [`run_shell_tee`]) and returned as `Some(text)`.
///
/// Returns `Ok(None)` when the action was served by the registered
/// adapter's built-in `execute` instead — that path writes straight to
/// the inherited stdio, so there is nothing for us to capture and the
/// caller must fall back to another source of truth (for Fastly deploy:
/// the Fastly API).
pub fn execute_capture(
    adapter_name: &str,
    action: Action,
    manifest_loader: Option<&ManifestLoader>,
    adapter_args: &[String],
) -> Result<Option<String>, String> {
    if let Some(loader) = manifest_loader
        && let Some(command) = manifest_command(loader.manifest(), adapter_name, action)
    {
        let root = loader.manifest().root().unwrap_or_else(|| Path::new("."));
        let env = loader.manifest().environment_for(adapter_name);
        let adapter_bind = adapter_bind_from_manifest(loader.manifest(), adapter_name);
        return run_shell_tee(
            command,
            root,
            adapter_name,
            action,
            Some(env),
            adapter_bind,
            adapter_args,
        )
        .map(Some);
    }
    execute(adapter_name, action, manifest_loader, adapter_args)?;
    Ok(None)
}

pub(crate) fn ensure_capabilities(
    adapter_name: &str,
    contract: ManifestContract<'_>,
) -> Result<(), String> {
    let verified_manifest = match contract {
        ManifestContract::Malformed(reason) => {
            return Err(format!(
                "capability check aborted: {reason}. This is an EdgeZero/app! contract bug; the baked manifest is unreadable, so required capabilities cannot be verified. Refusing to proceed rather than silently skipping enforcement."
            ));
        }
        ManifestContract::None => return Ok(()),
        ManifestContract::Present(present_manifest) => present_manifest,
        _ => {
            return Err(
                "capability check aborted: unrecognized manifest-contract state. Refusing to proceed rather than skipping enforcement."
                    .to_owned(),
            );
        }
    };
    let capabilities = &verified_manifest.capabilities;
    let Some(adapter) = adapter_registry::get_adapter(adapter_name) else {
        if capabilities.required.is_empty() {
            if capabilities.optional.is_empty() {
                log::warn!(
                    "adapter '{adapter_name}' not in registry; capability check skipped (no capabilities declared)"
                );
            } else {
                log::warn!(
                    "adapter '{adapter_name}' not in registry; cannot verify its OPTIONAL capabilities; proceeding, since optional capabilities never hard-fail"
                );
            }
            return Ok(());
        }
        return Err(format!(
            "adapter '{adapter_name}' is not in the registry; cannot verify REQUIRED capabilities. Register an adapter stub that returns capability metadata, or move those entries to `optional`."
        ));
    };

    let mut best_effort = Vec::new();
    let mut unsupported = Vec::new();
    for capability in capabilities.required.iter().copied() {
        match adapter.capability(capability) {
            CapabilitySupport::BestEffort => best_effort.push(capability.as_str()),
            CapabilitySupport::BoundedCooperative => log::info!(
                "adapter '{adapter_name}': required capability '{}' is bounded-cooperative; see capability docs for the bound",
                capability.as_str()
            ),
            CapabilitySupport::Native => {}
            CapabilitySupport::Unsupported | _ => unsupported.push(capability.as_str()),
        }
    }
    if !unsupported.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}' does not support required capabilities: {}",
            unsupported.join(", ")
        ));
    }
    if !best_effort.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}': required capabilities are only best-effort: {}. See https://edgezero.dev/guide/capabilities and declare them `optional` only when the documented limitation is acceptable.",
            best_effort.join(", ")
        ));
    }

    for capability in capabilities.optional.iter().copied() {
        match adapter.capability(capability) {
            CapabilitySupport::BestEffort => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' is best-effort; see https://edgezero.dev/guide/capabilities",
                capability.as_str()
            ),
            CapabilitySupport::BoundedCooperative | CapabilitySupport::Native => {}
            CapabilitySupport::Unsupported => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' unavailable",
                capability.as_str()
            ),
            _ => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' reports an unrecognized support level; treating as degraded",
                capability.as_str()
            ),
        }
    }
    Ok(())
}

/// Whether `action` for `adapter_name` resolves to a manifest-declared
/// shell command (rather than the registered adapter's built-in logic).
///
/// Callers use this to decide whether an EdgeZero-internal directive
/// (e.g. `--manifest-path`, understood only by the built-in adapter) is
/// safe to thread into `adapter_args`: a manifest shell command receives
/// those args verbatim and would choke on a flag its own CLI lacks.
pub fn has_manifest_command(
    manifest_loader: Option<&ManifestLoader>,
    adapter_name: &str,
    action: Action,
) -> bool {
    manifest_loader
        .is_some_and(|loader| manifest_command(loader.manifest(), adapter_name, action).is_some())
}

fn manifest_command<'manifest>(
    manifest: &'manifest Manifest,
    adapter_name: &str,
    action: Action,
) -> Option<&'manifest str> {
    let (_canonical, cfg) = manifest.adapter_entry(adapter_name)?;
    match action {
        Action::AuthLogin => cfg.commands.auth_login.as_deref(),
        Action::AuthLogout => cfg.commands.auth_logout.as_deref(),
        Action::AuthStatus => cfg.commands.auth_status.as_deref(),
        Action::Build => cfg.commands.build.as_deref(),
        Action::Deploy => cfg.commands.deploy.as_deref(),
        Action::Serve => cfg.commands.serve.as_deref(),
        // The Fastly staging lifecycle actions are not
        // manifest-configurable shell commands — they run the
        // adapter's built-in Fastly logic directly. Returning None
        // routes `adapter::execute` past the manifest-command path to
        // the registered adapter's `execute`.
        Action::DeployStaged | Action::EmitVersion | Action::Healthcheck | Action::Rollback => None,
    }
}

/// `(host, port)` from `[adapters.<name>.adapter]`. Translated into
/// `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT` on the
/// subprocess env so the runtime (which reads only the canonical
/// `EDGEZERO__*` names) actually sees the values declared in the manifest.
fn adapter_bind_from_manifest(
    manifest: &Manifest,
    adapter_name: &str,
) -> (Option<String>, Option<u16>) {
    let Some((_canonical, cfg)) = manifest.adapter_entry(adapter_name) else {
        return (None, None);
    };
    (cfg.adapter.host.clone(), cfg.adapter.port)
}

/// Build the `sh -c <command>` child for a manifest-declared adapter
/// command, with the manifest environment / bind hints applied. Shared
/// by [`run_shell`] (inherited stdio) and [`run_shell_tee`] (piped +
/// echoed stdio) so both dispatch paths apply identical env precedence.
fn build_shell_command(
    command: &str,
    cwd: &Path,
    adapter_name: &str,
    environment: Option<ResolvedEnvironment>,
    adapter_bind: (Option<String>, Option<u16>),
    adapter_args: &[String],
) -> Result<Command, String> {
    let full_command = if adapter_args.is_empty() {
        command.to_owned()
    } else {
        format!("{} {}", command, shell_join(adapter_args))
    };
    // Log only the manifest-defined `command`, never the trailing
    // `adapter_args` — passthrough args from `edgezero build/deploy <adapter>
    // -- --token …` can carry deploy tokens, API keys, or other secrets that
    // must not land in logs or in the `Err` strings below.
    log::info!(
        "[edgezero] executing `{}` for adapter `{}` in {}",
        command,
        adapter_name,
        cwd.display()
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&full_command).current_dir(cwd);

    // Precedence (high to low) for `EDGEZERO__ADAPTER__HOST/PORT` on the
    // subprocess:
    //   1. Parent env — propagated through std::process::Command's default
    //      inheritance unless we explicitly `cmd.env()` over it.
    //   2. Manifest `[environment.variables].<EDGEZERO__ADAPTER__...>` —
    //      `apply_environment` writes the explicit per-adapter value.
    //   3. Manifest `[adapters.<name>.adapter] host`/`port` — adapter-
    //      specific bind hint.
    // We inject the bind hint FIRST so `apply_environment` (manifest
    // variable) can overwrite it, then skip the bind injection entirely
    // when the parent env already has the canonical variable so the
    // user's CLI-invocation override wins over everything.
    let (manifest_host, manifest_port) = adapter_bind;
    if let Some(host) = manifest_host
        && env::var_os("EDGEZERO__ADAPTER__HOST").is_none()
    {
        cmd.env("EDGEZERO__ADAPTER__HOST", host);
    }
    if let Some(port) = manifest_port
        && env::var_os("EDGEZERO__ADAPTER__PORT").is_none()
    {
        cmd.env("EDGEZERO__ADAPTER__PORT", port.to_string());
    }

    if let Some(env) = environment {
        apply_environment(adapter_name, &env, &mut cmd)?;
    }

    Ok(cmd)
}

fn run_shell(
    command: &str,
    cwd: &Path,
    adapter_name: &str,
    action: Action,
    environment: Option<ResolvedEnvironment>,
    adapter_bind: (Option<String>, Option<u16>),
    adapter_args: &[String],
) -> Result<(), String> {
    let mut cmd = build_shell_command(
        command,
        cwd,
        adapter_name,
        environment,
        adapter_bind,
        adapter_args,
    )?;

    let status = cmd
        .status()
        .map_err(|err| format!("failed to run {action} command `{command}`: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{action} command `{command}` exited with status {status}"
        ))
    }
}

/// Stream `reader` to `writer` line-by-line while accumulating a copy.
/// This is the "tee" half of [`run_shell_tee`]: the operator still sees
/// the child's output as it happens, and the caller still gets the text
/// to parse.
fn tee_stream<R: Read, W: Write>(reader: R, mut writer: W) -> String {
    let mut buffered = BufReader::new(reader);
    let mut captured = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match buffered.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let _echoed = writer.write_all(line.as_bytes());
                let _flushed = writer.flush();
                captured.push_str(&line);
            }
            // A read error (e.g. a non-UTF-8 byte in the child's output) truncates
            // both the capture and the operator-visible echo. Log it before
            // breaking so the truncation is diagnosable rather than silent.
            Err(err) => {
                log::warn!("stopped reading child output early: {err}");
                break;
            }
        }
    }
    captured
}

/// Same dispatch as [`run_shell`], but the child's stdout/stderr are
/// piped, echoed through to our own stdout/stderr as they arrive, AND
/// captured. Returns the captured `stdout + stderr` text so the caller
/// can parse machine-readable lines (e.g. Fastly's activated
/// `version=<N>`) out of a command it does not otherwise control.
fn run_shell_tee(
    command: &str,
    cwd: &Path,
    adapter_name: &str,
    action: Action,
    environment: Option<ResolvedEnvironment>,
    adapter_bind: (Option<String>, Option<u16>),
    adapter_args: &[String],
) -> Result<String, String> {
    let mut cmd = build_shell_command(
        command,
        cwd,
        adapter_name,
        environment,
        adapter_bind,
        adapter_args,
    )?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to run {action} command `{command}`: {err}"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("failed to capture stdout of {action} command `{command}`"))?;
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("failed to capture stderr of {action} command `{command}`"))?;

    // stderr is drained on a worker thread so a chatty child cannot
    // deadlock by filling the stderr pipe while we block on stdout.
    let stderr_worker = thread::spawn(move || tee_stream(child_stderr, io::stderr()));
    let captured_stdout = tee_stream(child_stdout, io::stdout());
    let captured_stderr = stderr_worker.join().unwrap_or_default();

    let status = child
        .wait()
        .map_err(|err| format!("failed to run {action} command `{command}`: {err}"))?;

    if status.success() {
        Ok(format!("{captured_stdout}{captured_stderr}"))
    } else {
        Err(format!(
            "{action} command `{command}` exited with status {status}"
        ))
    }
}

fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_owned()
    } else if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "._-/:=@".contains(ch))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_escape(arg.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{ResolvedEnvironment, apply_environment, ensure_capabilities};
    use crate::test_support::manifest_guard;
    use edgezero_adapter::registry::{Adapter, AdapterAction, register_adapter};
    use edgezero_core::manifest::{
        Capability, CapabilitySupport, ManifestContract, ManifestLoader, ResolvedEnvironmentBinding,
    };
    use edgezero_core::test_env::EnvOverride;
    use std::process::Command;

    static BEST_EFFORT_ADAPTER: GateAdapter = GateAdapter {
        name: "gate-best-effort",
        support: CapabilitySupport::BestEffort,
    };
    static BOUNDED_ADAPTER: GateAdapter = GateAdapter {
        name: "gate-bounded",
        support: CapabilitySupport::BoundedCooperative,
    };
    static NATIVE_ADAPTER: GateAdapter = GateAdapter {
        name: "gate-native",
        support: CapabilitySupport::Native,
    };
    static UNSUPPORTED_ADAPTER: GateAdapter = GateAdapter {
        name: "gate-unsupported",
        support: CapabilitySupport::Unsupported,
    };

    struct GateAdapter {
        name: &'static str,
        support: CapabilitySupport,
    }

    #[expect(
        clippy::missing_trait_methods,
        reason = "capability-gate fixture overrides only the relevant trait methods"
    )]
    impl Adapter for GateAdapter {
        fn capability(&self, _capability: Capability) -> CapabilitySupport {
            self.support
        }

        fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
            Ok(())
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn capability_manifest(section: &str) -> ManifestLoader {
        ManifestLoader::load_from_str(&format!("[capabilities]\n{section}\n[adapters.gate]\n"))
    }

    #[test]
    fn apply_environment_sets_defaults_and_checks_secrets() {
        let _lock = manifest_guard().lock().expect("env lock");
        // Unset for the missing-secret path; restores the parent value on drop.
        let _unset = EnvOverride::remove("EDGEZERO_TEST_SECRET");

        let env = ResolvedEnvironment {
            secrets: vec![ResolvedEnvironmentBinding {
                description: None,
                env: "EDGEZERO_TEST_SECRET".into(),
                name: "Secret".into(),
                value: None,
            }],
            variables: vec![ResolvedEnvironmentBinding {
                description: None,
                env: "EDGEZERO_TEST_BASE".into(),
                name: "Base".into(),
                value: Some("https://demo".into()),
            }],
        };

        let adapter_name = "test-adapter";

        let result = apply_environment(adapter_name, &env, &mut Command::new("echo"));
        assert!(result.is_err());

        let _secret = EnvOverride::set("EDGEZERO_TEST_SECRET", "set");
        let mut cmd = Command::new("echo");
        apply_environment(adapter_name, &env, &mut cmd).expect("environment applied");
        let has_var = cmd.get_envs().any(|(key, value)| {
            key.to_str() == Some("EDGEZERO_TEST_BASE")
                && value.and_then(|val| val.to_str()) == Some("https://demo")
        });
        assert!(has_var);
    }

    #[test]
    fn apply_environment_defers_to_parent_env_when_already_set() {
        // Manifest `[environment.variables].value` is a DEFAULT.
        // When the operator exports the same env var in the parent
        // shell (e.g. `EDGEZERO__ADAPTER__HOST=parent edgezero build`),
        // the parent value must win -- the manifest default must
        // not stomp it. Without the precedence guard, `cmd.env(...)`
        // would inject the manifest value and the parent override
        // would be lost.
        const KEY: &str = "EDGEZERO_TEST_PARENT_WINS";
        let _lock = manifest_guard().lock().expect("env lock");
        let _parent = EnvOverride::set(KEY, "from_parent_shell");

        let env = ResolvedEnvironment {
            secrets: vec![],
            variables: vec![ResolvedEnvironmentBinding {
                description: None,
                env: KEY.into(),
                name: "Parent-Wins".into(),
                value: Some("from_manifest_default".into()),
            }],
        };

        let mut cmd = Command::new("echo");
        apply_environment("test-adapter", &env, &mut cmd).expect("apply env");

        // The child's explicitly-set envs are what `Command::env`
        // recorded. We DID NOT call it for this key, so it should
        // not appear in `get_envs`. Instead the child inherits the
        // parent's value via the OS env (verified separately by
        // env::var_os in the production path).
        let injected = cmd.get_envs().any(|(key, _)| key.to_str() == Some(KEY));
        assert!(
            !injected,
            "manifest default must NOT be injected when parent env is already set; \
             parent value would otherwise be shadowed"
        );
    }

    #[test]
    fn apply_environment_uses_manifest_default_when_parent_env_unset() {
        // Mirror of the above: when the parent shell has NOT set the
        // env var, the manifest default fills it in.
        const KEY: &str = "EDGEZERO_TEST_MANIFEST_FILLS";
        let _lock = manifest_guard().lock().expect("env lock");
        let _unset = EnvOverride::remove(KEY);

        let env = ResolvedEnvironment {
            secrets: vec![],
            variables: vec![ResolvedEnvironmentBinding {
                description: None,
                env: KEY.into(),
                name: "Manifest-Fills".into(),
                value: Some("from_manifest_default".into()),
            }],
        };

        let mut cmd = Command::new("echo");
        apply_environment("test-adapter", &env, &mut cmd).expect("apply env");

        let injected = cmd.get_envs().any(|(key, value)| {
            key.to_str() == Some(KEY)
                && value.and_then(|val| val.to_str()) == Some("from_manifest_default")
        });
        assert!(
            injected,
            "manifest default must fill the slot when parent env is unset"
        );
    }

    #[test]
    fn capability_gate_accepts_native_and_bounded_required_support() {
        register_adapter(&NATIVE_ADAPTER);
        register_adapter(&BOUNDED_ADAPTER);
        let loader = capability_manifest("required = [\"outbound-http\"]");
        for name in [NATIVE_ADAPTER.name, BOUNDED_ADAPTER.name] {
            assert_eq!(
                ensure_capabilities(name, ManifestContract::from_opt(Some(loader.manifest()))),
                Ok(())
            );
        }
    }

    #[test]
    fn capability_gate_fails_closed_for_malformed_contract() {
        let result = ensure_capabilities(
            "gate-missing-malformed",
            ManifestContract::Malformed("fixture corruption"),
        );
        assert!(result.is_err_and(|error| error.contains("fixture corruption")));
    }

    #[test]
    fn capability_gate_rejects_best_effort_and_unsupported_required_support() {
        register_adapter(&BEST_EFFORT_ADAPTER);
        register_adapter(&UNSUPPORTED_ADAPTER);
        let loader = capability_manifest("required = [\"outbound-http\"]");
        for name in [BEST_EFFORT_ADAPTER.name, UNSUPPORTED_ADAPTER.name] {
            assert!(
                ensure_capabilities(name, ManifestContract::from_opt(Some(loader.manifest())))
                    .is_err()
            );
        }
    }

    #[test]
    fn capability_gate_treats_missing_registry_by_requirement_level() {
        let required = capability_manifest("required = [\"outbound-http\"]");
        let optional = capability_manifest("optional = [\"outbound-http\"]");
        assert!(
            ensure_capabilities(
                "gate-missing-required",
                ManifestContract::from_opt(Some(required.manifest()))
            )
            .is_err()
        );
        assert_eq!(
            ensure_capabilities(
                "gate-missing-optional",
                ManifestContract::from_opt(Some(optional.manifest()))
            ),
            Ok(())
        );
        assert_eq!(
            ensure_capabilities("gate-missing-none", ManifestContract::None),
            Ok(())
        );
    }

    #[test]
    fn optional_capability_degradation_never_hard_fails() {
        register_adapter(&BEST_EFFORT_ADAPTER);
        register_adapter(&UNSUPPORTED_ADAPTER);
        let loader = capability_manifest("optional = [\"outbound-http\"]");
        for name in [BEST_EFFORT_ADAPTER.name, UNSUPPORTED_ADAPTER.name] {
            assert_eq!(
                ensure_capabilities(name, ManifestContract::from_opt(Some(loader.manifest()))),
                Ok(())
            );
        }
    }

    #[test]
    fn shell_escape_quotes_and_spaces() {
        assert_eq!(super::shell_escape("plain"), "plain");
        assert_eq!(super::shell_escape("with space"), "'with space'");
        assert_eq!(super::shell_escape("needs'quote"), "'needs'\"'\"'quote'");
        assert_eq!(super::shell_escape(""), "''");
    }

    #[test]
    fn shell_join_combines_arguments_with_escaping() {
        let args = vec![
            "plain".to_owned(),
            "with space".to_owned(),
            "needs'quote".to_owned(),
        ];
        let joined = super::shell_join(&args);
        assert_eq!(joined, "plain 'with space' 'needs'\"'\"'quote'");
    }
}
