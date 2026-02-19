use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use covy_core::config::GateConfig;
use covy_core::diagnostics::DiagnosticsData;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct DiffArgs {
    /// Base ref (default: main)
    #[arg(long)]
    base: Option<String>,

    /// Head ref (default: HEAD)
    #[arg(long)]
    head: Option<String>,

    /// Fail if changed lines coverage is below this %
    #[arg(long)]
    fail_under_changed: Option<f64>,

    /// Fail if total coverage is below this %
    #[arg(long)]
    fail_under_total: Option<f64>,

    /// Fail if new file coverage is below this %
    #[arg(long)]
    fail_under_new: Option<f64>,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Path to cached diagnostics state file (default: .covy/state/issues.bin)
    #[arg(long)]
    issues_state: Option<String>,

    /// Disable automatic diagnostics state loading when --issues is not provided
    #[arg(long)]
    no_issues_state: bool,

    /// Output format (terminal/json)
    #[arg(long, default_value = "terminal")]
    report: String,

    /// Coverage report files to ingest (instead of loading state)
    #[arg(long)]
    coverage: Vec<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,
}

pub fn run(args: DiffArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    // Load or ingest coverage data
    let mut coverage = if !args.coverage.is_empty() {
        let mut combined = covy_core::CoverageData::new();
        for path in &args.coverage {
            let data = covy_ingest::ingest_path(Path::new(path))?;
            combined.merge(&data);
        }
        combined
    } else {
        let input_path = args.input.as_deref().unwrap_or(".covy/state/latest.bin");
        let input_path = Path::new(input_path);
        if !input_path.exists() {
            anyhow::bail!(
                "No coverage data found at {}. Run `covy ingest` first or use --coverage.",
                input_path.display()
            );
        }
        let bytes = std::fs::read(input_path)?;
        covy_core::cache::deserialize_coverage(&bytes)?
    };
    covy_core::pathmap::auto_normalize_paths(&mut coverage, None);

    // Optional diagnostics
    let mut diagnostics = resolve_diagnostics(
        &args.issues,
        args.issues_state.as_deref(),
        args.no_issues_state,
    )?;
    if let Some(diag) = diagnostics.as_mut() {
        covy_core::pathmap::auto_normalize_issue_paths(diag, None);
    }

    // Get diff
    tracing::info!("Computing diff {base}..{head}");
    let diffs = covy_core::diff::git_diff(base, head)?;
    tracing::info!("Found {} changed files", diffs.len());

    // Build gate config from CLI args + config file
    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: config.gate.issues.clone(),
    };

    // Evaluate gate
    let result =
        covy_core::gate::evaluate_full_gate(&gate_config, &coverage, diagnostics.as_ref(), &diffs);

    // Output
    match args.report.as_str() {
        "json" => {
            let json = covy_core::report::render_gate_json(&result);
            println!("{json}");
        }
        _ => {
            covy_core::report::render_gate_result(&result);
            if let Some(diag) = diagnostics.as_ref() {
                covy_core::report::render_issues_terminal(diag, Some(&diffs));
            }
        }
    }

    // Exit code: 0 = pass, 1 = fail
    Ok(if result.passed { 0 } else { 1 })
}

fn resolve_and_ingest_issues(patterns: &[String]) -> Result<DiagnosticsData> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        files.extend(matches);
    }

    if files.is_empty() {
        anyhow::bail!("No diagnostics files found");
    }

    let mut combined = DiagnosticsData::new();
    for file in &files {
        let data = covy_ingest::ingest_diagnostics_path(file)?;
        combined.merge(&data);
    }

    Ok(combined)
}

fn resolve_diagnostics(
    issues_patterns: &[String],
    issues_state_path: Option<&str>,
    no_issues_state: bool,
) -> Result<Option<DiagnosticsData>> {
    if !issues_patterns.is_empty() {
        return Ok(Some(resolve_and_ingest_issues(issues_patterns)?));
    }

    if no_issues_state {
        return Ok(None);
    }

    let state_path = issues_state_path.unwrap_or(".covy/state/issues.bin");
    let state_path = Path::new(state_path);
    if !state_path.exists() {
        return Ok(None);
    }

    tracing::info!(
        "Loading diagnostics from cached state {}",
        state_path.display()
    );
    let bytes = std::fs::read(state_path)?;
    let diagnostics = covy_core::cache::deserialize_diagnostics(&bytes)?;
    Ok(Some(diagnostics))
}
