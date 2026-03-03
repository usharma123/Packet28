mod cmd_analyze;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "diffy",
    version,
    about = "Diff-focused coverage and diagnostics analysis"
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
    /// Analyze a git diff and evaluate quality gate
    Analyze(cmd_analyze::AnalyzeArgs),
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Analyze(args) => cmd_analyze::run(args, &cli.config),
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
