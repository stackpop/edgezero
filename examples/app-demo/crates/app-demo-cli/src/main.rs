//! `app-demo` CLI — built on the `edgezero-cli` library.
//!
//! Reuses the built-in `edgezero` commands via the `edgezero_cli`
//! library. This is the canonical example of a downstream project
//! building its own CLI binary on the `EdgeZero` substrate.

use app_demo_core::config::AppDemoConfig;
use clap::{Parser, Subcommand};
use edgezero_cli::args::{
    AuthArgs, BuildArgs, ConfigValidateArgs, DeployArgs, NewArgs, ProvisionArgs, ServeArgs,
};

#[derive(Parser, Debug)]
#[command(name = "app-demo-cli", about = "app-demo edge CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Sign in / out / status against the adapter's native CLI
    /// (`wrangler` / `fastly` / `spin`). See spec §11.
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
    /// `[stores.<kind>].ids` (spec §12).
    Provision(ProvisionArgs),
    /// Run a local simulation (adapter-specific).
    Serve(ServeArgs),
}

/// Mirrors `edgezero_cli::args::ConfigCmd` but dispatches `validate`
/// to the **typed** validator parameterised over `AppDemoConfig` —
/// the downstream project owns the struct, so it can enforce the
/// typed deserialise, `validator` rules, and `#[secret]` /
/// `#[secret(store_ref)]` checks the raw default-binary path skips
/// (spec §10).
#[derive(Subcommand, Debug)]
enum AppDemoConfigCmd {
    /// Validate `edgezero.toml` and `app-demo.toml` against the
    /// typed `AppDemoConfig` contract.
    Validate(ConfigValidateArgs),
}

fn main() {
    use std::process;

    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Cmd::Auth(args) => edgezero_cli::run_auth(&args),
        Cmd::Build(args) => edgezero_cli::run_build(&args),
        Cmd::Config(AppDemoConfigCmd::Validate(args)) => {
            edgezero_cli::run_config_validate_typed::<AppDemoConfig>(&args)
        }
        Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
        Cmd::New(args) => edgezero_cli::run_new(&args),
        Cmd::Provision(args) => edgezero_cli::run_provision(&args),
        Cmd::Serve(args) => edgezero_cli::run_serve(&args),
    };
    if let Err(err) = result {
        log::error!("[app-demo] {err}");
        process::exit(1);
    }
}
