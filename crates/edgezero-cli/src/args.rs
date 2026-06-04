use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "edgezero", about = "EdgeZero CLI")]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in / out / status against the adapter's native CLI
    /// (`wrangler` / `fastly` / `spin`). `EdgeZero` stores no
    /// credentials itself — `auth` just delegates.
    Auth(AuthArgs),
    /// Build the project for a target edge.
    Build(BuildArgs),
    /// Inspect or mutate the typed `<name>.toml` app config.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Run the bundled `app-demo` example locally (contributor-only).
    #[cfg(feature = "demo-example")]
    Demo,
    /// Deploy to a target edge.
    Deploy(DeployArgs),
    /// Create a new `EdgeZero` app skeleton (multi-crate workspace).
    New(NewArgs),
    /// Create the platform resources backing the declared
    /// `[stores.<kind>].ids`. Each adapter owns its
    /// own dispatch: cloudflare shells out to `wrangler`, fastly to
    /// `fastly`, spin edits `spin.toml` in-place, axum is a no-op.
    Provision(ProvisionArgs),
    /// Run a local simulation (adapter-specific).
    Serve(ServeArgs),
}

/// Subcommands under `edgezero config …`. Carries
/// `validate` and `push`.
#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Push the typed `<name>.toml` (flattened, secret-stripped) to
    /// the adapter's config store.
    Push(ConfigPushArgs),
    /// Validate `edgezero.toml` and the typed `<name>.toml` against the
    /// manifest / app-config / Spin-key contract.
    Validate(ConfigValidateArgs),
}

/// Arguments for the `auth` command.
///
/// Intentionally has no `Default` impl: unlike the other `*Args`
/// types in this module (whose fields default to empty strings /
/// vectors / `None`), `AuthSub` is a required subcommand with no
/// "neutral" variant. A default-constructed `AuthArgs` would have
/// no sensible interpretation, so clap derives the required-arg
/// machinery instead.
///
/// The `#[non_exhaustive]` attribute is purely forward-compatibility
/// scaffolding -- there's no struct-literal construction it blocks
/// today (the single `sub` field has no default), but it reserves
/// the option to add a non-`Default` field later without it counting
/// as a `SemVer` break for external callers.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct AuthArgs {
    #[command(subcommand)]
    pub sub: AuthSub,
}

/// Subcommands under `edgezero auth …`. Each carries the adapter the
/// session belongs to; the runtime dispatches to the matching native
/// CLI (`wrangler` / `fastly` / `spin`). `axum` is a no-op (no
/// remote auth).
#[derive(Subcommand, Debug)]
pub enum AuthSub {
    /// Sign in (`wrangler login` / `fastly profile create` / `spin
    /// cloud login`).
    Login {
        #[arg(long)]
        adapter: String,
    },
    /// Sign out (`wrangler logout` / `fastly profile delete` / `spin
    /// cloud logout`).
    Logout {
        #[arg(long)]
        adapter: String,
    },
    /// Show the current session (`wrangler whoami` / `fastly profile
    /// list` / `spin cloud info`).
    Status {
        #[arg(long)]
        adapter: String,
    },
}

/// Arguments for the `build` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct BuildArgs {
    /// Target adapter name.
    #[arg(long = "adapter", required = true)]
    pub adapter: String,
    /// Arguments passed through to the adapter build command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub adapter_args: Vec<String>,
}

/// Arguments for the `deploy` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct DeployArgs {
    /// Target adapter name.
    #[arg(long = "adapter", required = true)]
    pub adapter: String,
    /// Arguments passed through to the adapter deploy command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub adapter_args: Vec<String>,
}

/// Arguments for the `new` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct NewArgs {
    /// Directory to create the app in (default: current dir).
    #[arg(long)]
    pub dir: Option<String>,
    /// App name (e.g., my-edge-app).
    pub name: String,
}

/// Arguments for the `provision` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ProvisionArgs {
    /// Target adapter name.
    #[arg(long, required = true)]
    pub adapter: String,
    /// Print the would-be commands and would-be manifest edits
    /// without performing them.
    #[arg(long)]
    pub dry_run: bool,
    /// Path to the manifest (default: `edgezero.toml`).
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
}

/// Arguments for the `serve` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ServeArgs {
    /// Target adapter name.
    #[arg(long = "adapter", required = true)]
    pub adapter: String,
}

/// Arguments for the `config push` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ConfigPushArgs {
    /// Target adapter name.
    #[arg(long, required = true)]
    pub adapter: String,
    /// Path to the typed app-config file (default: `<app_name>.toml`
    /// resolved from the manifest's `[app].name`, next to the manifest).
    #[arg(long)]
    pub app_config: Option<PathBuf>,
    /// Print the would-be operations without performing them.
    #[arg(long)]
    pub dry_run: bool,
    /// Push to the adapter's local-emulator state instead of the live
    /// platform. For Fastly this edits `[local_server.config_stores]`
    /// in the adapter's `fastly.toml` (the Viceroy reads it on startup);
    /// for Cloudflare it runs `wrangler kv bulk put --local` so writes
    /// land in `.wrangler/state`. Axum and Spin's pushes are already
    /// local-only, so `--local` is a no-op there (identical to the
    /// default).
    #[arg(long)]
    pub local: bool,
    /// Path to the manifest (default: `edgezero.toml`).
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// Skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the
    /// typed app-config. The default loads the overlay so the runtime
    /// and the push see the same resolved values.
    #[arg(long)]
    pub no_env: bool,
    /// Seed token for adapters that push via HTTP (currently spin).
    /// Resolution order: this flag → `EDGEZERO__ADAPTERS__<NAME>__SEED_TOKEN`.
    /// Never read from `edgezero.toml` (tokens stay out of manifests).
    #[arg(long)]
    pub seed_token: Option<String>,
    /// Seed URL for adapters that push via HTTP (currently spin). For
    /// the prod chain (no `--local`), resolution order is: this flag →
    /// `EDGEZERO__ADAPTERS__<NAME>__SEED_URL` → `[adapters.<name>.commands].seed_url`
    /// in `edgezero.toml`. For `--local`, manifest is NEVER consulted;
    /// the order is: this flag → `EDGEZERO__ADAPTERS__<NAME>__LOCAL_SEED_URL`
    /// → builtin `http://127.0.0.1:3000/__edgezero/config/seed`.
    #[arg(long)]
    pub seed_url: Option<String>,
    /// Logical config store id to push to. Defaults to the
    /// `[stores.config].default` (or the only declared id when
    /// `[stores.config].ids` has length 1).
    #[arg(long)]
    pub store: Option<String>,
}

/// Arguments for the `config validate` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ConfigValidateArgs {
    /// Path to the typed app-config file (default: `<app_name>.toml`
    /// resolved from the manifest's `[app].name`, next to the manifest).
    #[arg(long)]
    pub app_config: Option<PathBuf>,
    /// Path to the manifest (default: `edgezero.toml`).
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// Skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the
    /// typed app-config. The default loads the overlay so validation
    /// sees the same values the runtime would.
    #[arg(long)]
    pub no_env: bool,
    /// Strict mode: additionally check capability-aware completeness
    /// for the declared adapter set and well-formed handler paths.
    #[arg(long)]
    pub strict: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_derives_default() {
        let args = BuildArgs::default();
        assert!(args.adapter.is_empty());
        assert!(args.adapter_args.is_empty());
    }

    #[test]
    fn new_args_derives_default() {
        let args = NewArgs::default();
        assert!(args.name.is_empty());
        assert!(args.dir.is_none());
    }

    #[test]
    fn missing_required_adapter_returns_error() {
        Args::try_parse_from(["edgezero", "build"]).expect_err("missing --adapter");
    }

    #[test]
    fn parses_build_command_with_passthrough_args() {
        let args = Args::try_parse_from([
            "edgezero",
            "build",
            "--adapter",
            "fastly",
            "--",
            "--flag",
            "value",
        ])
        .expect("parse build");
        let Command::Build(BuildArgs {
            adapter,
            adapter_args,
        }) = args.cmd
        else {
            panic!("expected Command::Build");
        };
        assert_eq!(adapter, "fastly");
        assert_eq!(adapter_args, vec!["--flag", "value"]);
    }

    #[test]
    fn parses_new_command_with_defaults() {
        let args = Args::try_parse_from(["edgezero", "new", "demo-app"]).expect("parse new");
        let Command::New(new_args) = args.cmd else {
            panic!("expected Command::New");
        };
        assert_eq!(new_args.name, "demo-app");
        assert!(new_args.dir.is_none());
    }

    #[test]
    fn config_validate_parses_with_strict() {
        let args = Args::try_parse_from(["edgezero", "config", "validate", "--strict"])
            .expect("parse config validate --strict");
        let Command::Config(ConfigCmd::Validate(validate)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Validate)");
        };
        assert!(validate.strict);
        assert!(!validate.no_env);
        assert_eq!(validate.manifest, PathBuf::from("edgezero.toml"));
        assert!(validate.app_config.is_none());
    }

    #[test]
    fn config_validate_parses_explicit_paths_and_no_env() {
        let args = Args::try_parse_from([
            "edgezero",
            "config",
            "validate",
            "--manifest",
            "custom/edgezero.toml",
            "--app-config",
            "custom/my-app.toml",
            "--no-env",
        ])
        .expect("parse config validate with overrides");
        let Command::Config(ConfigCmd::Validate(validate)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Validate)");
        };
        assert_eq!(validate.manifest, PathBuf::from("custom/edgezero.toml"));
        assert_eq!(
            validate.app_config,
            Some(PathBuf::from("custom/my-app.toml"))
        );
        assert!(validate.no_env);
        assert!(!validate.strict);
    }

    #[test]
    fn config_validate_args_defaults() {
        let args = ConfigValidateArgs::default();
        assert_eq!(args.manifest, PathBuf::new());
        assert!(args.app_config.is_none());
        assert!(!args.strict);
        assert!(!args.no_env);
    }

    #[test]
    fn auth_login_parses_with_adapter() {
        let args = Args::try_parse_from(["edgezero", "auth", "login", "--adapter", "cloudflare"])
            .expect("parse auth login --adapter cloudflare");
        let Command::Auth(AuthArgs {
            sub: AuthSub::Login { adapter },
        }) = args.cmd
        else {
            panic!("expected Command::Auth(AuthSub::Login)");
        };
        assert_eq!(adapter, "cloudflare");
    }

    #[test]
    fn auth_logout_parses_with_adapter() {
        let args = Args::try_parse_from(["edgezero", "auth", "logout", "--adapter", "fastly"])
            .expect("parse `auth logout --adapter fastly`");
        let Command::Auth(AuthArgs {
            sub: AuthSub::Logout { adapter },
        }) = args.cmd
        else {
            panic!("expected Command::Auth(AuthSub::Logout)");
        };
        assert_eq!(adapter, "fastly");
    }

    #[test]
    fn auth_status_parses_with_adapter() {
        let args = Args::try_parse_from(["edgezero", "auth", "status", "--adapter", "spin"])
            .expect("parse `auth status --adapter spin`");
        let Command::Auth(AuthArgs {
            sub: AuthSub::Status { adapter },
        }) = args.cmd
        else {
            panic!("expected Command::Auth(AuthSub::Status)");
        };
        assert_eq!(adapter, "spin");
    }

    #[test]
    fn auth_requires_adapter() {
        Args::try_parse_from(["edgezero", "auth", "login"])
            .expect_err("`auth login` without --adapter must error");
    }

    #[test]
    fn provision_parses_with_adapter_and_dry_run() {
        let args = Args::try_parse_from([
            "edgezero",
            "provision",
            "--adapter",
            "cloudflare",
            "--dry-run",
        ])
        .expect("parse provision --adapter cloudflare --dry-run");
        let Command::Provision(provision) = args.cmd else {
            panic!("expected Command::Provision");
        };
        assert_eq!(provision.adapter, "cloudflare");
        assert!(provision.dry_run);
        assert_eq!(provision.manifest, PathBuf::from("edgezero.toml"));
    }

    #[test]
    fn provision_requires_adapter() {
        Args::try_parse_from(["edgezero", "provision"])
            .expect_err("`provision` without --adapter must error");
    }

    #[test]
    fn config_push_parses_with_adapter_and_defaults() {
        let args = Args::try_parse_from(["edgezero", "config", "push", "--adapter", "axum"])
            .expect("parse config push --adapter axum");
        let Command::Config(ConfigCmd::Push(push)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Push)");
        };
        assert_eq!(push.adapter, "axum");
        assert!(!push.dry_run);
        assert!(!push.no_env);
        assert!(push.store.is_none());
        assert!(push.app_config.is_none());
        assert_eq!(push.manifest, PathBuf::from("edgezero.toml"));
    }

    #[test]
    fn config_push_parses_explicit_paths_store_and_flags() {
        let args = Args::try_parse_from([
            "edgezero",
            "config",
            "push",
            "--adapter",
            "cloudflare",
            "--manifest",
            "custom/edgezero.toml",
            "--app-config",
            "custom/my-app.toml",
            "--store",
            "app_config",
            "--no-env",
            "--dry-run",
        ])
        .expect("parse config push with overrides");
        let Command::Config(ConfigCmd::Push(push)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Push)");
        };
        assert_eq!(push.adapter, "cloudflare");
        assert_eq!(push.manifest, PathBuf::from("custom/edgezero.toml"));
        assert_eq!(push.app_config, Some(PathBuf::from("custom/my-app.toml")));
        assert_eq!(push.store.as_deref(), Some("app_config"));
        assert!(push.no_env);
        assert!(push.dry_run);
    }

    #[test]
    fn config_push_requires_adapter() {
        Args::try_parse_from(["edgezero", "config", "push"])
            .expect_err("`config push` without --adapter must error");
    }
}
