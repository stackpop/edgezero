use anyedge_adapter_fastly::cli;
use anyedge_core::manifest::{Manifest, ManifestLoader, ResolvedEnvironment};

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Fastly,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Build,
    Deploy,
    Serve,
}

impl Provider {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "fastly" => Ok(Self::Fastly),
            other => Err(format!("provider `{other}` is not yet supported")),
        }
    }
}

pub fn execute(
    provider: &str,
    action: Action,
    manifest: Option<&ManifestLoader>,
) -> Result<(), String> {
    let parsed = Provider::parse(provider)?;

    if let Some(manifest) = manifest {
        if let Some(command) = manifest_command(manifest.manifest(), provider, action) {
            let root = manifest.manifest().root().unwrap_or_else(|| Path::new("."));
            let env = manifest.manifest().environment_for(provider);
            return run_shell(command, root, provider, action, Some(env));
        }
    }

    match (parsed, action) {
        (Provider::Fastly, Action::Build) => {
            let artifact = cli::build()?;
            println!("[anyedge] Fastly build complete -> {}", artifact.display());
            Ok(())
        }
        (Provider::Fastly, Action::Deploy) => cli::deploy(),
        (Provider::Fastly, Action::Serve) => cli::serve(),
    }
}

fn run_shell(
    command: &str,
    cwd: &Path,
    provider: &str,
    action: Action,
    environment: Option<ResolvedEnvironment>,
) -> Result<(), String> {
    println!(
        "[anyedge] executing `{}` for provider `{}` in {}",
        command,
        provider,
        cwd.display()
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);

    if let Some(env) = environment {
        apply_environment(provider, &env, &mut cmd)?;
    }

    let status = cmd
        .status()
        .map_err(|err| format!("failed to run {} command `{}`: {}", action, command, err))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} command `{}` exited with status {}",
            action, command, status
        ))
    }
}

fn apply_environment(
    provider: &str,
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
            "provider `{}` requires the following secrets to be set: {}",
            provider,
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

fn manifest_command<'a>(manifest: &'a Manifest, provider: &str, action: Action) -> Option<&'a str> {
    manifest
        .providers
        .get(provider)
        .and_then(|cfg| match action {
            Action::Build => cfg.commands.build.as_deref(),
            Action::Deploy => cfg.commands.deploy.as_deref(),
            Action::Serve => cfg.commands.serve.as_deref(),
        })
}

#[cfg(test)]
mod tests {
    use super::{apply_environment, Provider, ResolvedEnvironment};
    use anyedge_core::manifest::ResolvedEnvironmentBinding;
    use std::process::Command;

    #[test]
    fn parse_fastly() {
        assert!(matches!(Provider::parse("fastly"), Ok(Provider::Fastly)));
        assert!(matches!(Provider::parse("Fastly"), Ok(Provider::Fastly)));
    }

    #[test]
    fn parse_unknown() {
        let err = Provider::parse("unknown").unwrap_err();
        assert!(err.contains("not yet supported"));
    }

    #[test]
    fn apply_environment_sets_defaults_and_checks_secrets() {
        std::env::remove_var("ANYEDGE_TEST_SECRET");

        let env = ResolvedEnvironment {
            variables: vec![ResolvedEnvironmentBinding {
                name: "Base".into(),
                description: None,
                env: "ANYEDGE_TEST_BASE".into(),
                value: Some("https://demo".into()),
            }],
            secrets: vec![ResolvedEnvironmentBinding {
                name: "Secret".into(),
                description: None,
                env: "ANYEDGE_TEST_SECRET".into(),
                value: None,
            }],
        };

        let result = apply_environment("fastly", &env, &mut Command::new("echo"));
        assert!(result.is_err());

        std::env::set_var("ANYEDGE_TEST_SECRET", "set");
        let mut cmd = Command::new("echo");
        apply_environment("fastly", &env, &mut cmd).expect("environment applied");
        let has_var = cmd.get_envs().any(|(key, value)| {
            key.to_str() == Some("ANYEDGE_TEST_BASE")
                && value.and_then(|v| v.to_str()) == Some("https://demo")
        });
        assert!(has_var);

        std::env::remove_var("ANYEDGE_TEST_SECRET");
    }
}
