//! `EdgeZero` CLI binary — a thin wrapper over the `edgezero_cli` library.

#[cfg(feature = "cli")]
fn main() {
    use clap::Parser as _;
    use edgezero_cli::args::{self, Args, Command, ConfigCmd};
    use std::process;

    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Command::Auth(cmd_args) => edgezero_cli::run_auth(&cmd_args),
        Command::Build(cmd_args) => edgezero_cli::run_build(&cmd_args),
        // `config push` and `config diff` require a typed app-config struct
        // (`C`) that only downstream CLIs own.  The bundled binary catches the
        // invocation, prints the pointer text, and exits 2 so callers can
        // distinguish "wrong binary" from a runtime error (exit 1).
        Command::Config(ConfigCmd::Push(_) | ConfigCmd::Diff(_)) => {
            #[expect(
                clippy::print_stderr,
                reason = "intentional: pointer text must reach the user even when \
                          stdout is piped; this is the only stderr write in main"
            )]
            {
                eprintln!("{}", args::STUB_POINTER_AFTER_HELP);
            };
            process::exit(2);
        }
        Command::Config(ConfigCmd::Validate(cmd_args)) => {
            edgezero_cli::run_config_validate(&cmd_args)
        }
        Command::Deploy(cmd_args) => edgezero_cli::run_deploy(&cmd_args),
        #[cfg(feature = "demo-example")]
        Command::Demo => edgezero_cli::run_demo(),
        Command::Healthcheck(cmd_args) => edgezero_cli::run_healthcheck(&cmd_args),
        Command::ActiveVersion(cmd_args) => edgezero_cli::run_active_version(&cmd_args),
        Command::New(cmd_args) => edgezero_cli::run_new(&cmd_args),
        Command::Rollback(cmd_args) => edgezero_cli::run_rollback(&cmd_args),
        Command::Provision(cmd_args) => edgezero_cli::run_provision(&cmd_args),
        Command::Serve(cmd_args) => edgezero_cli::run_serve(&cmd_args),
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
