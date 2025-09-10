//! AnyEdge CLI.

#[cfg(feature = "cli")]
mod args;
#[cfg(feature = "cli")]
mod dev_server;
#[cfg(feature = "cli")]
mod generator;
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
            println!("[anyedge] build for provider: {} (TODO)", provider);
        }
        Command::Deploy { provider } => {
            println!("[anyedge] deploy to provider: {} (TODO)", provider);
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
