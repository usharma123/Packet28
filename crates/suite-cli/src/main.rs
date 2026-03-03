mod cmd_build;
mod cmd_common;
mod cmd_context;
mod cmd_diff;
mod cmd_guard;
mod cmd_impact;
mod cmd_map;
mod cmd_shard;
mod cmd_stack;

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
    /// Guard/policy domain commands
    Guard(GuardArgs),
    /// Context assembly domain commands
    Context(ContextArgs),
    /// Stack trace / failure log reduction commands
    Stack(StackArgs),
    /// Build diagnostics reduction commands
    Build(BuildArgs),
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

#[derive(Args)]
struct GuardArgs {
    #[command(subcommand)]
    command: GuardCommands,
}

#[derive(Subcommand)]
enum GuardCommands {
    /// Validate guard policy config (context.yaml) shape and rule syntax
    Validate(cmd_guard::ValidateArgs),
    /// Evaluate one packet against guard policy config
    Check(cmd_guard::CheckArgs),
}

#[derive(Args)]
struct ContextArgs {
    #[command(subcommand)]
    command: ContextCommands,
}

#[derive(Subcommand)]
enum ContextCommands {
    /// Merge multiple reducer packets into a bounded final packet
    Assemble(cmd_context::AssembleArgs),
}

#[derive(Args)]
struct StackArgs {
    #[command(subcommand)]
    command: StackCommands,
}

#[derive(Subcommand)]
enum StackCommands {
    /// Parse stack traces/failing logs into deduped failure packets
    Slice(cmd_stack::SliceArgs),
}

#[derive(Args)]
struct BuildArgs {
    #[command(subcommand)]
    command: BuildCommands,
}

#[derive(Subcommand)]
enum BuildCommands {
    /// Parse compiler/linter output into deduped build diagnostic packets
    Reduce(cmd_build::ReduceArgs),
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
        Commands::Guard(guard) => match guard.command {
            GuardCommands::Validate(args) => cmd_guard::run_validate(args, &cli.config),
            GuardCommands::Check(args) => cmd_guard::run_check(args, &cli.config),
        },
        Commands::Context(context) => match context.command {
            ContextCommands::Assemble(args) => cmd_context::run_assemble(args),
        },
        Commands::Stack(stack) => match stack.command {
            StackCommands::Slice(args) => cmd_stack::run(args),
        },
        Commands::Build(build) => match build.command {
            BuildCommands::Reduce(args) => cmd_build::run(args),
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
