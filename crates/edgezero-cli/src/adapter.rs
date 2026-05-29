use edgezero_adapter::registry::{self as adapter_registry, AdapterAction};
use edgezero_core::manifest::{Manifest, ManifestLoader, ResolvedEnvironment};

use std::env;
use std::fmt;
use std::path::Path;
use std::process::Command;

include!(concat!(env!("OUT_DIR"), "/linked_adapters.rs"));

#[derive(Debug, Clone, Copy)]
pub enum Action {
    AuthLogin,
    AuthLogout,
    AuthStatus,
    Build,
    Deploy,
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
    if let Some(loader) = manifest_loader {
        if let Some(command) = manifest_command(loader.manifest(), adapter_name, action) {
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

fn manifest_command<'manifest>(
    manifest: &'manifest Manifest,
    adapter_name: &str,
    action: Action,
) -> Option<&'manifest str> {
    let cfg = manifest.adapters.get(adapter_name)?;
    match action {
        Action::AuthLogin => cfg.commands.auth_login.as_deref(),
        Action::AuthLogout => cfg.commands.auth_logout.as_deref(),
        Action::AuthStatus => cfg.commands.auth_status.as_deref(),
        Action::Build => cfg.commands.build.as_deref(),
        Action::Deploy => cfg.commands.deploy.as_deref(),
        Action::Serve => cfg.commands.serve.as_deref(),
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
    let Some(cfg) = manifest.adapters.get(adapter_name) else {
        return (None, None);
    };
    (cfg.adapter.host.clone(), cfg.adapter.port)
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
    let full_command = if adapter_args.is_empty() {
        command.to_owned()
    } else {
        format!("{} {}", command, shell_join(adapter_args))
    };
    log::info!(
        "[edgezero] executing `{}` for adapter `{}` in {}",
        full_command,
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
    if let Some(host) = manifest_host {
        if env::var_os("EDGEZERO__ADAPTER__HOST").is_none() {
            cmd.env("EDGEZERO__ADAPTER__HOST", host);
        }
    }
    if let Some(port) = manifest_port {
        if env::var_os("EDGEZERO__ADAPTER__PORT").is_none() {
            cmd.env("EDGEZERO__ADAPTER__PORT", port.to_string());
        }
    }

    if let Some(env) = environment {
        apply_environment(adapter_name, &env, &mut cmd)?;
    }

    let status = cmd
        .status()
        .map_err(|err| format!("failed to run {action} command `{full_command}`: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{action} command `{full_command}` exited with status {status}"
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
    use super::{apply_environment, ResolvedEnvironment};
    use edgezero_core::manifest::ResolvedEnvironmentBinding;
    use std::env;
    use std::process::Command;

    #[test]
    fn apply_environment_sets_defaults_and_checks_secrets() {
        env::remove_var("EDGEZERO_TEST_SECRET");

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

        env::set_var("EDGEZERO_TEST_SECRET", "set");
        let mut cmd = Command::new("echo");
        apply_environment(adapter_name, &env, &mut cmd).expect("environment applied");
        let has_var = cmd.get_envs().any(|(key, value)| {
            key.to_str() == Some("EDGEZERO_TEST_BASE")
                && value.and_then(|val| val.to_str()) == Some("https://demo")
        });
        assert!(has_var);

        env::remove_var("EDGEZERO_TEST_SECRET");
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
        env::set_var(KEY, "from_parent_shell");

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

        env::remove_var(KEY);
    }

    #[test]
    fn apply_environment_uses_manifest_default_when_parent_env_unset() {
        // Mirror of the above: when the parent shell has NOT set the
        // env var, the manifest default fills it in.
        const KEY: &str = "EDGEZERO_TEST_MANIFEST_FILLS";
        env::remove_var(KEY);

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
