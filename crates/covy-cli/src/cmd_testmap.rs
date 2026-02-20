use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct TestmapArgs {
    #[command(subcommand)]
    pub command: TestmapCommands,
}

#[derive(Subcommand)]
pub enum TestmapCommands {
    /// Build test impact map artifacts
    Build(TestmapBuildArgs),
}

#[derive(Args)]
pub struct TestmapBuildArgs {
    /// Input manifest glob(s)
    #[arg(long)]
    pub manifest: Vec<String>,

    /// Output test map path
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub output: String,

    /// Output timing map path
    #[arg(long, default_value = ".covy/state/testtimings.bin")]
    pub timings_output: String,
}

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    match args.command {
        TestmapCommands::Build(_build) => {
            anyhow::bail!("`covy testmap build` is not implemented yet")
        }
    }
}
