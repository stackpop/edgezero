use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "edgezero", about = "EdgeZero CLI")]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build the project for a target edge.
    Build(BuildArgs),
    /// Run the example app locally on the axum demo server.
    Demo,
    /// Deploy to a target edge.
    Deploy(DeployArgs),
    /// Create a new `EdgeZero` app skeleton (multi-crate workspace).
    New(NewArgs),
    /// Run a local simulation (adapter-specific).
    Serve(ServeArgs),
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
#[derive(clap::Args, Debug)]
pub struct NewArgs {
    /// Directory to create the app in (default: current dir).
    #[arg(long)]
    pub dir: Option<String>,
    /// Force using a local path dependency to edgezero-core (if available).
    #[arg(long)]
    pub local_core: bool,
    /// App name (e.g., my-edge-app).
    pub name: String,
}

/// Arguments for the `serve` command.
#[derive(clap::Args, Debug, Default)]
#[non_exhaustive]
pub struct ServeArgs {
    /// Target adapter name.
    #[arg(long = "adapter", required = true)]
    pub adapter: String,
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
        assert!(!new_args.local_core);
    }
}
