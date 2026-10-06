use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "packet28d", version, about = "Packet28 local daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon server for one workspace root
    Serve {
        #[arg(long, default_value = ".")]
        root: String,
        /// Own the workspace packet28d.log and rotate it by size while
        /// running, instead of writing diagnostics to stderr
        #[arg(long)]
        managed_log: bool,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve {
            root,
            managed_log: true,
        } => packet28d::serve_with_managed_log(PathBuf::from(root)),
        Commands::Serve {
            root,
            managed_log: false,
        } => packet28d::serve(PathBuf::from(root)),
    }
}
