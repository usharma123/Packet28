use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct MergeArgs {
    /// Coverage shard artifacts (supports globs)
    #[arg(long)]
    pub coverage: Vec<String>,

    /// Diagnostics shard artifacts (supports globs)
    #[arg(long)]
    pub issues: Vec<String>,

    /// Strict mode for missing/corrupt artifacts
    #[arg(long, default_value_t = true)]
    pub strict: bool,

    /// Output coverage state path
    #[arg(long, default_value = ".covy/state/latest.bin")]
    pub output_coverage: String,

    /// Output diagnostics state path
    #[arg(long, default_value = ".covy/state/issues.bin")]
    pub output_issues: String,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

pub fn run(_args: MergeArgs, _config_path: &str) -> Result<i32> {
    anyhow::bail!("`covy merge` is not implemented yet")
}
