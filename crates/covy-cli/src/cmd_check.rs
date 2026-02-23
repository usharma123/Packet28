use std::collections::HashSet;
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

    /// Output format (terminal/json/markdown/github). Defaults to "terminal"
    /// in interactive mode and "json" when stdout is piped.
    #[arg(long)]
    report: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    json: bool,

    /// Prefixes to strip from file paths in coverage data
    #[arg(long)]
    strip_prefix: Vec<String>,

    /// Source root for resolving relative paths
    #[arg(long)]
    source_root: Option<String>,

    /// Path to coverage state file
    #[arg(long)]
    input: Option<String>,

    /// Show missing line numbers
    #[arg(long)]
    show_missing: bool,
}

pub fn run(args: CheckArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let report =
        if crate::cmd_common::resolve_json_output(args.json, args.report.as_deref(), "--report")? {
            "json".to_string()
        } else {
            crate::cmd_common::resolve_report_format(args.report.as_deref())
        };

    // Ingest coverage data
    let mut coverage = resolve_and_ingest(&args, &config)?;

    // Normalize coverage paths
    let source_root = args.source_root.as_deref().map(Path::new);
    covy_core::pathmap::auto_normalize_paths(&mut coverage, source_root);

    // Diff
    let base = args.base.as_deref().unwrap_or(&config.diff.base);
    let head = args.head.as_deref().unwrap_or(&config.diff.head);

    tracing::info!("Computing diff {base}..{head}");
    let diffs = covy_core::diff::git_diff(base, head)?;
    tracing::info!("Found {} changed files", diffs.len());

    let changed_paths: HashSet<String> = diffs.iter().map(|d| d.path.clone()).collect();

    // Ingest diagnostics (optional)
    let mut loaded = resolve_diagnostics(
        &args.issues,
        args.issues_state.as_deref(),
        args.no_issues_state,
        &changed_paths,
        source_root,
    )?;

    if let Some(diag) = loaded.data.as_mut() {
        if loaded.needs_normalization {
            covy_core::pathmap::auto_normalize_issue_paths(diag, source_root);
        }
    }

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
        covy_core::gate::evaluate_full_gate(&gate_config, &coverage, loaded.data.as_ref(), &diffs);

    // Render
    match report.as_str() {
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
                loaded.data.as_ref(),
            );
            print!("{md}");
        }
        "github" => {
            covy_core::report::render_github_annotations(
                &coverage,
                &diffs,
                &gate_result,
                loaded.data.as_ref(),
            );
        }
        _ => {
            covy_core::report::render_terminal(&coverage, args.show_missing, "name", None, false);
            if let Some(diag) = loaded.data.as_ref() {
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
        if !args.paths.is_empty() {
            anyhow::bail!("Cannot combine positional coverage paths with --stdin");
        }
        if args.input.is_some() {
            anyhow::bail!("Cannot combine --input with --stdin");
        }
        let fmt = format.ok_or_else(|| {
            anyhow::anyhow!("--format is required when reading from --stdin (can't auto-detect)")
        })?;
        let data = covy_ingest::ingest_reader(std::io::stdin().lock(), fmt)?;
        return Ok(data);
    }

    if let Some(path) = args.input.as_deref() {
        if !args.paths.is_empty() {
            anyhow::bail!("Cannot combine positional coverage paths with --input");
        }
        let state_path = Path::new(path);
        if !state_path.exists() {
            anyhow::bail!(
                "No coverage data found at {}. Run `covy ingest` first or provide valid coverage paths.",
                state_path.display()
            );
        }
        tracing::info!(
            "Loading coverage from configured state {}",
            state_path.display()
        );
        let bytes = std::fs::read(state_path)?;
        let data = covy_core::cache::deserialize_coverage(&bytes)?;
        return Ok(data);
    }

    if args.paths.is_empty() {
        let state_path = Path::new(".covy/state/latest.bin");
        if !state_path.exists() {
            anyhow::bail!(
                "No coverage files specified and no cached coverage state found at {}. Provide file paths, use --stdin, or run `covy ingest` first.",
                state_path.display()
            );
        }
        tracing::info!(
            "Loading coverage from cached state {}",
            state_path.display()
        );
        let bytes = std::fs::read(state_path)?;
        let data = covy_core::cache::deserialize_coverage(&bytes)?;
        return Ok(data);
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
    selected_paths: &HashSet<String>,
    source_root: Option<&Path>,
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
    let needs_normalization = !state_metadata_compatible(meta.as_ref(), source_root);
    Ok(LoadedDiagnostics {
        data: Some(diagnostics),
        needs_normalization,
    })
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

fn state_metadata_compatible(
    meta: Option<&covy_core::cache::DiagnosticsStateMetadata>,
    source_root: Option<&Path>,
) -> bool {
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

    let current_root_id = covy_core::cache::current_repo_root_id(source_root);
    meta.repo_root_id == current_root_id
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
