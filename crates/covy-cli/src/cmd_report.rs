use std::path::Path;

use anyhow::Result;
use clap::Args;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct ReportArgs {
    /// Output format (terminal/json)
    #[arg(short, long)]
    format: Option<String>,

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
}

pub fn run(args: ReportArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

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

    let format = args
        .format
        .as_deref()
        .unwrap_or(&config.report.format);
    let show_missing = args.show_missing || config.report.show_missing;

    match format {
        "json" => {
            let json = covy_core::report::render_json(&coverage);
            println!("{json}");
        }
        _ => {
            covy_core::report::render_terminal(&coverage, show_missing, &args.sort);
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
