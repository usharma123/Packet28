use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ShardArgs {
    #[command(subcommand)]
    pub command: ShardCommands,
}

#[derive(Subcommand)]
pub enum ShardCommands {
    /// Plan test shards for CI runners
    Plan(ShardPlanArgs),
}

#[derive(Args)]
pub struct ShardPlanArgs {
    /// Number of shards
    #[arg(long)]
    pub shards: usize,

    /// Input tests file
    #[arg(long)]
    pub tests_file: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Directory for shard output files
    #[arg(long)]
    pub write_files: Option<String>,
}

pub fn run(args: ShardArgs, _config_path: &str) -> Result<i32> {
    match args.command {
        ShardCommands::Plan(_plan) => anyhow::bail!("`covy shard plan` is not implemented yet"),
    }
}
