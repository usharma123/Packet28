use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct ImpactArgs {
    /// Base ref for diff (default: main)
    #[arg(long)]
    pub base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    pub head: Option<String>,

    /// Path to testmap state
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub testmap: String,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,

    /// Emit runnable test command
    #[arg(long)]
    pub print_command: bool,
}

pub fn run(args: ImpactArgs, config_path: &str) -> Result<i32> {
    testy_cli_common::impact::run_legacy_impact(
        testy_cli_common::impact::LegacyImpactArgs {
            base: args.base,
            head: args.head,
            testmap: args.testmap,
            json: args.json,
            print_command: args.print_command,
        },
        config_path,
    )
}
