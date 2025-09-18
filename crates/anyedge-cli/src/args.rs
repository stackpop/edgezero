use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "anyedge", about = "AnyEdge CLI")]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new AnyEdge app skeleton (multi-crate workspace)
    New(NewArgs),
    /// Build the project for a target edge
    Build {
        #[arg(long, default_value = "fastly")]
        provider: String,
    },
    /// Deploy to a target edge
    Deploy {
        #[arg(long, default_value = "fastly")]
        provider: String,
    },
    /// Run a local simulation (provider-specific)
    Serve {
        #[arg(long, default_value = "fastly")]
        provider: String,
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
    /// Force using a local path dependency to anyedge-core (if available)
    #[arg(long)]
    pub local_core: bool,
}
