//! `EdgeZero` CLI binary — a thin wrapper over the `edgezero_cli` library.

#[cfg(feature = "cli")]
fn main() {
    use clap::Parser as _;
    use edgezero_cli::args::{Args, Command};
    use std::process;

    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Command::Build(args) => edgezero_cli::run_build(&args),
        Command::Deploy(args) => edgezero_cli::run_deploy(&args),
        #[cfg(feature = "demo-example")]
        Command::Demo => edgezero_cli::run_demo(),
        Command::New(args) => edgezero_cli::run_new(&args),
        Command::Serve(args) => edgezero_cli::run_serve(&args),
    };
    if let Err(err) = result {
        log::error!("[edgezero] {err}");
        process::exit(1);
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    use log::LevelFilter;
    use simple_logger::SimpleLogger;
    let _logger_init = SimpleLogger::new()
        .with_level(LevelFilter::Error)
        .without_timestamps()
        .init();
    log::error!("edgezero-cli built without `cli` feature. Rebuild with `--features cli`.");
}
