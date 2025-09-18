//! AnyEdge CLI.

#[cfg(feature = "cli")]
mod args;
#[cfg(feature = "cli")]
mod dev_server;
#[cfg(feature = "cli")]
mod generator;
#[cfg(feature = "cli")]
mod provider;
#[cfg(feature = "cli")]
mod scaffold;

#[cfg(feature = "cli")]
fn main() {
    use args::{Args, Command};
    use clap::Parser;

    let args = Args::parse();
    match args.cmd {
        Command::New(new_args) => {
            if let Err(e) = generator::generate_new(new_args) {
                eprintln!("[anyedge] new error: {e}");
                std::process::exit(1);
            }
        }
        Command::Build { provider } => {
            if let Err(err) = handle_build(&provider) {
                eprintln!("[anyedge] build error: {err}");
                std::process::exit(1);
            }
        }
        Command::Deploy { provider } => {
            if let Err(err) = handle_deploy(&provider) {
                eprintln!("[anyedge] deploy error: {err}");
                std::process::exit(1);
            }
        }
        Command::Serve { provider } => {
            if let Err(err) = handle_serve(&provider) {
                eprintln!("[anyedge] serve error: {err}");
                std::process::exit(1);
            }
        }
        Command::Dev => {
            dev_server::run_dev();
        }
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("anyedge-cli built without `cli` feature. Rebuild with `--features cli`.");
}

#[cfg(feature = "cli")]
fn handle_build(provider: &str) -> Result<(), String> {
    provider::Provider::parse(provider)?.build()
}

#[cfg(feature = "cli")]
fn handle_deploy(provider: &str) -> Result<(), String> {
    provider::Provider::parse(provider)?.deploy()
}

#[cfg(feature = "cli")]
fn handle_serve(provider: &str) -> Result<(), String> {
    provider::Provider::parse(provider)?.serve()
}
