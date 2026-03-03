use std::path::Path;

use anyhow::Result;
use clap::Args;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct ReportArgs {
    /// Output format (terminal/json)
    #[arg(short, long)]
    format: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Sort by (name/coverage)
    #[arg(long, default_value = "name")]
    sort: String,

    /// Show missing line numbers
    #[arg(long)]
    show_missing: bool,

    /// Minimum coverage threshold (exit 1 if below)
    #[arg(long)]
    min_coverage: Option<f64>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,

    /// Render diagnostics issues from .covy/state/issues.bin
    #[arg(long)]
    issues: bool,

    /// Only show files with coverage below this threshold (percent)
    #[arg(long)]
    below: Option<f64>,

    /// Print only the total coverage summary line (no per-file table)
    #[arg(long)]
    summary_only: bool,
}

pub fn run(args: ReportArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

    let format =
        if crate::cmd_common::resolve_json_output(args.json, args.format.as_deref(), "--format")? {
            "json"
        } else {
            args.format.as_deref().unwrap_or(&config.report.format)
        };

    if args.issues {
        let issue_path = Path::new(".covy/state/issues.bin");
        if !issue_path.exists() {
            anyhow::bail!(
                "No diagnostics data found at {}. Run `covy ingest --issues ...` first.",
                issue_path.display()
            );
        }

        let bytes = std::fs::read(issue_path)?;
        let diagnostics = covy_core::cache::deserialize_diagnostics(&bytes)?;

        match format {
            "json" => {
                let json = covy_core::report::render_issues_json(&diagnostics);
                println!("{json}");
            }
            _ => {
                covy_core::report::render_issues_terminal(&diagnostics, None);
            }
        }

        return Ok(0);
    }

    let input_path = args.input.as_deref().unwrap_or(".covy/state/latest.bin");
    let input_path = Path::new(input_path);

    if !input_path.exists() {
        anyhow::bail!(
            "No coverage data found at {}. Run `covy ingest` first.",
            input_path.display()
        );
    }

    let bytes = std::fs::read(input_path)?;
    let coverage = covy_core::cache::deserialize_coverage(&bytes)?;

    let show_missing = args.show_missing || config.report.show_missing;

    match format {
        "json" => {
            let json = covy_core::report::render_json(&coverage, args.below, args.summary_only);
            println!("{json}");
        }
        _ => {
            covy_core::report::render_terminal(
                &coverage,
                show_missing,
                &args.sort,
                args.below,
                args.summary_only,
            );
        }
    }

    // Check min coverage threshold
    if let Some(threshold) = args.min_coverage {
        if let Some(pct) = coverage.total_coverage_pct() {
            if pct < threshold {
                return Ok(1);
            }
        }
    }

    Ok(0)
}
