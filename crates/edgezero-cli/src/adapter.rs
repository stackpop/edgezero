use edgezero_adapter::{self as adapter_registry, AdapterAction};
use edgezero_core::manifest::{Manifest, ManifestLoader, ResolvedEnvironment};

use std::path::Path;
use std::process::Command;

include!(concat!(env!("OUT_DIR"), "/linked_adapters.rs"));

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Build,
    Deploy,
    Serve,
}

impl From<Action> for AdapterAction {
    fn from(value: Action) -> Self {
        match value {
            Action::Build => AdapterAction::Build,
            Action::Deploy => AdapterAction::Deploy,
            Action::Serve => AdapterAction::Serve,
        }
    }
}

pub fn execute(
    adapter_name: &str,
    action: Action,
    manifest: Option<&ManifestLoader>,
    adapter_args: &[String],
) -> Result<(), String> {
    if let Some(manifest) = manifest {
        if let Some(command) = manifest_command(manifest.manifest(), adapter_name, action) {
            let root = manifest.manifest().root().unwrap_or_else(|| Path::new("."));
            let env = manifest.manifest().environment_for(adapter_name);
            return run_shell(command, root, adapter_name, action, Some(env), adapter_args);
        }
    }

    let adapter = adapter_registry::get_adapter(adapter_name).ok_or_else(|| {
        let available = adapter_registry::registered_adapters();
        if available.is_empty() {
            if manifest.is_none() {
                format!(
                    "adapter `{}` is not registered in this build. Provide an `edgezero.toml` (or set `EDGEZERO_MANIFEST`) so the CLI can load adapters, or rebuild `edgezero-cli` with the `{adapter_name}` adapter feature enabled.",
                    adapter_name
                )
            } else {
                format!(
                    "adapter `{}` is not registered (no adapters available)",
                    adapter_name
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

fn run_shell(
    command: &str,
    cwd: &Path,
    adapter_name: &str,
    action: Action,
    environment: Option<ResolvedEnvironment>,
    adapter_args: &[String],
) -> Result<(), String> {
    let full_command = if adapter_args.is_empty() {
        command.to_string()
    } else {
        format!("{} {}", command, shell_join(adapter_args))
    };
    println!(
        "[edgezero] executing `{}` for adapter `{}` in {}",
        full_command,
        adapter_name,
        cwd.display()
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&full_command).current_dir(cwd);

    if let Some(env) = environment {
        apply_environment(adapter_name, &env, &mut cmd)?;
    }

    let status = cmd.status().map_err(|err| {
        format!(
            "failed to run {} command `{}`: {}",
            action, full_command, err
        )
    })?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} command `{}` exited with status {}",
            action, full_command, status
        ))
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_escape(arg.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-/:=@".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn apply_environment(
    adapter_name: &str,
    environment: &ResolvedEnvironment,
    command: &mut Command,
) -> Result<(), String> {
    for binding in &environment.variables {
        if let Some(value) = &binding.value {
            command.env(&binding.env, value);
        }
    }

    let mut missing = Vec::new();
    for binding in &environment.secrets {
        if std::env::var_os(&binding.env).is_none() {
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

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Action::Build => "build",
            Action::Deploy => "deploy",
            Action::Serve => "serve",
        };
        f.write_str(label)
    }
}

fn manifest_command<'a>(
    manifest: &'a Manifest,
    adapter_name: &str,
    action: Action,
) -> Option<&'a str> {
    manifest
        .adapters
        .get(adapter_name)
        .and_then(|cfg| match action {
            Action::Build => cfg.commands.build.as_deref(),
            Action::Deploy => cfg.commands.deploy.as_deref(),
            Action::Serve => cfg.commands.serve.as_deref(),
        })
}

#[cfg(test)]
mod tests {
    use super::{apply_environment, ResolvedEnvironment};
    use edgezero_core::manifest::ResolvedEnvironmentBinding;
    use std::process::Command;

    #[test]
    fn apply_environment_sets_defaults_and_checks_secrets() {
        std::env::remove_var("EDGEZERO_TEST_SECRET");

        let env = ResolvedEnvironment {
            variables: vec![ResolvedEnvironmentBinding {
                name: "Base".into(),
                description: None,
                env: "EDGEZERO_TEST_BASE".into(),
                value: Some("https://demo".into()),
            }],
            secrets: vec![ResolvedEnvironmentBinding {
                name: "Secret".into(),
                description: None,
                env: "EDGEZERO_TEST_SECRET".into(),
                value: None,
            }],
        };

        let adapter_name = "test-adapter";

        let result = apply_environment(adapter_name, &env, &mut Command::new("echo"));
        assert!(result.is_err());

        std::env::set_var("EDGEZERO_TEST_SECRET", "set");
        let mut cmd = Command::new("echo");
        apply_environment(adapter_name, &env, &mut cmd).expect("environment applied");
        let has_var = cmd.get_envs().any(|(key, value)| {
            key.to_str() == Some("EDGEZERO_TEST_BASE")
                && value.and_then(|v| v.to_str()) == Some("https://demo")
        });
        assert!(has_var);

        std::env::remove_var("EDGEZERO_TEST_SECRET");
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
            "plain".to_string(),
            "with space".to_string(),
            "needs'quote".to_string(),
        ];
        let joined = super::shell_join(&args);
        assert_eq!(joined, "plain 'with space' 'needs'\"'\"'quote'");
    }
}
