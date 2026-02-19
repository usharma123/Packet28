use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::config::{GateConfig, IssueGateConfig};
use covy_core::diagnostics::DiagnosticsData;
use covy_core::model::{CoverageData, CoverageFormat};
use covy_core::CovyConfig;

#[derive(Args)]
pub struct CheckArgs {
    /// Coverage report file paths (supports globs)
    #[arg()]
    paths: Vec<String>,

    /// Coverage format (auto/lcov/cobertura/jacoco/gocov/llvm-cov)
    #[arg(short, long, default_value = "auto")]
    format: String,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Path to cached diagnostics state file (default: .covy/state/issues.bin)
    #[arg(long)]
    issues_state: Option<String>,

    /// Disable automatic diagnostics state loading when --issues is not provided
    #[arg(long)]
    no_issues_state: bool,

    /// Read coverage data from stdin
    #[arg(long)]
    stdin: bool,

    /// Base ref for diff (default: main)
    #[arg(long)]
    base: Option<String>,

    /// Head ref for diff (default: HEAD)
    #[arg(long)]
    head: Option<String>,

    /// Fail if total coverage is below this %
    #[arg(long)]
    fail_under_total: Option<f64>,

    /// Fail if changed lines coverage is below this %
    #[arg(long)]
    fail_under_changed: Option<f64>,

    /// Fail if new file coverage is below this %
    #[arg(long)]
    fail_under_new: Option<f64>,

    /// Fail if changed-line errors exceed this value
    #[arg(long)]
    max_new_errors: Option<u32>,

    /// Fail if changed-line warnings exceed this value
    #[arg(long)]
    max_new_warnings: Option<u32>,

    /// Output format (terminal/json/markdown/github)
    #[arg(long, default_value = "terminal")]
    report: String,

    /// Prefixes to strip from file paths in coverage data
    #[arg(long)]
    strip_prefix: Vec<String>,

    /// Source root for resolving relative paths
    #[arg(long)]
    source_root: Option<String>,

    /// Show missing line numbers
    #[arg(long)]
    show_missing: bool,
}

pub fn run(args: CheckArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

    // Ingest coverage data
    let mut coverage = resolve_and_ingest(&args, &config)?;

    // Normalize coverage paths
    let source_root = args.source_root.as_deref().map(Path::new);
    covy_core::pathmap::auto_normalize_paths(&mut coverage, source_root);

    // Ingest diagnostics (optional)
    let mut diagnostics = resolve_diagnostics(
        &args.issues,
        args.issues_state.as_deref(),
        args.no_issues_state,
    )?;

    if let Some(diag) = diagnostics.as_mut() {
        covy_core::pathmap::auto_normalize_issue_paths(diag, source_root);
    }

    // Diff
    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    tracing::info!("Computing diff {base}..{head}");
    let diffs = covy_core::diff::git_diff(base, head)?;
    tracing::info!("Found {} changed files", diffs.len());

    // Gate
    let issue_gate = IssueGateConfig {
        max_new_errors: args.max_new_errors.or(config.gate.issues.max_new_errors),
        max_new_warnings: args
            .max_new_warnings
            .or(config.gate.issues.max_new_warnings),
        max_new_issues: config.gate.issues.max_new_issues,
    };

    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: issue_gate,
    };

    let gate_result =
        covy_core::gate::evaluate_full_gate(&gate_config, &coverage, diagnostics.as_ref(), &diffs);

    // Render
    match args.report.as_str() {
        "json" => {
            let json = covy_core::report::render_gate_json(&gate_result);
            println!("{json}");
        }
        "markdown" => {
            let md = covy_core::report::render_markdown(
                &coverage,
                &gate_result,
                &diffs,
                args.show_missing,
                diagnostics.as_ref(),
            );
            print!("{md}");
        }
        "github" => {
            covy_core::report::render_github_annotations(
                &coverage,
                &diffs,
                &gate_result,
                diagnostics.as_ref(),
            );
        }
        _ => {
            covy_core::report::render_terminal(&coverage, args.show_missing, "name");
            if let Some(diag) = diagnostics.as_ref() {
                covy_core::report::render_issues_terminal(diag, Some(&diffs));
            }
            covy_core::report::render_gate_result(&gate_result);
        }
    }

    Ok(if gate_result.passed { 0 } else { 1 })
}

/// Shared helper: resolve file paths/stdin and ingest coverage data.
pub fn resolve_and_ingest(args: &CheckArgs, config: &CovyConfig) -> Result<CoverageData> {
    let format = parse_format(&args.format)?;

    if args.stdin {
        let fmt = format.ok_or_else(|| {
            anyhow::anyhow!("--format is required when reading from --stdin (can't auto-detect)")
        })?;
        let data = covy_ingest::ingest_reader(std::io::stdin().lock(), fmt)?;
        return Ok(data);
    }

    if args.paths.is_empty() {
        anyhow::bail!("No coverage files specified. Provide file paths or use --stdin.");
    }

    // Resolve globs
    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in &args.paths {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        if matches.is_empty() {
            tracing::warn!("No files matched pattern: {}", pattern);
        }
        files.extend(matches);
    }

    if files.is_empty() {
        anyhow::bail!("No coverage files found");
    }

    let strip_prefixes: Vec<&str> = args
        .strip_prefix
        .iter()
        .chain(config.ingest.strip_prefixes.iter())
        .map(|s| s.as_str())
        .collect();

    let mut combined = CoverageData::new();
    for file in &files {
        tracing::info!("Ingesting {}", file.display());
        let data = if let Some(fmt) = format {
            covy_ingest::ingest_path_with_format(file, fmt)?
        } else {
            covy_ingest::ingest_path(file)?
        };

        let data = if strip_prefixes.is_empty() {
            data
        } else {
            apply_strip_prefixes(data, &strip_prefixes)
        };

        combined.merge(&data);
    }

    Ok(combined)
}

fn resolve_and_ingest_issues(patterns: &[String]) -> Result<DiagnosticsData> {
    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        if matches.is_empty() {
            tracing::warn!("No diagnostics files matched pattern: {}", pattern);
        }
        files.extend(matches);
    }

    if files.is_empty() {
        anyhow::bail!("No diagnostics files found");
    }

    let mut combined = DiagnosticsData::new();
    for file in &files {
        tracing::info!("Ingesting diagnostics {}", file.display());
        let data = load_diagnostics_input(file)?;
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

fn load_diagnostics_input(path: &Path) -> Result<DiagnosticsData> {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
    {
        let bytes = std::fs::read(path)?;
        let diagnostics = covy_core::cache::deserialize_diagnostics(&bytes)?;
        return Ok(diagnostics);
    }

    covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

fn parse_format(s: &str) -> Result<Option<CoverageFormat>> {
    match s {
        "lcov" => Ok(Some(CoverageFormat::Lcov)),
        "cobertura" => Ok(Some(CoverageFormat::Cobertura)),
        "jacoco" => Ok(Some(CoverageFormat::JaCoCo)),
        "gocov" => Ok(Some(CoverageFormat::GoCov)),
        "llvm-cov" => Ok(Some(CoverageFormat::LlvmCov)),
        "auto" => Ok(None),
        other => anyhow::bail!("Unknown format: {other}"),
    }
}

fn apply_strip_prefixes(data: CoverageData, prefixes: &[&str]) -> CoverageData {
    let mut result = CoverageData {
        files: std::collections::BTreeMap::new(),
        format: data.format,
        timestamp: data.timestamp,
    };
    for (path, fc) in data.files {
        let mut stripped = path.as_str();
        for prefix in prefixes {
            if let Some(rest) = stripped.strip_prefix(prefix) {
                stripped = rest;
                break;
            }
        }
        result.files.insert(stripped.to_string(), fc);
    }
    result
}
