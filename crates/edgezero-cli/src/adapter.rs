use edgezero_adapter::registry::{self as adapter_registry, AdapterAction, AdapterExecContext};
use edgezero_core::manifest::{Manifest, ManifestLoader, ResolvedEnvironment};

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
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

/// Resolve every env override for a spawned child into an owned
/// `(key, value)` list, then assert the adapter's required secrets
/// are actually reachable.
///
/// Single source of truth for BOTH dispatch paths -- `run_shell`
/// (manifest `[adapters.<name>.commands].<action>`) and the registry
/// fallback that hands the result to `Adapter::execute` via
/// `AdapterExecContext`. Before this was extracted, only `run_shell`
/// applied any of it and the fallback silently ran with none of it.
///
/// Precedence, high to low:
///   1. Parent env -- an operator's `KEY=v edgezero serve` wins over
///      everything. `Command` has no inherit-then-override per key, so
///      a parent-set key is simply never pushed here and the child
///      inherits it untouched.
///   2. The `.env` overlay (Spin's `<spin_dir>/.env`, Axum's
///      `.edgezero/.env`).
///   3. Manifest `[environment.variables]` -- a DEFAULT, not an
///      override.
///   4. Manifest `[adapters.<name>.adapter]` host/port bind hint.
///
/// Pushed low-to-high with last-wins so the ordering above reads
/// directly off the call order below.
fn build_child_env(
    adapter_name: &str,
    environment: Option<&ResolvedEnvironment>,
    adapter_bind: (Option<String>, Option<u16>),
    env_overlay: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |key: &str, value: String| {
        // Parent wins: never shadow a key the operator exported.
        if env::var_os(key).is_some() {
            return;
        }
        out.retain(|(existing, _)| existing != key);
        out.push((key.to_owned(), value));
    };

    let (manifest_host, manifest_port) = adapter_bind;
    if let Some(host) = manifest_host {
        push("EDGEZERO__ADAPTER__HOST", host);
    }
    if let Some(port) = manifest_port {
        push("EDGEZERO__ADAPTER__PORT", port.to_string());
    }

    if let Some(env) = environment {
        for binding in &env.variables {
            if let Some(value) = &binding.value {
                push(&binding.env, value.clone());
            }
        }
    }

    for (key, value) in env_overlay {
        push(key, value.clone());
    }

    if let Some(env) = environment {
        assert_required_secrets_present(adapter_name, env, &out)?;
    }
    Ok(out)
}

/// Every `[environment.secrets]` binding must resolve to a value the
/// child will actually see.
///
/// The check used to consult ONLY the parent process env, and ran
/// before the `.env` overlay was applied to the command -- so a secret
/// the provision-written `.env` file supplied was still reported
/// missing, and `edgezero serve` refused to start on a correctly
/// provisioned project. Resolution order
/// is now the same one the child sees: parent env, or anything
/// `build_child_env` resolved for it.
fn assert_required_secrets_present(
    adapter_name: &str,
    environment: &ResolvedEnvironment,
    resolved: &[(String, String)],
) -> Result<(), String> {
    let missing: Vec<String> = environment
        .secrets
        .iter()
        .filter(|binding| {
            env::var_os(&binding.env).is_none()
                && !resolved.iter().any(|(key, _)| key == &binding.env)
        })
        .map(|binding| format!("{} (env `{}`)", binding.name, binding.env))
        .collect();

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
    execute_with_env_overlay(adapter_name, action, manifest_loader, adapter_args, &[])
}

/// Same as [`execute`] but additionally applies an owned
/// `(key, value)` overlay to the spawned child's environment via
/// `Command::env`. Used by `run_serve` to expose the
/// provision-written `.env` file WITHOUT mutating the parent
/// process's shared `environ` (see `env_file.rs` for the
/// thread-safety rationale).
///
/// Existing-env-wins: entries whose key is already set in the
/// parent process are dropped here so a caller-supplied
/// `KEY=value` on the command line still overrides the `.env`.
pub(crate) fn execute_with_env_overlay(
    adapter_name: &str,
    action: Action,
    manifest_loader: Option<&ManifestLoader>,
    adapter_args: &[String],
    env_overlay: &[(String, String)],
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
            Some(&env),
            adapter_bind,
            adapter_args,
            env_overlay,
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

    // Registry fallback: no manifest `commands.<action>`, so the
    // adapter spawns its own vendor CLI. Hand it the SAME cwd + child
    // env the shell path above would have applied, otherwise a `serve`
    // here starts with none of the `.env` secrets and resolves its
    // manifest from the process cwd.
    let (cwd, child_env): (Option<&Path>, Vec<(String, String)>) = match manifest_loader {
        Some(loader) => {
            let root = loader.manifest().root();
            let env = loader.manifest().environment_for(adapter_name);
            let adapter_bind = adapter_bind_from_manifest(loader.manifest(), adapter_name);
            let resolved = build_child_env(adapter_name, Some(&env), adapter_bind, env_overlay)?;
            (root, resolved)
        }
        // No manifest at all: still forward any overlay the caller
        // passed (today `run_serve` only builds one from a manifest, so
        // this is empty in practice, but the plumbing stays honest).
        None => (
            None,
            build_child_env(adapter_name, None, (None, None), env_overlay)?,
        ),
    };
    // The manifest-declared, root-resolved
    // `[adapters.<name>.adapter].manifest` / `.crate`. Passing them stops
    // the adapter from rediscovering (and possibly mis-selecting, or
    // symlink-escaping) its per-platform manifest by scanning the
    // workspace, and tells it where `Cargo.toml` actually lives.
    //
    // `.manifest` is REQUIRED whenever a manifest is loaded and declares
    // this adapter: without it every adapter falls back to a recursive,
    // symlink-following workspace scan that can select an unrelated or
    // out-of-tree project. Refuse the dispatch instead of discovering.
    let (adapter_manifest_abs, adapter_crate_abs) =
        resolve_declared_adapter_paths(manifest_loader, adapter_name)?;
    let mut ctx = AdapterExecContext::new().with_env(&child_env);
    if let Some(root) = cwd {
        ctx = ctx.with_cwd(root);
    }
    if let Some(manifest_abs) = adapter_manifest_abs.as_deref() {
        ctx = ctx.with_adapter_manifest(manifest_abs);
    }
    if let Some(crate_abs) = adapter_crate_abs.as_deref() {
        ctx = ctx.with_adapter_crate(crate_abs);
    }
    adapter.execute(AdapterAction::from(action), adapter_args, &ctx)
}

/// Resolve the declared, project-root-resolved
/// `[adapters.<name>.adapter]` `manifest` / `crate` paths for the
/// registry-fallback dispatch.
///
/// `.manifest` is REQUIRED whenever a manifest is loaded and declares
/// this adapter: without it the adapter falls back to a recursive,
/// symlink-following workspace scan that can select an unrelated or
/// out-of-tree project. `.crate` is optional here — adapters fall back
/// to the manifest's parent — but is passed through so a nested manifest
/// still resolves the right `Cargo.toml`.
///
/// Returns `(None, None)` when no manifest is loaded or the adapter
/// isn't declared in it: the adapter then runs standalone and its own
/// discovery is the only thing available.
#[cfg(feature = "cli")]
fn resolve_declared_adapter_paths(
    manifest_loader: Option<&ManifestLoader>,
    adapter_name: &str,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    let Some((declared_root, cfg)) = manifest_loader.and_then(|loader| {
        let manifest = loader.manifest();
        manifest
            .adapter_entry(adapter_name)
            .map(|(_canonical, cfg)| (manifest.root(), cfg))
    }) else {
        return Ok((None, None));
    };
    let root = declared_root.unwrap_or_else(|| Path::new("."));
    let rel = cfg.adapter.manifest.as_deref().ok_or_else(|| {
        format!(
            "`[adapters.{adapter_name}.adapter].manifest` is not declared. It is required: \
             without it the adapter falls back to scanning the workspace for its platform \
             manifest, which can select an unrelated or out-of-tree project. Add \
             `manifest = \"<path/to/manifest>\"` (run `provision --adapter {adapter_name} \
             --local` to generate the file)."
        )
    })?;
    Ok((
        Some(root.join(rel)),
        cfg.adapter.crate_path.as_deref().map(|cp| root.join(cp)),
    ))
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

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper; each argument is a distinct axis of \
              the shell dispatch (command, cwd, adapter identity, \
              action, manifest env, bind hint, passthrough args, \
              env-file overlay) and bundling them for the lint's \
              sake would add ceremony without clarity"
)]
fn run_shell(
    command: &str,
    cwd: &Path,
    adapter_name: &str,
    action: Action,
    environment: Option<&ResolvedEnvironment>,
    adapter_bind: (Option<String>, Option<u16>),
    adapter_args: &[String],
    env_overlay: &[(String, String)],
) -> Result<(), String> {
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

    // Precedence + the required-secrets assertion both live in
    // `build_child_env` so this path and the registry fallback in
    // `execute_with_env_overlay` cannot drift apart.
    let child_env = build_child_env(adapter_name, environment, adapter_bind, env_overlay)?;
    cmd.envs(child_env);

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
    use super::{ResolvedEnvironment, build_child_env, resolve_declared_adapter_paths};
    use crate::test_support::manifest_guard;
    use edgezero_core::manifest::{ManifestLoader, ResolvedEnvironmentBinding};
    use edgezero_core::test_env::EnvOverride;
    use std::path::{Path, PathBuf};

    #[test]
    fn declared_adapter_paths_require_manifest_field() {
        // Without `.manifest` the adapter would fall back to a recursive,
        // symlink-following workspace scan. Refuse the dispatch instead.
        let loader = ManifestLoader::load_from_str(
            "[app]\nname = \"demo\"\n\n[adapters.spin.adapter]\ncrate = \"crates/spin\"\n",
        );
        let err = resolve_declared_adapter_paths(Some(&loader), "spin")
            .expect_err("a declared adapter without `.manifest` must be refused");
        assert!(
            err.contains("[adapters.spin.adapter].manifest") && err.contains("required"),
            "error names the missing required field: {err}"
        );
        assert!(
            err.contains("out-of-tree"),
            "error explains why ambient discovery is unsafe: {err}"
        );
    }

    #[test]
    fn declared_adapter_paths_resolve_nested_manifest_and_crate_root() {
        // A nested manifest must NOT imply its parent is the crate root:
        // `crates/server/config/spin.toml` has its Cargo.toml at
        // `crates/server`, which only `.crate` can tell us.
        let loader = ManifestLoader::load_from_str(
            "[app]\nname = \"demo\"\n\n[adapters.spin.adapter]\ncrate = \"crates/server\"\nmanifest = \"crates/server/config/spin.toml\"\n",
        );
        let (manifest_abs, crate_abs) =
            resolve_declared_adapter_paths(Some(&loader), "spin").expect("both declared");
        let root = loader
            .manifest()
            .root()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        assert_eq!(
            manifest_abs,
            Some(root.join("crates/server/config/spin.toml"))
        );
        assert_eq!(
            crate_abs,
            Some(root.join("crates/server")),
            "crate root is the DECLARED `.crate`, not the manifest's parent"
        );
    }

    #[test]
    fn declared_adapter_paths_are_none_without_a_manifest() {
        // Standalone invocation (no edgezero.toml): the adapter's own
        // discovery is the only thing available, so this is not an error.
        let (manifest_abs, crate_abs) =
            resolve_declared_adapter_paths(None, "spin").expect("no manifest is not an error");
        assert!(manifest_abs.is_none() && crate_abs.is_none());
    }

    #[test]
    fn declared_adapter_paths_are_none_for_an_undeclared_adapter() {
        let loader = ManifestLoader::load_from_str("[app]\nname = \"demo\"\n");
        let (manifest_abs, crate_abs) = resolve_declared_adapter_paths(Some(&loader), "spin")
            .expect("an undeclared adapter is not an error here");
        assert!(manifest_abs.is_none() && crate_abs.is_none());
    }

    fn value_for<'env>(env: &'env [(String, String)], key: &str) -> Option<&'env str> {
        env.iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn build_child_env_sets_defaults_and_checks_secrets() {
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

        // Neither parent env nor overlay supplies the secret -> error.
        build_child_env(adapter_name, Some(&env), (None, None), &[])
            .expect_err("a required secret with no source must error");

        let _secret = EnvOverride::set("EDGEZERO_TEST_SECRET", "set");
        let resolved =
            build_child_env(adapter_name, Some(&env), (None, None), &[]).expect("env resolved");
        assert_eq!(
            value_for(&resolved, "EDGEZERO_TEST_BASE"),
            Some("https://demo")
        );
    }

    #[test]
    fn build_child_env_treats_env_overlay_as_a_secret_source() {
        // Regression: required secrets
        // were checked against the parent env only, BEFORE the `.env`
        // overlay was applied -- so a secret supplied purely by the
        // provision-written `.env` file was falsely reported missing
        // and `edgezero serve` refused to start.
        const SECRET: &str = "EDGEZERO_TEST_OVERLAY_SECRET";
        let _lock = manifest_guard().lock().expect("env lock");
        let _unset = EnvOverride::remove(SECRET);

        let env = ResolvedEnvironment {
            secrets: vec![ResolvedEnvironmentBinding {
                description: None,
                env: SECRET.into(),
                name: "Overlay-Secret".into(),
                value: None,
            }],
            variables: vec![],
        };
        let overlay = vec![(SECRET.to_owned(), "from_dotenv".to_owned())];

        let resolved = build_child_env("test-adapter", Some(&env), (None, None), &overlay)
            .expect("overlay must satisfy the required secret");
        assert_eq!(value_for(&resolved, SECRET), Some("from_dotenv"));
    }

    #[test]
    fn build_child_env_defers_to_parent_env_when_already_set() {
        // Manifest `[environment.variables].value` is a DEFAULT.
        // When the operator exports the same env var in the parent
        // shell (e.g. `EDGEZERO__ADAPTER__HOST=parent edgezero build`),
        // the parent value must win -- the manifest default must not
        // stomp it. The resolved list must therefore NOT carry the key
        // (the child inherits the parent value via the OS env).
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

        let resolved =
            build_child_env("test-adapter", Some(&env), (None, None), &[]).expect("env resolved");
        assert!(
            value_for(&resolved, KEY).is_none(),
            "manifest default must NOT be injected when parent env is already set; \
             parent value would otherwise be shadowed"
        );
    }

    #[test]
    fn build_child_env_uses_manifest_default_when_parent_env_unset() {
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

        let resolved =
            build_child_env("test-adapter", Some(&env), (None, None), &[]).expect("env resolved");
        assert_eq!(
            value_for(&resolved, KEY),
            Some("from_manifest_default"),
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
