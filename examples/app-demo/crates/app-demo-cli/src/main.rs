//! `app-demo` CLI — built on the `edgezero-cli` library.
//!
//! This binary reuses every built-in `edgezero` command via the
//! `edgezero_cli` library and is the place to add your own
//! subcommands. The `Config` arm dispatches the **typed** validate,
//! push, and diff paths, parameterised over `AppDemoConfig` —
//! the struct your `app-demo-core` crate owns. The default
//! `edgezero` binary runs the *raw* paths because it has no typed
//! struct in scope; a downstream CLI like this one upgrades to
//! typed so `validator` rules, `#[secret]` / `#[secret(store_ref)]`
//! checks, and the Spin namespace collision check all run.

use app_demo_core::config::AppDemoConfig;
use clap::{Parser, Subcommand};
use edgezero_cli::args::{
    AuthArgs, BuildArgs, ConfigDiffArgs, ConfigPushArgs, ConfigValidateArgs, DeployArgs, NewArgs,
    ProvisionArgs, ServeArgs,
};
use edgezero_cli::DiffExit;

#[derive(Parser, Debug)]
#[command(name = "app-demo-cli", about = "app-demo edge CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Sign in / out / status against the adapter's native CLI
    /// (`wrangler` / `fastly` / `spin`). See spec.
    Auth(AuthArgs),
    /// Build the project for a target edge.
    Build(BuildArgs),
    /// Inspect or mutate the typed `app-demo.toml` app config.
    #[command(subcommand)]
    Config(AppDemoConfigCmd),
    /// Deploy to a target edge.
    Deploy(DeployArgs),
    /// Create a new `EdgeZero` app skeleton.
    New(NewArgs),
    /// Create the platform resources backing the declared
    /// `[stores.<kind>].ids`.
    Provision(ProvisionArgs),
    /// Run a local simulation (adapter-specific).
    Serve(ServeArgs),
}

/// Mirrors `edgezero_cli::args::ConfigCmd` but dispatches `validate`,
/// `push`, and `diff` to the **typed** entry points parameterised over
/// `AppDemoConfig` — the downstream project owns the struct,
/// so it can enforce the typed deserialise, `validator` rules, and
/// `#[secret]` / `#[secret(store_ref)]` checks the raw default-binary
/// path skips.
#[derive(Subcommand, Debug)]
enum AppDemoConfigCmd {
    /// Diff `app-demo.toml` against the live (or local-emulator) config
    /// store and report changes. Exits 0 (no changes), 1 (changes with
    /// `--exit-code`), or 2 (unsupported / error).
    Diff(ConfigDiffArgs),
    /// Push `app-demo.toml` as a single blob envelope to the
    /// adapter's config store. The blob carries every field verbatim
    /// (per spec 3.3 Model A — `#[secret]` fields store the key NAME,
    /// resolved at runtime); a SHA over the canonical-form data gates
    /// drift detection.
    Push(ConfigPushArgs),
    /// Validate `edgezero.toml` and `app-demo.toml` against the
    /// typed `AppDemoConfig` contract.
    Validate(ConfigValidateArgs),
}

fn main() {
    use std::process;

    edgezero_cli::init_cli_logger();
    let result: Result<(), String> = match Args::parse().cmd {
        Cmd::Auth(args) => edgezero_cli::run_auth(&args),
        Cmd::Build(args) => edgezero_cli::run_build(&args),
        Cmd::Config(AppDemoConfigCmd::Diff(args)) => {
            // `run_config_diff_typed` returns `Result<DiffExit, String>` (not
            // `Result<(), String>`), so we can't use `?` directly here.
            // Match the Ok shape explicitly: exit(code) for non-zero codes
            // (1 = diff present with --exit-code; 2 = Unsupported).
            match edgezero_cli::run_config_diff_typed::<AppDemoConfig>(&args) {
                Ok(DiffExit { code: 0 }) => Ok(()),
                Ok(DiffExit { code }) => process::exit(code),
                Err(err) => Err(err),
            }
        }
        Cmd::Config(AppDemoConfigCmd::Push(args)) => {
            edgezero_cli::run_config_push_typed::<AppDemoConfig>(&args)
        }
        Cmd::Config(AppDemoConfigCmd::Validate(args)) => {
            edgezero_cli::run_config_validate_typed::<AppDemoConfig>(&args)
        }
        Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
        Cmd::New(args) => edgezero_cli::run_new(&args),
        Cmd::Provision(args) => edgezero_cli::run_provision_typed::<AppDemoConfig>(&args),
        Cmd::Serve(args) => edgezero_cli::run_serve(&args),
    };
    if let Err(err) = result {
        log::error!("[app-demo] {err}");
        // Exit 2 for all errors so diff errors satisfy Q10's "errors always ≥ 2" rule.
        // Push / validate errors are not behaviour-checked against the 1 vs 2 distinction.
        process::exit(2);
    }
}
