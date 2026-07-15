use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Shown in `--help` and printed to stderr when the bundled binary
/// receives a `config push` or `config diff` invocation that requires
/// a typed app-config struct (`C`).  Downstream CLIs own that struct
/// and re-expose the real subcommands.
pub const STUB_POINTER_AFTER_HELP: &str = "\
This command requires a typed app-config struct (`C`) and runs from your generated downstream \
CLI, not the bundled `edgezero` binary. Run `<your-app>-cli config push` (or `... diff`) \
instead. See `<your-app>-cli config push --help`.";

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
    #[command(subcommand, after_help = crate::args::STUB_POINTER_AFTER_HELP)]
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

/// Subcommands under `edgezero config …`.
///
/// In the bundled `edgezero` binary, `push` and `diff` are stubs that
/// print a pointer to the downstream typed CLI and exit 2.  `validate`
/// is the only subcommand that runs in-band here because it does not
/// require a typed app-config struct.
#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Diff the typed `<name>.toml` against the live config store.
    /// (Bundled `edgezero` stub — see after-help for the typed CLI.)
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Diff(ConfigCmdStubArgs),
    /// Reclaim chunk entries in the adapter's config store that no live
    /// config pointer references.
    ///
    /// Deliberately NOT part of `config push`. On an eventually-consistent
    /// store a chunk may only be deleted once the pointer that referenced it
    /// has stopped being served everywhere — and the platform may record no
    /// such timestamp (Fastly does not). Only YOU know your deploy history,
    /// so `--older-than` is your assertion: "nothing created before this is
    /// still being served".
    ///
    /// SAFE BY DEFAULT: without `--yes` this only reports what it would
    /// delete. Nothing is removed until you pass `--yes`.
    Gc(ConfigGcArgs),
    /// Push the typed `<name>.toml` as a single blob envelope to the
    /// adapter's config store. The blob carries every field verbatim
    /// (per spec 3.3 Model A — `#[secret]` fields store the key NAME,
    /// resolved at runtime); a SHA over the canonical-form data gates
    /// drift detection.
    /// (Bundled `edgezero` stub — see after-help for the typed CLI.)
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Push(ConfigCmdStubArgs),
    /// Validate `edgezero.toml` and the typed `<name>.toml` against the
    /// manifest / app-config / Spin-key contract.
    Validate(ConfigValidateArgs),
}

/// Hidden catch-all argument sink for the bundled stub variants of
/// `config push` and `config diff`.  Absorbs any flags the user types
/// so clap does not error before we can print the pointer text (3.2.2).
#[derive(clap::Args, Debug)]
pub struct ConfigCmdStubArgs {
    /// Hidden catch-all sink (see spec 3.2.2).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub trailing: Vec<String>,
}

/// Arguments for `config gc`.
///
/// Unlike `push` / `diff`, `gc` needs no typed app-config: it reclaims
/// unreferenced chunk entries by inspecting the store, so it runs in-band in
/// the bundled `edgezero` binary.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigGcArgs {
    /// Adapter whose config store to reclaim (e.g. `fastly`).
    #[arg(long)]
    pub adapter: String,
    /// Path to `edgezero.toml`.
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// Do not overlay `EDGEZERO__*` environment variables.
    #[arg(long)]
    pub no_env: bool,
    /// Only reclaim entries older than this. YOUR SAFETY ASSERTION: nothing
    /// created before this is still being served. Accepts `s`/`m`/`h`/`d`
    /// suffixes (e.g. `7d`, `24h`, `90m`); a bare number means seconds.
    #[arg(long, default_value = "7d")]
    pub older_than: String,
    /// Override the config-store id (defaults to the manifest's).
    #[arg(long)]
    pub store: Option<String>,
    /// Actually delete. Without this, `gc` only reports what it WOULD delete.
    #[arg(long)]
    pub yes: bool,
}

impl Default for ConfigGcArgs {
    #[inline]
    fn default() -> Self {
        Self {
            adapter: String::new(),
            manifest: PathBuf::from("edgezero.toml"),
            no_env: false,
            older_than: "7d".to_owned(),
            store: None,
            yes: false,
        }
    }
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
#[derive(clap::Args, Debug)]
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

impl Default for ProvisionArgs {
    /// Match clap's `#[arg(default_value = "edgezero.toml")]` so
    /// library callers using `ProvisionArgs { adapter: ..,
    /// ..Default::default() }` get the same `manifest` clap's CLI
    /// parser would write. Without this manual impl, the derived
    /// `Default` would set `manifest` to `PathBuf::new()` (empty
    /// string), and downstream `ManifestLoader::from_path("")` would
    /// fail with a confusing "is a directory" / "no such file"
    /// error.
    #[inline]
    fn default() -> Self {
        Self {
            adapter: String::new(),
            dry_run: false,
            manifest: default_manifest_path(),
        }
    }
}

/// Arguments for the `serve` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ServeArgs {
    /// Target adapter name.
    #[arg(long = "adapter", required = true)]
    pub adapter: String,
}

/// Output format for `config diff`.
#[derive(clap::ValueEnum, Clone, Debug, Default, PartialEq)]
pub enum DiffFormat {
    /// Machine-readable JSON object with `local_sha256`, `remote_sha256`,
    /// `added`, `removed`, `changed` fields (per spec 8.1.3).
    Json,
    /// Machine-readable structured representation (key/old/new triples).
    Structured,
    /// POSIX unified-diff text (default).
    #[default]
    Unified,
}

/// Arguments for the `config diff` command.
///
/// Used by downstream typed CLIs that wire
/// `run_config_diff_typed::<C>`.  The bundled `edgezero` binary exposes
/// a `ConfigCmdStubArgs` catch-all instead and redirects to the typed
/// CLI at runtime.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigDiffArgs {
    /// Target adapter name.
    #[arg(long, required = true)]
    pub adapter: String,
    /// Path to the typed app-config file (default: `<app_name>.toml`
    /// resolved from the manifest's `[app].name`, next to the manifest).
    #[arg(long)]
    pub app_config: Option<PathBuf>,
    /// Exit with a non-zero code when changes exist (for CI gating).
    #[arg(long)]
    pub exit_code: bool,
    /// Output format for the diff.
    #[arg(long, default_value = "unified")]
    pub format: DiffFormat,
    /// Override the default key — 5.4.
    #[arg(long)]
    pub key: Option<String>,
    /// Diff against the adapter's local-emulator state instead of the
    /// live platform.
    #[arg(long)]
    pub local: bool,
    /// Path to the manifest (default: `edgezero.toml`).
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// Skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the
    /// typed app-config.
    #[arg(long)]
    pub no_env: bool,
    /// Path to the adapter's runtime configuration file.
    #[arg(long)]
    pub runtime_config: Option<PathBuf>,
    /// Logical config store id to diff against. Defaults to the
    /// `[stores.config].default` (or the only declared id when
    /// `[stores.config].ids` has length 1).
    #[arg(long)]
    pub store: Option<String>,
}

impl Default for ConfigDiffArgs {
    /// See `ProvisionArgs::default` — same rationale.
    #[inline]
    fn default() -> Self {
        Self {
            adapter: String::new(),
            app_config: None,
            exit_code: false,
            format: DiffFormat::Unified,
            key: None,
            local: false,
            manifest: default_manifest_path(),
            no_env: false,
            runtime_config: None,
            store: None,
        }
    }
}

/// Arguments for the `config push` command.
///
/// Used by downstream typed CLIs that wire
/// `run_config_push_typed::<C>`.  The bundled `edgezero` binary exposes
/// a `ConfigCmdStubArgs` catch-all instead and redirects to the typed
/// CLI at runtime.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "clap args struct: each bool is a distinct CLI flag \
              (dry_run, local, no_diff, no_env, yes); a state machine \
              would be inappropriate here"
)]
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
    /// Override the default key — 5.4.
    #[arg(long)]
    pub key: Option<String>,
    /// Push to the adapter's local-emulator state instead of the live
    /// platform. For Fastly this edits `[local_server.config_stores]`
    /// in the adapter's `fastly.toml` (Viceroy reads it on startup);
    /// for Cloudflare it runs `wrangler kv bulk put --local` so
    /// writes land in `.wrangler/state`. Axum's push is already
    /// local-only, so `--local` is a no-op there. For Spin, `--local`
    /// suppresses Fermyon Cloud auto-detection so the push writes
    /// directly to Spin's local `SQLite` KV file
    /// (`<spin.toml dir>/.spin/sqlite_key_value.db`) even when the
    /// manifest's deploy command shells to `spin deploy`.
    #[arg(long)]
    pub local: bool,
    /// Path to the manifest (default: `edgezero.toml`).
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// Skip the inline diff render.
    #[arg(long)]
    pub no_diff: bool,
    /// Skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the
    /// typed app-config. The default loads the overlay so the runtime
    /// and the push see the same resolved values.
    #[arg(long)]
    pub no_env: bool,
    /// Path to the adapter's runtime configuration file. Currently
    /// only honoured by Spin, which reads
    /// `[key_value_store.<label>]` stanzas to dispatch
    /// `config push --adapter spin` to the right backend writer
    /// (`type = "spin"` → direct `SQLite` write; redis/azure-*/etc. →
    /// errors pointing at the native backend CLI). Default:
    /// `runtime-config.toml` next to the adapter manifest.
    #[arg(long)]
    pub runtime_config: Option<PathBuf>,
    /// Logical config store id to push to. Defaults to the
    /// `[stores.config].default` (or the only declared id when
    /// `[stores.config].ids` has length 1).
    #[arg(long)]
    pub store: Option<String>,
    /// Skip the inline diff prompt and write unconditionally.
    #[arg(long, short)]
    pub yes: bool,
}

impl Default for ConfigPushArgs {
    /// See `ProvisionArgs::default` — same rationale.
    #[inline]
    fn default() -> Self {
        Self {
            adapter: String::new(),
            app_config: None,
            dry_run: false,
            key: None,
            local: false,
            manifest: default_manifest_path(),
            no_diff: false,
            no_env: false,
            runtime_config: None,
            store: None,
            yes: false,
        }
    }
}

/// Arguments for the `config validate` command.
#[derive(clap::Args, Debug)]
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

impl Default for ConfigValidateArgs {
    /// See `ProvisionArgs::default` — same rationale.
    #[inline]
    fn default() -> Self {
        Self {
            app_config: None,
            manifest: default_manifest_path(),
            no_env: false,
            strict: false,
        }
    }
}

/// Default `manifest` value shared by all args structs that have
/// the `#[arg(default_value = "edgezero.toml")]` clap attribute.
/// Centralised here so the value stays in sync across the clap
/// attribute (which can only be a literal) and the manual `Default`
/// impls above.
fn default_manifest_path() -> PathBuf {
    PathBuf::from("edgezero.toml")
}

/// Parse a human duration (`7d`, `24h`, `90m`, `30s`, or bare seconds) into
/// seconds.
///
/// # Errors
///
/// Returns `Err` when the value is empty, non-numeric, or carries an unknown
/// suffix — a destructive command must never guess at its safety threshold.
#[must_use = "the parsed threshold gates a destructive command"]
#[inline]
pub fn parse_duration_secs(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    let unknown = || {
        format!(
            "could not parse `--older-than {raw}`; expected e.g. `7d`, `24h`, `90m`, `30s`, or a number of seconds"
        )
    };
    let (digits, multiplier) = match trimmed.strip_suffix('s') {
        Some(rest) => (rest, 1_u64),
        None => match trimmed.strip_suffix('m') {
            Some(rest) => (rest, 60_u64),
            None => match trimmed.strip_suffix('h') {
                Some(rest) => (rest, 3_600_u64),
                None => match trimmed.strip_suffix('d') {
                    Some(rest) => (rest, 86_400_u64),
                    None => (trimmed, 1_u64),
                },
            },
        },
    };
    if digits.is_empty() {
        return Err(unknown());
    }
    let value: u64 = digits.parse().map_err(|_err| unknown())?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("`--older-than {raw}` overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thin wrapper so `ConfigDiffArgs` (a `clap::Args`) can be
    /// tested via real clap parsing. `ConfigDiffArgs` is `#[non_exhaustive]`
    /// so it cannot be constructed with struct-literal syntax outside this
    /// crate; the wrapper lets tests parse flags through clap directly.
    #[derive(clap::Parser)]
    struct DiffTestWrapper {
        #[command(flatten)]
        args: ConfigDiffArgs,
    }

    /// Parse `ConfigDiffArgs` from a CLI token slice via clap.
    fn parse_diff(tokens: &[&str]) -> ConfigDiffArgs {
        let mut full = vec!["bin"];
        full.extend_from_slice(tokens);
        DiffTestWrapper::try_parse_from(full)
            .expect("parse ConfigDiffArgs")
            .args
    }

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
    fn provision_args_default_manifest_matches_clap_default() {
        // PR #269 round 4 / F4: library callers using
        // `ProvisionArgs { adapter: "...", ..Default::default() }`
        // must end up with `manifest = "edgezero.toml"`, matching
        // what clap writes when no `--manifest` is passed on the
        // CLI. Pre-fix the derived `Default` produced
        // `PathBuf::new()` (empty) and `ManifestLoader::from_path("")`
        // erred with a confusing "no such file" message.
        let args = ProvisionArgs::default();
        assert_eq!(args.manifest, PathBuf::from("edgezero.toml"));
        assert!(args.adapter.is_empty());
        assert!(!args.dry_run);
    }

    #[test]
    fn config_push_args_default_manifest_matches_clap_default() {
        // Manual Default must stay in sync with every field.
        // If the impl were to miss any of them the struct would fail to compile
        // due to #[non_exhaustive] preventing struct-literal construction outside
        // this crate, and this test confirms Default values are sensible.
        let args = ConfigPushArgs::default();
        assert_eq!(args.manifest, PathBuf::from("edgezero.toml"));
        assert!(args.adapter.is_empty());
        assert!(args.app_config.is_none());
        assert!(!args.dry_run);
        assert!(args.key.is_none());
        assert!(!args.local);
        assert!(!args.no_diff);
        assert!(!args.no_env);
        assert!(args.runtime_config.is_none());
        assert!(args.store.is_none());
        assert!(!args.yes);
    }

    #[test]
    fn config_validate_args_default_manifest_matches_clap_default() {
        let args = ConfigValidateArgs::default();
        assert_eq!(args.manifest, PathBuf::from("edgezero.toml"));
        assert!(args.app_config.is_none());
        assert!(!args.no_env);
        assert!(!args.strict);
    }

    #[test]
    fn default_manifest_path_matches_clap_literal() {
        // Lock the shared helper to the same string the clap
        // attributes use, so a future bump only needs one site
        // updated (and this test catches drift if not).
        assert_eq!(default_manifest_path(), PathBuf::from("edgezero.toml"));
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
        // Post-F4 (PR #269 round 4): library callers using
        // `..Default::default()` now get the same `manifest`
        // value clap writes when no `--manifest` is passed
        // (`edgezero.toml`), instead of the empty-PathBuf the
        // derived `Default` produced pre-fix.
        let args = ConfigValidateArgs::default();
        assert_eq!(args.manifest, PathBuf::from("edgezero.toml"));
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

    // ── config push / diff stub tests (12.8 + 12.11) ──────────────────

    /// Bundled binary: bare `config push` parses to the stub variant.
    /// The catch-all absorbs nothing; trailing is empty.
    #[test]
    fn config_push_stub_parses_bare() {
        let args = Args::try_parse_from(["edgezero", "config", "push"])
            .expect("config push stub \u{2014} no args required");
        let Command::Config(ConfigCmd::Push(stub)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Push)");
        };
        assert!(stub.trailing.is_empty());
    }

    /// Bundled binary: `config push` with flags is absorbed by the
    /// trailing catch-all and still parses without error.
    #[test]
    fn config_push_stub_absorbs_flags() {
        let args = Args::try_parse_from([
            "edgezero",
            "config",
            "push",
            "--adapter",
            "axum",
            "--dry-run",
        ])
        .expect("config push stub absorbs typed flags");
        let Command::Config(ConfigCmd::Push(stub)) = args.cmd else {
            panic!("expected Command::Config(ConfigCmd::Push)");
        };
        // The catch-all absorbs unrecognised flags as trailing tokens.
        assert!(!stub.trailing.is_empty());
    }

    /// Bundled binary: `config diff` parses to the `Diff` stub variant.
    #[test]
    fn config_diff_stub_parses_bare() {
        let args = Args::try_parse_from(["edgezero", "config", "diff"])
            .expect("config diff stub \u{2014} no args required");
        assert!(matches!(args.cmd, Command::Config(ConfigCmd::Diff(_))));
    }

    /// 12.11 — `ConfigPushArgs` new flags: `--yes` / `-y` / `--no-diff`
    /// / `--dry-run` parse correctly on the *downstream* typed struct.
    /// (The bundled binary uses `ConfigCmdStubArgs`; these tests cover the
    /// struct fields directly via `Default` + mutation, since clap can only
    /// parse `ConfigPushArgs` when wired as `Push(ConfigPushArgs)` in a
    /// downstream `ConfigCmd`.)
    #[test]
    fn config_push_args_yes_default_is_false() {
        assert!(!ConfigPushArgs::default().yes);
    }

    #[test]
    fn config_push_args_no_diff_default_is_false() {
        assert!(!ConfigPushArgs::default().no_diff);
    }

    #[test]
    fn config_push_args_key_default_is_none() {
        assert!(ConfigPushArgs::default().key.is_none());
    }

    // ── ConfigDiffArgs parser-roundtrip tests (12.11) ──────────────────

    /// Default `ConfigDiffArgs` has correct zero-values.
    /// KEEP as struct-literal sanity check — this is a Default-impl pin,
    /// NOT a clap parse test.
    #[test]
    fn config_diff_args_default_values() {
        let args = ConfigDiffArgs::default();
        assert_eq!(args.manifest, PathBuf::from("edgezero.toml"));
        assert!(args.adapter.is_empty());
        assert!(args.app_config.is_none());
        assert!(!args.exit_code);
        assert_eq!(args.format, DiffFormat::Unified);
        assert!(args.key.is_none());
        assert!(!args.local);
        assert!(!args.no_env);
        assert!(args.runtime_config.is_none());
        assert!(args.store.is_none());
    }

    /// `--format unified` parses → `DiffFormat::Unified`.
    #[test]
    fn config_diff_args_format_unified() {
        let args = parse_diff(&["--adapter", "spin", "--format", "unified"]);
        assert_eq!(args.format, DiffFormat::Unified);
    }

    /// `--format structured` parses → `DiffFormat::Structured`.
    #[test]
    fn config_diff_args_format_structured() {
        let args = parse_diff(&["--adapter", "spin", "--format", "structured"]);
        assert_eq!(args.format, DiffFormat::Structured);
    }

    /// `--format json` parses → `DiffFormat::Json`.
    #[test]
    fn config_diff_args_format_json() {
        let args = parse_diff(&["--adapter", "spin", "--format", "json"]);
        assert_eq!(args.format, DiffFormat::Json);
    }

    /// Default (no `--format`) → `DiffFormat::Unified`.
    #[test]
    fn config_diff_args_format_default_is_unified() {
        let args = parse_diff(&["--adapter", "spin"]);
        assert_eq!(args.format, DiffFormat::Unified);
    }

    /// `--exit-code` parses → `exit_code: true`. Default → `false`.
    #[test]
    fn config_diff_args_exit_code_flag() {
        let with_flag = parse_diff(&["--adapter", "spin", "--exit-code"]);
        assert!(with_flag.exit_code);
        let without_flag = parse_diff(&["--adapter", "spin"]);
        assert!(!without_flag.exit_code);
    }

    /// `--local` parses → `local: true`.
    #[test]
    fn config_diff_args_local_flag() {
        let args = parse_diff(&["--adapter", "spin", "--local"]);
        assert!(args.local);
    }

    /// `--key staging` parses → `key: Some("staging")`.
    #[test]
    fn config_diff_args_key_field() {
        let args = parse_diff(&["--adapter", "spin", "--key", "staging"]);
        assert_eq!(args.key.as_deref(), Some("staging"));
    }

    /// `--no-env` parses → `no_env: true`.
    #[test]
    fn config_diff_args_no_env_flag() {
        let args = parse_diff(&["--adapter", "spin", "--no-env"]);
        assert!(args.no_env);
    }

    /// `--runtime-config path/to/rc.toml` parses → `runtime_config: Some(PathBuf("path/to/rc.toml"))`.
    #[test]
    fn config_diff_args_runtime_config_field() {
        let args = parse_diff(&["--adapter", "spin", "--runtime-config", "path/to/rc.toml"]);
        assert_eq!(args.runtime_config, Some(PathBuf::from("path/to/rc.toml")));
    }

    /// `--adapter spin --store config_v2` parses → `adapter: "spin"`, `store: Some("config_v2")`.
    #[test]
    fn config_diff_args_store_field() {
        let args = parse_diff(&["--adapter", "spin", "--store", "config_v2"]);
        assert_eq!(args.adapter, "spin");
        assert_eq!(args.store.as_deref(), Some("config_v2"));
    }

    /// Verify `--help` output for the stub `push` subcommand contains
    /// the pointer text and does NOT expose `[TRAILING]`.
    #[test]
    fn config_push_stub_help_contains_pointer_and_hides_trailing() {
        use clap::CommandFactory as _;
        let mut cmd = Args::command();
        // Walk the subcommand tree to reach `config push`.
        let mut config_sub = cmd
            .find_subcommand_mut("config")
            .expect("config subcommand")
            .find_subcommand_mut("push")
            .expect("push subcommand")
            .clone();
        let help = config_sub.render_help().to_string();
        assert!(
            help.contains("typed app-config struct"),
            "pointer text missing from push help: {help}"
        );
        assert!(
            !help.contains("[TRAILING]"),
            "`[TRAILING]` placeholder leaked into push help: {help}"
        );
    }
    #[test]
    fn parse_duration_secs_accepts_suffixes_and_bare_seconds() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("90m").unwrap(), 5_400);
        assert_eq!(parse_duration_secs("24h").unwrap(), 86_400);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
        assert_eq!(parse_duration_secs("3600").unwrap(), 3_600);
        assert_eq!(parse_duration_secs("  7d  ").unwrap(), 604_800);
    }

    /// A destructive command must never guess at its safety threshold.
    #[test]
    fn parse_duration_secs_rejects_garbage() {
        parse_duration_secs("").unwrap_err();
        parse_duration_secs("soon").unwrap_err();
        parse_duration_secs("7w").unwrap_err();
        parse_duration_secs("-1d").unwrap_err();
        parse_duration_secs("d").unwrap_err();
    }
}
