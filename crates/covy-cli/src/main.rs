mod cmd_annotate;
mod cmd_check;
mod cmd_comment;
mod cmd_common;
mod cmd_diff;
mod cmd_doctor;
mod cmd_github;
mod cmd_impact;
mod cmd_ingest;
mod cmd_init;
mod cmd_map_paths;
mod cmd_merge;
mod cmd_pr;
mod cmd_report;
mod cmd_shard;
mod cmd_testmap;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "covy", version, about = "Universal code coverage tool")]
struct Cli {
    /// Path to config file
    #[arg(long, global = true, default_value = "covy.toml")]
    config: String,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Suppress non-essential output
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Color mode (auto/always/never)
    #[arg(long, global = true, default_value = "auto")]
    color: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// One-shot: ingest + diff + gate + report
    Check(cmd_check::CheckArgs),
    /// Ingest coverage reports
    Ingest(cmd_ingest::IngestArgs),
    /// Display coverage report
    Report(cmd_report::ReportArgs),
    /// Run PR quality gate against a diff
    Diff(cmd_diff::DiffArgs),
    /// Build and inspect test impact maps
    Testmap(cmd_testmap::TestmapArgs),
    /// Compute impacted tests from a git diff
    Impact(cmd_impact::ImpactArgs),
    /// Render PR comment markdown artifacts
    Comment(cmd_comment::CommentArgs),
    /// Generate SARIF annotations
    Annotate(cmd_annotate::AnnotateArgs),
    /// One-shot PR artifact generation
    Pr(cmd_pr::PrArgs),
    /// Initialize covy.toml and .covy/ directory
    Init(cmd_init::InitArgs),
    /// Diagnose setup and integration issues
    Doctor(cmd_doctor::DoctorArgs),
    /// Learn and inspect path mapping rules
    MapPaths(cmd_map_paths::MapPathsArgs),
    /// Plan and inspect test shard allocations
    Shard(cmd_shard::ShardArgs),
    /// Merge shard artifacts into canonical state
    Merge(cmd_merge::MergeArgs),
    /// Post coverage report as a GitHub PR comment
    GithubComment(cmd_github::GithubCommentArgs),
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let mut msg = err.to_string();
            let replacements = [
                ("--out-comment", "--output-comment"),
                ("--out-sarif", "--output-sarif"),
                ("--out-coverage", "--output-coverage"),
                ("--out-issues", "--output-issues"),
            ];
            for (legacy, canonical) in replacements {
                msg = msg.replace(legacy, canonical);
            }
            if err.use_stderr() {
                eprint!("{msg}");
            } else {
                print!("{msg}");
            }
            std::process::exit(err.exit_code());
        }
    };

    // Set up tracing
    let json_enabled = cmd_common::global_json_enabled();
    let filter = if cli.quiet || json_enabled {
        "warn"
    } else if cli.verbose {
        "covy_cli=debug,covy_core=debug,covy_ingest=debug,info"
    } else {
        "info"
    };
    let env_filter = std::env::var("COVY_LOG")
        .ok()
        .and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok())
        .or_else(|| tracing_subscriber::EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();

    // Handle color
    match cli.color.as_str() {
        "never" => colored::control::set_override(false),
        "always" => colored::control::set_override(true),
        _ => {} // auto
    }

    let result = match cli.command {
        Commands::Check(args) => cmd_check::run(args, &cli.config),
        Commands::Ingest(args) => cmd_ingest::run(args, &cli.config),
        Commands::Report(args) => cmd_report::run(args, &cli.config),
        Commands::Diff(args) => cmd_diff::run(args, &cli.config),
        Commands::Testmap(args) => cmd_testmap::run(args, &cli.config),
        Commands::Impact(args) => cmd_impact::run(args, &cli.config),
        Commands::Comment(args) => cmd_comment::run(args, &cli.config),
        Commands::Annotate(args) => cmd_annotate::run(args, &cli.config),
        Commands::Pr(args) => cmd_pr::run(args, &cli.config),
        Commands::Init(args) => cmd_init::run(args, &cli.config),
        Commands::Doctor(args) => cmd_doctor::run(args, &cli.config),
        Commands::MapPaths(args) => cmd_map_paths::run(args, &cli.config),
        Commands::Shard(args) => cmd_shard::run(args, &cli.config),
        Commands::Merge(args) => cmd_merge::run(args, &cli.config),
        Commands::GithubComment(args) => cmd_github::run(args, &cli.config),
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

    // Check if it's a CovyError with a hint
    if let Some(covy_err) = err.downcast_ref::<covy_core::CovyError>() {
        eprintln!("{} {covy_err}", "error:".red().bold());
        if let Some(hint) = covy_err.hint() {
            eprintln!("  {} {hint}", "hint:".cyan().bold());
        }
    } else {
        eprintln!("{} {err}", "error:".red().bold());
        // Print cause chain
        for cause in err.chain().skip(1) {
            eprintln!("  {} {cause}", "caused by:".dimmed());
        }
    }
}
