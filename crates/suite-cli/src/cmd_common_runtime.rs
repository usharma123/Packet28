use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use suite_packet_core::{CoverageData, CoverageFormat};

pub fn parse_daemon_env_flag(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

pub fn via_daemon_env_enabled() -> bool {
    parse_daemon_env_flag(std::env::var("PACKET28_VIA_DAEMON").ok().as_deref())
}

pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None => "terminal".to_string(),
    }
}

pub fn caller_cwd() -> Result<PathBuf> {
    std::env::current_dir().context("failed to resolve current directory")
}

pub fn resolve_path_from_cwd(value: &str, cwd: &Path) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    let path = PathBuf::from(value);
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    absolute
        .canonicalize()
        .unwrap_or(absolute)
        .to_string_lossy()
        .into_owned()
}

pub fn resolve_optional_path_from_cwd(value: Option<&str>, cwd: &Path) -> Option<String> {
    value.map(|value| resolve_path_from_cwd(value, cwd))
}

pub fn resolve_paths_from_cwd(values: &[String], cwd: &Path) -> Vec<String> {
    values
        .iter()
        .map(|value| resolve_path_from_cwd(value, cwd))
        .collect()
}

pub fn default_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

pub fn repo_cache_fingerprint(repo_root: &Path, relevant_paths: &[PathBuf]) -> String {
    suite_foundation_core::repo_fingerprint::cache_fingerprint(repo_root, relevant_paths)
}

fn ingest_coverage_auto(path: &Path) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, None).map_err(Into::into)
}

fn ingest_coverage_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_path(path, Some(format)).map_err(Into::into)
}

fn ingest_coverage_stdin(format: CoverageFormat) -> Result<CoverageData> {
    suite_ingest::ingest_coverage_stdin(format).map_err(Into::into)
}

fn ingest_diagnostics(path: &Path) -> Result<suite_packet_core::diagnostics::DiagnosticsData> {
    suite_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}
