//! `app-demo` CLI — built on the `edgezero-cli` library.
//!
//! Reuses every built-in `edgezero` command via the `edgezero_cli`
//! library. This is the canonical example of a downstream project
//! building its own CLI binary on the `EdgeZero` substrate.

use clap::{Parser, Subcommand};
use edgezero_cli::args::{BuildArgs, DeployArgs, NewArgs, ServeArgs};

#[derive(Parser, Debug)]
#[command(name = "app-demo-cli", about = "app-demo edge CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Build the project for a target edge.
    Build(BuildArgs),
    /// Run the example app locally on the axum demo server.
    Demo,
    /// Deploy to a target edge.
    Deploy(DeployArgs),
    /// Create a new `EdgeZero` app skeleton.
    New(NewArgs),
    /// Run a local simulation (adapter-specific).
    Serve(ServeArgs),
}

fn main() {
    use std::process;

    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Cmd::Build(args) => edgezero_cli::run_build(&args),
        Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
        Cmd::Demo => edgezero_cli::run_demo(),
        Cmd::New(args) => edgezero_cli::run_new(&args),
        Cmd::Serve(args) => edgezero_cli::run_serve(&args),
    };
    if let Err(err) = result {
        log::error!("[app-demo] {err}");
        process::exit(1);
    }
}
