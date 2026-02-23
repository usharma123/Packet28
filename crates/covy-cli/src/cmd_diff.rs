use std::collections::HashSet;
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

    /// Output format (terminal/json). Defaults to "terminal" in interactive
    /// mode and "json" when stdout is piped.
    #[arg(long)]
    report: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Coverage report files to ingest (instead of loading state)
    #[arg(long)]
    coverage: Vec<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,
}

pub fn run(args: DiffArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let report =
        if crate::cmd_common::resolve_json_output(args.json, args.report.as_deref(), "--report")? {
            "json".to_string()
        } else {
            crate::cmd_common::resolve_report_format(args.report.as_deref())
        };

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

    // Get diff first so cached diagnostics can be selective.
    tracing::info!("Computing diff {base}..{head}");
    let diffs = covy_core::diff::git_diff(base, head)?;
    tracing::info!("Found {} changed files", diffs.len());
    let changed_paths: HashSet<String> = diffs.iter().map(|d| d.path.clone()).collect();

    // Optional diagnostics
    let mut loaded = resolve_diagnostics(
        &args.issues,
        args.issues_state.as_deref(),
        args.no_issues_state,
        &changed_paths,
    )?;
    if let Some(diag) = loaded.data.as_mut() {
        if loaded.needs_normalization {
            covy_core::pathmap::auto_normalize_issue_paths(diag, None);
        }
    }

    // Build gate config from CLI args + config file
    let gate_config = GateConfig {
        fail_under_total: args.fail_under_total.or(config.gate.fail_under_total),
        fail_under_changed: args.fail_under_changed.or(config.gate.fail_under_changed),
        fail_under_new: args.fail_under_new.or(config.gate.fail_under_new),
        issues: config.gate.issues.clone(),
    };

    // Evaluate gate
    let result =
        covy_core::gate::evaluate_full_gate(&gate_config, &coverage, loaded.data.as_ref(), &diffs);

    // Output
    match report.as_str() {
        "json" => {
            let json = covy_core::report::render_gate_json(&result);
            println!("{json}");
        }
        _ => {
            covy_core::report::render_gate_result(&result);
            if let Some(diag) = loaded.data.as_ref() {
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
        let data = load_diagnostics_input(file)?;
        combined.merge(&data);
    }

    Ok(combined)
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

fn resolve_diagnostics(
    issues_patterns: &[String],
    issues_state_path: Option<&str>,
    no_issues_state: bool,
    selected_paths: &HashSet<String>,
) -> Result<LoadedDiagnostics> {
    if !issues_patterns.is_empty() {
        return Ok(LoadedDiagnostics {
            data: Some(resolve_and_ingest_issues(issues_patterns)?),
            needs_normalization: true,
        });
    }

    if no_issues_state {
        return Ok(LoadedDiagnostics::none());
    }

    let state_path = issues_state_path.unwrap_or(".covy/state/issues.bin");
    let state_path = Path::new(state_path);
    if !state_path.exists() {
        return Ok(LoadedDiagnostics::none());
    }

    tracing::info!(
        "Loading diagnostics from cached state {}",
        state_path.display()
    );
    let (diagnostics, meta) =
        covy_core::cache::deserialize_diagnostics_for_paths_from_file(state_path, selected_paths)?;

    let needs_normalization = !state_metadata_compatible(meta.as_ref());
    Ok(LoadedDiagnostics {
        data: Some(diagnostics),
        needs_normalization,
    })
}

#[derive(Default)]
struct LoadedDiagnostics {
    data: Option<DiagnosticsData>,
    needs_normalization: bool,
}

impl LoadedDiagnostics {
    fn none() -> Self {
        Self {
            data: None,
            needs_normalization: false,
        }
    }
}

fn state_metadata_compatible(meta: Option<&covy_core::cache::DiagnosticsStateMetadata>) -> bool {
    let Some(meta) = meta else {
        return false;
    };

    if meta.schema_version != covy_core::cache::DIAGNOSTICS_STATE_SCHEMA_VERSION {
        return false;
    }
    if meta.path_norm_version != covy_core::cache::DIAGNOSTICS_PATH_NORM_VERSION {
        return false;
    }
    if !meta.normalized_paths {
        return false;
    }

    meta.repo_root_id == covy_core::cache::current_repo_root_id(None)
}
