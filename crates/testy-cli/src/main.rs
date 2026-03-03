mod cmd_impact;
mod cmd_shard;
mod cmd_testmap;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "testy", version, about = "Test impact and sharding CLI")]
struct Cli {
    /// Path to config file
    #[arg(long, global = true, default_value = "covy.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compute impacted tests from a git diff
    Impact(cmd_impact::ImpactArgs),
    /// Plan and inspect test shard allocations
    Shard(cmd_shard::ShardArgs),
    /// Build and inspect test impact maps
    Testmap(cmd_testmap::TestmapArgs),
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Impact(args) => cmd_impact::run(args, &cli.config),
        Commands::Shard(args) => cmd_shard::run(args, &cli.config),
        Commands::Testmap(args) => cmd_testmap::run(args, &cli.config),
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
