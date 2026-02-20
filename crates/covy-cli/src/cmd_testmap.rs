use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Deserialize;

#[derive(Args)]
pub struct TestmapArgs {
    #[command(subcommand)]
    pub command: TestmapCommands,
}

#[derive(Subcommand)]
pub enum TestmapCommands {
    /// Build test impact map artifacts
    Build(TestmapBuildArgs),
}

#[derive(Args)]
pub struct TestmapBuildArgs {
    /// Input manifest glob(s)
    #[arg(long)]
    pub manifest: Vec<String>,

    /// Output test map path
    #[arg(long, default_value = ".covy/state/testmap.bin")]
    pub output: String,

    /// Output timing map path
    #[arg(long, default_value = ".covy/state/testtimings.bin")]
    pub timings_output: String,
}

pub fn run(args: TestmapArgs, _config_path: &str) -> Result<i32> {
    match args.command {
        TestmapCommands::Build(build) => {
            let files = resolve_globs(&build.manifest)?;
            if files.is_empty() {
                anyhow::bail!("No manifest files found");
            }
            let records = read_manifest_records(&files)?;
            validate_manifest_records(&records)?;
            tracing::info!(
                "Validated {} manifest records from {} file(s)",
                records.len(),
                files.len()
            );
            Ok(0)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestRecord {
    test_id: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    coverage_report: Option<String>,
    #[serde(default)]
    coverage_reports: Vec<String>,
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

fn read_manifest_records(files: &[PathBuf]) -> Result<Vec<ManifestRecord>> {
    let mut out = Vec::new();
    for file in files {
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read manifest file {}", file.display()))?;
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec: ManifestRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Invalid JSON on {} line {}",
                    file.display(),
                    idx + 1
                )
            })?;
            out.push(rec);
        }
    }
    Ok(out)
}

fn validate_manifest_records(records: &[ManifestRecord]) -> Result<()> {
    if records.is_empty() {
        anyhow::bail!("Manifest contains no records");
    }
    for (idx, rec) in records.iter().enumerate() {
        if rec.test_id.trim().is_empty() {
            anyhow::bail!("Record {} has empty test_id", idx + 1);
        }
        if rec.language.as_deref().is_some_and(|s| s.trim().is_empty()) {
            anyhow::bail!("Record {} has empty language", idx + 1);
        }
        if rec.coverage_report.as_deref().is_none() && rec.coverage_reports.is_empty() {
            anyhow::bail!(
                "Record {} for test '{}' must provide coverage_report or coverage_reports",
                idx + 1,
                rec.test_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_manifest_records_success() {
        let records = vec![ManifestRecord {
            test_id: "com.foo.BarTest".to_string(),
            language: Some("java".to_string()),
            duration_ms: Some(123),
            coverage_report: Some("reports/bar.xml".to_string()),
            coverage_reports: Vec::new(),
        }];
        assert!(validate_manifest_records(&records).is_ok());
    }

    #[test]
    fn test_validate_manifest_records_missing_coverage() {
        let records = vec![ManifestRecord {
            test_id: "com.foo.BarTest".to_string(),
            language: None,
            duration_ms: None,
            coverage_report: None,
            coverage_reports: Vec::new(),
        }];
        let err = validate_manifest_records(&records).unwrap_err();
        assert!(err
            .to_string()
            .contains("must provide coverage_report or coverage_reports"));
    }
}
