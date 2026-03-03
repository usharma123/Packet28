use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::{collections::BTreeSet, ffi::OsString};

use anyhow::{Context, Result};
use covy_core::config::GateConfig;
use covy_core::diagnostics::DiagnosticsData;
use covy_core::{CoverageData, CoverageFormat, CovyConfig, FileDiff};
use roaring::RoaringBitmap;

/// Resolve the report output format: use the explicit value if provided,
/// otherwise default to "json" when stdout is piped (non-TTY) and "terminal"
/// when running interactively.
pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None if std::io::stdout().is_terminal() => "terminal".to_string(),
        None => "json".to_string(),
    }
}

/// Resolve whether JSON output should be emitted.
/// `--json` always wins and conflicts with explicit non-json legacy formats.
pub fn resolve_json_output(
    json_flag: bool,
    legacy_format: Option<&str>,
    legacy_flag_name: &str,
) -> Result<bool> {
    if json_flag {
        if let Some(fmt) = legacy_format {
            if !fmt.eq_ignore_ascii_case("json") {
                anyhow::bail!(
                    "Conflicting output flags: --json and {} {}",
                    legacy_flag_name,
                    fmt
                );
            }
        }
        return Ok(true);
    }

    Ok(legacy_format.is_some_and(|fmt| fmt.eq_ignore_ascii_case("json")))
}

pub fn warn_if_legacy_flag_used(alias: &str, canonical: &str) {
    if !deprecation_warnings_enabled() || global_quiet_enabled() || global_json_enabled() {
        return;
    }
    let used = std::env::args().any(|arg| arg == alias);
    if used {
        eprintln!(
            "warning: `{alias}` is deprecated; use `{canonical}` (to be removed after 2 minor releases)."
        );
    }
}

pub fn warn_if_legacy_flags_used(pairs: &[(&str, &str)]) {
    for (alias, canonical) in pairs {
        warn_if_legacy_flag_used(alias, canonical);
    }
}

pub fn global_quiet_enabled() -> bool {
    std::env::args().any(|arg| arg == "-q" || arg == "--quiet")
}

pub fn global_json_enabled() -> bool {
    std::env::args().any(|arg| arg == "--json")
}

pub fn deprecation_warnings_enabled() -> bool {
    match std::env::var("COVY_DEPRECATION_WARNINGS") {
        Ok(v) => {
            let normalized = v.trim().to_ascii_lowercase();
            normalized == "1" || normalized == "true" || normalized == "yes" || normalized == "on"
        }
        Err(_) => false,
    }
}

pub fn maybe_warn_deprecated(message: &str) {
    if deprecation_warnings_enabled() && !global_quiet_enabled() && !global_json_enabled() {
        eprintln!("{message}");
    }
}

/// Deserialize JSON with a helpful error message that includes an example of
/// the expected JSON shape.
pub fn deserialize_json_with_example<T: serde::de::DeserializeOwned>(
    input: &str,
    type_name: &str,
    example: &str,
) -> anyhow::Result<T> {
    serde_json::from_str(input).map_err(|e| {
        anyhow::anyhow!("Failed to parse {type_name}: {e}\n\nExpected JSON shape:\n{example}")
    })
}

pub fn default_pipeline_ingest_adapters() -> covy_core::pipeline::PipelineIngestAdapters {
    covy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto: ingest_coverage_auto,
        ingest_coverage_with_format: ingest_coverage_with_format,
        ingest_coverage_stdin: ingest_coverage_stdin,
        ingest_diagnostics: ingest_diagnostics,
    }
}

pub fn default_impact_adapters() -> covy_core::impact_pipeline::ImpactAdapters {
    covy_core::impact_pipeline::ImpactAdapters {
        ingest_coverage_auto: ingest_coverage_auto,
        ingest_coverage_with_format: ingest_coverage_with_format,
        git_diff: impact_git_diff,
    }
}

pub fn default_testmap_adapters() -> covy_core::testmap_pipeline::TestMapAdapters {
    covy_core::testmap_pipeline::TestMapAdapters {
        ingest_coverage: ingest_coverage_auto,
    }
}

fn ingest_coverage_auto(path: &Path) -> Result<CoverageData> {
    covy_ingest::ingest_path(path).map_err(Into::into)
}

fn ingest_coverage_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData> {
    covy_ingest::ingest_path_with_format(path, format).map_err(Into::into)
}

fn ingest_coverage_stdin(format: CoverageFormat) -> Result<CoverageData> {
    covy_ingest::ingest_reader(std::io::stdin().lock(), format).map_err(Into::into)
}

fn ingest_diagnostics(path: &Path) -> Result<DiagnosticsData> {
    covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

fn impact_git_diff(base: &str, head: &str) -> Result<Vec<FileDiff>> {
    covy_core::diff::git_diff(base, head).map_err(Into::into)
}

pub fn load_coverage_state(path: &str) -> Result<CoverageData> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read coverage state at {path}"))?;
    covy_core::cache::deserialize_coverage(&bytes).map_err(Into::into)
}

pub fn load_diagnostics_if_present(path: &str) -> Result<Option<DiagnosticsData>> {
    if !Path::new(path).exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let mut data = covy_core::cache::deserialize_diagnostics(&bytes)?;
    covy_core::pathmap::auto_normalize_issue_paths(&mut data, None);
    Ok(Some(data))
}

pub fn compute_uncovered_blocks_generic<T, F>(
    coverage: &CoverageData,
    diffs: &[FileDiff],
    mut make_block: F,
) -> Vec<T>
where
    F: FnMut(&FileDiff, u32, u32) -> T,
{
    let mut blocks = Vec::new();

    for diff in diffs {
        let mut uncovered = RoaringBitmap::new();
        if let Some(fc) = coverage.files.get(&diff.path) {
            let missing = &fc.lines_instrumented - &fc.lines_covered;
            uncovered |= &(&diff.changed_lines & &missing);
        } else {
            uncovered |= &diff.changed_lines;
        }

        let lines: Vec<u32> = uncovered.iter().collect();
        if lines.is_empty() {
            continue;
        }

        let mut start = lines[0];
        let mut end = lines[0];
        for line in lines.iter().skip(1) {
            if *line == end + 1 {
                end = *line;
            } else {
                blocks.push(make_block(diff, start, end));
                start = *line;
                end = *line;
            }
        }
        blocks.push(make_block(diff, start, end));
    }

    blocks
}

pub struct PrSharedState {
    pub config: CovyConfig,
    pub coverage: CoverageData,
    pub diagnostics: Option<DiagnosticsData>,
    pub diffs: Vec<FileDiff>,
    pub gate: covy_core::model::QualityGateResult,
}

pub fn compute_pr_shared_state(
    config_path: &str,
    base_ref: Option<&str>,
    head_ref: Option<&str>,
    coverage_state_path: &str,
    diagnostics_state_path: &str,
) -> Result<PrSharedState> {
    let config = CovyConfig::load(Path::new(config_path))?;
    let base = base_ref.unwrap_or(&config.diff.base);
    let head = head_ref.unwrap_or(&config.diff.head);

    let mut coverage = load_coverage_state(coverage_state_path)?;
    covy_core::pathmap::auto_normalize_paths(&mut coverage, None);

    let diffs = covy_core::diff::git_diff(base, head)?;
    let diagnostics = load_diagnostics_if_present(diagnostics_state_path)?;

    let gate = covy_core::gate::evaluate_full_gate(
        &GateConfig {
            fail_under_total: config.gate.fail_under_total,
            fail_under_changed: config.gate.fail_under_changed,
            fail_under_new: config.gate.fail_under_new,
            issues: config.gate.issues.clone(),
        },
        &coverage,
        diagnostics.as_ref(),
        &diffs,
    );

    Ok(PrSharedState {
        config,
        coverage,
        diagnostics,
        diffs,
        gate,
    })
}

/// Detect the git repository root directory.
pub fn detect_repo_root() -> Result<std::path::PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to detect git repository root")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to detect git repository root: {stderr}");
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        anyhow::bail!("Git repository root resolved to an empty path");
    }
    Ok(std::path::PathBuf::from(root))
}

/// Resolve glob patterns into matching file paths.
pub fn resolve_report_globs(patterns: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    for pattern in patterns {
        let matches: Vec<_> = glob::glob(pattern)
            .with_context(|| format!("Invalid glob pattern: {pattern}"))?
            .filter_map(|r| r.ok())
            .collect();
        files.extend(matches);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

pub fn resolve_report_globs_for_config(
    config_path: &str,
    patterns: &[String],
) -> Result<Vec<std::path::PathBuf>> {
    let base = config_base_dir(config_path)?;
    let cwd = std::env::current_dir().ok();
    let repo_root = detect_repo_root().ok();
    let mut adjusted = Vec::new();

    for pattern in patterns {
        let path = Path::new(pattern);
        if path.is_absolute() {
            adjusted.push(pattern.clone());
            continue;
        }

        let mut candidates: BTreeSet<OsString> = BTreeSet::new();
        candidates.insert(OsString::from(pattern));
        candidates.insert(base.join(path).into_os_string());
        if let Some(cwd) = &cwd {
            candidates.insert(cwd.join(path).into_os_string());
        }
        if let Some(repo_root) = &repo_root {
            candidates.insert(repo_root.join(path).into_os_string());
        }

        for candidate in candidates {
            adjusted.push(candidate.to_string_lossy().to_string());
        }
    }
    resolve_report_globs(&adjusted)
}

fn config_base_dir(config_path: &str) -> Result<PathBuf> {
    let path = Path::new(config_path);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = abs.parent().map(Path::to_path_buf).unwrap_or(abs);
    Ok(parent)
}

#[cfg(test)]
pub(crate) fn cwd_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_report_globs_for_config_uses_config_dir() {
        let _guard = cwd_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_dir = dir.path().join("project");
        let reports_dir = cfg_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::write(reports_dir.join("lcov.info"), "TN:\n").unwrap();

        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let matches =
            resolve_report_globs_for_config("project/covy.toml", &[String::from("reports/*.info")])
                .unwrap();

        std::env::set_current_dir(old).unwrap();

        assert!(!matches.is_empty());
        assert!(matches.iter().any(|p| p.ends_with("lcov.info")));
    }

    #[test]
    fn test_resolve_report_globs_for_config_accepts_repo_relative_pattern() {
        let _guard = cwd_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_dir = dir.path().join("project");
        let reports_dir = cfg_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::write(reports_dir.join("a.info"), "TN:\n").unwrap();

        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let matches = resolve_report_globs_for_config(
            "project/covy.toml",
            &[String::from("project/reports/*.info")],
        )
        .unwrap();

        std::env::set_current_dir(old).unwrap();

        assert!(!matches.is_empty());
        assert!(matches.iter().any(|p| p.ends_with("a.info")));
    }
}
