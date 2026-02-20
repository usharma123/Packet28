use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use covy_core::CovyConfig;

#[derive(Args)]
pub struct MergeArgs {
    /// Coverage shard artifacts (supports globs)
    #[arg(long)]
    pub coverage: Vec<String>,

    /// Diagnostics shard artifacts (supports globs)
    #[arg(long)]
    pub issues: Vec<String>,

    /// Strict mode for missing/corrupt artifacts
    #[arg(long)]
    pub strict: Option<bool>,

    /// Output coverage state path
    #[arg(long, default_value = ".covy/state/latest.bin")]
    pub output_coverage: String,

    /// Output diagnostics state path
    #[arg(long, default_value = ".covy/state/issues.bin")]
    pub output_issues: String,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: MergeArgs, config_path: &str) -> Result<i32> {
    let config = CovyConfig::load(Path::new(config_path)).unwrap_or_default();
    let strict = args.strict.unwrap_or(config.merge.strict);

    let coverage_inputs = resolve_globs(&args.coverage)?;
    let diagnostics_inputs = resolve_globs(&args.issues)?;
    if coverage_inputs.is_empty() && diagnostics_inputs.is_empty() {
        anyhow::bail!("No merge inputs found. Provide --coverage and/or --issues globs.");
    }

    let (coverage_merged, skipped_cov) =
        covy_core::merge::merge_coverage_inputs(&coverage_inputs, strict)?;
    let (diag_merged, skipped_diag) =
        covy_core::merge::merge_diagnostics_inputs(&diagnostics_inputs, strict)?;

    let output_coverage = if args.output_coverage == ".covy/state/latest.bin" {
        config.merge.output_coverage.as_str()
    } else {
        args.output_coverage.as_str()
    };
    let output_issues = if args.output_issues == ".covy/state/issues.bin" {
        config.merge.output_issues.as_str()
    } else {
        args.output_issues.as_str()
    };

    if !coverage_inputs.is_empty() {
        write_file(
            Path::new(output_coverage),
            &covy_core::cache::serialize_coverage(&coverage_merged)?,
        )?;
    }
    if !diagnostics_inputs.is_empty() {
        write_file(
            Path::new(output_issues),
            &covy_core::cache::serialize_diagnostics_with_metadata(
                &diag_merged,
                &covy_core::cache::DiagnosticsStateMetadata::normalized_for_repo_root(
                    covy_core::cache::current_repo_root_id(None),
                ),
            )?,
        )?;
    }

    let summary = covy_core::merge::MergeSummary {
        coverage_inputs: coverage_inputs.len(),
        diagnostics_inputs: diagnostics_inputs.len(),
        skipped_inputs: skipped_cov + skipped_diag,
        coverage_files_merged: coverage_merged.files.len(),
        diagnostics_files_merged: diag_merged.issues_by_file.len(),
        strict_mode: strict,
        output_coverage_path: if coverage_inputs.is_empty() {
            None
        } else {
            Some(output_coverage.to_string())
        },
        output_issues_path: if diagnostics_inputs.is_empty() {
            None
        } else {
            Some(output_issues.to_string())
        },
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "merge summary: coverage_inputs={} diagnostics_inputs={} skipped_inputs={} coverage_files_merged={} diagnostics_files_merged={} strict_mode={}",
            summary.coverage_inputs,
            summary.diagnostics_inputs,
            summary.skipped_inputs,
            summary.coverage_files_merged,
            summary.diagnostics_files_merged,
            summary.strict_mode
        );
    }

    Ok(0)
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

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_globs_empty_ok() {
        let files = resolve_globs(&["/definitely/not/found/*.bin".to_string()]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_write_file_creates_parent_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("state.bin");
        write_file(&path, b"abc").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
    }
}
