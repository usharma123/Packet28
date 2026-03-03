use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use covy_core::path_diagnose::{diagnose_paths, load_repo_paths, PathDiagnosisRequest};
use covy_core::CovyConfig;

#[derive(Args)]
pub struct DoctorArgs {
    /// Base ref for validation (default from config)
    #[arg(long)]
    pub base_ref: Option<String>,

    /// Head ref for validation (default from config)
    #[arg(long)]
    pub head_ref: Option<String>,

    /// Emit JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, serde::Serialize)]
struct DoctorSummary {
    config_path: String,
    config_base_dir: String,
    repo_root: String,
    report_files: usize,
    parsed_report_paths: usize,
    mapped: usize,
    total: usize,
    mapped_pct: f64,
    unmapped_prefixes: Vec<(String, usize)>,
    suggested_strip_prefixes: Vec<String>,
    next_step: String,
}

pub fn run(args: DoctorArgs, config_path: &str) -> Result<i32> {
    let config = load_config_checked(config_path)?;
    let base = args.base_ref.as_deref().unwrap_or(&config.diff.base);
    let head = args.head_ref.as_deref().unwrap_or(&config.diff.head);

    ensure_git_available()?;
    validate_git_refs(base, head)?;

    let repo_root = crate::cmd_common::detect_repo_root()?;
    let config_path_abs = std::fs::canonicalize(Path::new(config_path))
        .unwrap_or_else(|_| Path::new(config_path).to_path_buf());
    let config_base_dir = config_path_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let report_files = crate::cmd_common::resolve_report_globs_for_config(
        config_path,
        &config.ingest.report_paths,
    )?;
    if report_files.is_empty() {
        if args.json {
            let summary = DoctorSummary {
                config_path: config_path_abs.display().to_string(),
                config_base_dir: config_base_dir.display().to_string(),
                repo_root: repo_root.display().to_string(),
                report_files: 0,
                parsed_report_paths: 0,
                mapped: 0,
                total: 0,
                mapped_pct: 0.0,
                unmapped_prefixes: Vec::new(),
                suggested_strip_prefixes: Vec::new(),
                next_step: "configure [ingest].report_paths and run covy map-paths --learn --write"
                    .to_string(),
            };
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            println!("Repo root: {}", repo_root.display());
            println!("No report files matched [ingest].report_paths");
            println!(
                "Next: configure [ingest].report_paths and run covy map-paths --learn --write"
            );
        }
        return Ok(0);
    }

    let report_paths = parse_report_paths_quick(&report_files)?;
    let parsed_report_paths = report_paths.len();
    let repo_paths = load_repo_paths(&repo_root)?;
    let stats = diagnose_paths(PathDiagnosisRequest::from_config(
        report_paths,
        repo_paths,
        &config,
    ))?;
    let pct = if stats.total == 0 {
        0.0
    } else {
        (stats.mapped as f64 / stats.total as f64) * 100.0
    };

    if args.json {
        let summary = DoctorSummary {
            config_path: config_path_abs.display().to_string(),
            config_base_dir: config_base_dir.display().to_string(),
            repo_root: repo_root.display().to_string(),
            report_files: report_files.len(),
            parsed_report_paths,
            mapped: stats.mapped,
            total: stats.total,
            mapped_pct: pct,
            unmapped_prefixes: stats.unmapped_prefixes.clone(),
            suggested_strip_prefixes: stats.suggested_strip_prefixes.clone(),
            next_step: "run covy map-paths --learn --write".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(0);
    }

    println!("Repo root: {}", repo_root.display());
    println!("Parsed reports: {} files", parsed_report_paths);
    if stats.total == 0 {
        println!("Mapped paths: 0/0 (0.0%)");
        println!("No file paths were extracted from reports.");
        return Ok(0);
    }

    println!("Mapped paths: {}/{} ({pct:.1}%)", stats.mapped, stats.total);

    if !stats.unmapped_prefixes.is_empty() {
        println!("Unmapped prefixes (top):");
        for (prefix, count) in stats.unmapped_prefixes.iter().take(5) {
            println!("  - {prefix} ({count})");
        }
    }

    if !stats.suggested_strip_prefixes.is_empty() {
        let joined = stats
            .suggested_strip_prefixes
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Suggested rule: strip_prefix += [{joined}]");
    }

    println!("Next: run covy map-paths --learn --write");
    Ok(0)
}

fn load_config_checked(config_path: &str) -> Result<CovyConfig> {
    CovyConfig::load(Path::new(config_path))
        .with_context(|| format!("Invalid config at {config_path}"))
        .map_err(Into::into)
}

fn ensure_git_available() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .context("git is not available in PATH")?;
    if !output.status.success() {
        anyhow::bail!("git command is unavailable");
    }
    Ok(())
}

fn validate_git_refs(base: &str, head: &str) -> Result<()> {
    for r in [base, head] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", r])
            .output()
            .with_context(|| format!("Failed to resolve git ref '{r}'"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to resolve git ref '{r}': {stderr}");
        }
    }
    Ok(())
}

fn parse_report_paths_quick(report_files: &[PathBuf]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for report in report_files {
        let coverage = covy_ingest::ingest_path(report)
            .with_context(|| format!("Failed to parse report {}", report.display()))?;
        paths.extend(coverage.files.keys().cloned());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_config_checked_reports_precise_path() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("broken.toml");
        std::fs::write(&config_path, "[impact\nmax_tests = 10").unwrap();

        let err = load_config_checked(config_path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Invalid config at"));
        assert!(err.to_string().contains("broken.toml"));
    }
}
