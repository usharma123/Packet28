use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::diagnostics::DiagnosticsData;
use covy_core::model::{CoverageData, CoverageFormat};
use covy_core::CovyConfig;

#[derive(Args)]
pub struct IngestArgs {
    /// Coverage report file paths (supports globs)
    #[arg()]
    paths: Vec<String>,

    /// Coverage format (auto/lcov/cobertura/jacoco/gocov/llvm-cov)
    #[arg(short, long, default_value = "auto")]
    format: String,

    /// SARIF diagnostics file paths (supports globs)
    #[arg(long)]
    issues: Vec<String>,

    /// Read coverage data from stdin
    #[arg(long)]
    stdin: bool,

    /// Source root for resolving relative paths
    #[arg(long)]
    source_root: Option<String>,

    /// Prefixes to strip from file paths in coverage data
    #[arg(long)]
    strip_prefix: Vec<String>,

    /// Merge with existing coverage/diagnostics data
    #[arg(short, long)]
    merge: bool,

    /// Output file path (default: .covy/state/latest.bin)
    #[arg(short, long, alias = "out")]
    output: Option<String>,

    /// Emit JSON summary output
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct IngestSummary {
    coverage_inputs: usize,
    coverage_files_tracked: usize,
    diagnostics_inputs: usize,
    diagnostics_total_issues: usize,
    output_coverage_path: String,
    output_issues_path: Option<String>,
}

pub fn run(args: IngestArgs, config_path: &str) -> Result<i32> {
    crate::cmd_common::warn_if_legacy_flag_used("--out", "--output");
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();

    // Resolve coverage globs
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

    // If no CLI paths given, fall back to config report_paths
    if files.is_empty() && !args.stdin && args.paths.is_empty() {
        if !config.ingest.report_paths.is_empty() {
            tracing::debug!(
                "No CLI paths given, trying config report_paths: {:?}",
                config.ingest.report_paths
            );
            let config_files = crate::cmd_common::resolve_report_globs_for_config(
                config_path,
                &config.ingest.report_paths,
            )?;
            // Canonicalize to deduplicate paths resolved from different bases
            let mut seen = std::collections::HashSet::new();
            for f in config_files {
                let canonical = f.canonicalize().unwrap_or_else(|_| f.clone());
                if seen.insert(canonical) {
                    files.push(f);
                }
            }
        }
    }

    if files.is_empty() && !args.stdin && args.issues.is_empty() {
        if !config.ingest.report_paths.is_empty() {
            anyhow::bail!(
                "No input files found.\n  \
                 hint: [ingest].report_paths = {:?} matched 0 files.\n        \
                 Provide paths as arguments, use --stdin, or check your covy.toml config.",
                config.ingest.report_paths
            );
        }
        anyhow::bail!("No input files found. Provide coverage paths, --stdin, or --issues.");
    }

    // Parse coverage format
    let format = match args.format.as_str() {
        "lcov" => Some(CoverageFormat::Lcov),
        "cobertura" => Some(CoverageFormat::Cobertura),
        "jacoco" => Some(CoverageFormat::JaCoCo),
        "gocov" => Some(CoverageFormat::GoCov),
        "llvm-cov" => Some(CoverageFormat::LlvmCov),
        "auto" => None,
        other => anyhow::bail!("Unknown format: {other}"),
    };

    // Load existing coverage data if merging
    let mut combined = if args.merge {
        load_existing_coverage(&args.output, &config)?
    } else {
        CoverageData::new()
    };

    // Handle coverage stdin
    if args.stdin {
        let fmt = format.ok_or_else(|| {
            anyhow::anyhow!("--format is required when reading from --stdin (can't auto-detect)")
        })?;
        let data = covy_ingest::ingest_reader(std::io::stdin().lock(), fmt)?;
        combined.merge(&data);
    }

    // Process each coverage file
    for file in &files {
        tracing::info!("Ingesting {}", file.display());
        let data = if let Some(fmt) = format {
            covy_ingest::ingest_path_with_format(file, fmt)?
        } else {
            covy_ingest::ingest_path(file)?
        };

        // Apply strip prefixes
        let strip_prefixes: Vec<&str> = args
            .strip_prefix
            .iter()
            .chain(config.ingest.strip_prefixes.iter())
            .map(|s| s.as_str())
            .collect();

        let data = if strip_prefixes.is_empty() {
            data
        } else {
            apply_strip_prefixes(data, &strip_prefixes)
        };

        combined.merge(&data);
    }

    // Normalize coverage paths (auto-detect or use --source-root)
    let source_root = args.source_root.as_deref().map(Path::new);
    covy_core::pathmap::auto_normalize_paths(&mut combined, source_root);

    // Save coverage state
    let output_path = args
        .output
        .clone()
        .unwrap_or_else(|| ".covy/state/latest.bin".to_string());
    let output_path = Path::new(&output_path);

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let bytes = covy_core::cache::serialize_coverage(&combined)?;
    std::fs::write(output_path, bytes)?;

    let mut diagnostics_input_count = 0usize;
    let mut diagnostics_total_issues = 0usize;

    // Process diagnostics, if requested
    if !args.issues.is_empty() {
        let mut diagnostics = if args.merge {
            load_existing_issues()?
        } else {
            DiagnosticsData::new()
        };

        let issue_files = resolve_globs(&args.issues)?;
        diagnostics_input_count = issue_files.len();
        if issue_files.is_empty() {
            anyhow::bail!("No diagnostics files found");
        }

        for file in &issue_files {
            tracing::info!("Ingesting diagnostics {}", file.display());
            let data = load_diagnostics_input(file)?;
            diagnostics.merge(&data);
        }

        covy_core::pathmap::auto_normalize_issue_paths(&mut diagnostics, source_root);
        diagnostics_total_issues = diagnostics.total_issues();

        let issue_path = Path::new(".covy/state/issues.bin");
        if let Some(parent) = issue_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = covy_core::cache::serialize_diagnostics_with_metadata(
            &diagnostics,
            &covy_core::cache::DiagnosticsStateMetadata::normalized_for_repo_root(
                covy_core::cache::current_repo_root_id(source_root),
            ),
        )?;
        std::fs::write(issue_path, bytes)?;

        tracing::info!(
            "Ingested {} diagnostics files, {} total issues tracked",
            issue_files.len(),
            diagnostics.total_issues()
        );
    }

    tracing::info!(
        "Ingested {} coverage files, {} total files tracked",
        files.len(),
        combined.files.len()
    );

    if let Some(pct) = combined.total_coverage_pct() {
        tracing::info!("Total coverage: {pct:.1}%");
    }

    if args.json || crate::cmd_common::global_quiet_enabled() {
        let summary = IngestSummary {
            coverage_inputs: files.len(),
            coverage_files_tracked: combined.files.len(),
            diagnostics_inputs: diagnostics_input_count,
            diagnostics_total_issues,
            output_coverage_path: output_path.display().to_string(),
            output_issues_path: if diagnostics_input_count > 0 {
                Some(".covy/state/issues.bin".to_string())
            } else {
                None
            },
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    Ok(0)
}

fn load_existing_coverage(output: &Option<String>, config: &CovyConfig) -> Result<CoverageData> {
    let path = output.as_deref().unwrap_or(".covy/state/latest.bin");
    let path = Path::new(path);
    if path.exists() {
        let bytes = std::fs::read(path)?;
        let data = covy_core::cache::deserialize_coverage(&bytes)?;
        Ok(data)
    } else {
        let _ = config; // suppress unused warning
        Ok(CoverageData::new())
    }
}

fn load_existing_issues() -> Result<DiagnosticsData> {
    let path = Path::new(".covy/state/issues.bin");
    if path.exists() {
        let bytes = std::fs::read(path)?;
        let data = covy_core::cache::deserialize_diagnostics(&bytes)?;
        Ok(data)
    } else {
        Ok(DiagnosticsData::new())
    }
}

fn resolve_globs(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .context(format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        if matches.is_empty() {
            tracing::warn!("No files matched pattern: {}", pattern);
        }
        files.extend(matches);
    }
    Ok(files)
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
