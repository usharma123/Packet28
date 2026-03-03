mod cmd_common;
mod cmd_diff;
mod cmd_impact;
mod cmd_map;
mod cmd_shard;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "suite",
    version,
    about = "Umbrella platform CLI for suite domains"
)]
struct Cli {
    /// Path to config file
    #[arg(long, global = true, default_value = "covy.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Diff domain commands
    Diff(DiffArgs),
    /// Test domain commands
    Test(TestArgs),
}

#[derive(Args)]
struct DiffArgs {
    #[command(subcommand)]
    command: DiffCommands,
}

#[derive(Subcommand)]
enum DiffCommands {
    /// Analyze a git diff and evaluate quality gate
    Analyze(cmd_diff::AnalyzeArgs),
}

#[derive(Args)]
struct TestArgs {
    #[command(subcommand)]
    command: TestCommands,
}

#[derive(Subcommand)]
enum TestCommands {
    /// Compute impacted tests from a git diff
    Impact(cmd_impact::ImpactArgs),
    /// Plan test shard allocations
    Shard(cmd_shard::ShardArgs),
    /// Build test impact map artifacts
    Map(cmd_map::MapArgs),
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Diff(diff) => match diff.command {
            DiffCommands::Analyze(args) => cmd_diff::run(args, &cli.config),
        },
        Commands::Test(test) => match test.command {
            TestCommands::Impact(args) => cmd_impact::run(args, &cli.config),
            TestCommands::Shard(args) => cmd_shard::run(args, &cli.config),
            TestCommands::Map(args) => cmd_map::run(args),
        },
    };

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            display_error(&e);
            std::process::exit(2);
        }
    }
}

fn display_error(err: &anyhow::Error) {
    use colored::Colorize;

    if let Some(covy_err) = err.downcast_ref::<suite_packet_core::CovyError>() {
        eprintln!("{} {covy_err}", "error:".red().bold());
        if let Some(hint) = covy_err.hint() {
            eprintln!("  {} {hint}", "hint:".cyan().bold());
        }
    } else {
        eprintln!("{} {err}", "error:".red().bold());
        for cause in err.chain().skip(1) {
            eprintln!("  {} {cause}", "caused by:".dimmed());
        }
    }
}
