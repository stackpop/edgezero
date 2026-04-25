use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "edgezero", about = "EdgeZero CLI")]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new EdgeZero app skeleton (multi-crate workspace)
    New(NewArgs),
    /// Build the project for a target edge
    Build {
        #[arg(long = "adapter", required = true)]
        adapter: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        adapter_args: Vec<String>,
    },
    /// Deploy to a target edge
    Deploy {
        #[arg(long = "adapter", required = true)]
        adapter: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        adapter_args: Vec<String>,
    },
    /// Run a local simulation (adapter-specific)
    Serve {
        #[arg(long = "adapter", required = true)]
        adapter: String,
    },
    /// Run a local simulation (if available)
    Dev,
}

#[derive(clap::Args, Debug)]
pub struct NewArgs {
    /// App name (e.g., my-edge-app)
    pub name: String,
    /// Directory to create the app in (default: current dir)
    #[arg(long)]
    pub dir: Option<String>,
    /// Force using a local path dependency to edgezero-core (if available)
    #[arg(long)]
    pub local_core: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
        let Command::Build {
            adapter,
            adapter_args,
        } = args.cmd
        else {
            panic!("expected Command::Build");
        };
        assert_eq!(adapter, "fastly");
        assert_eq!(adapter_args, vec!["--flag", "value"]);
    }

    #[test]
    fn missing_required_adapter_returns_error() {
        assert!(Args::try_parse_from(["edgezero", "build"]).is_err());
    }
}
