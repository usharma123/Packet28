use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;
use suite_packet_core::{CoverageData, CoverageFormat, FileDiff};

pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None if std::io::stdout().is_terminal() => "terminal".to_string(),
        None => "json".to_string(),
    }
}

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

pub fn default_pipeline_ingest_adapters() -> diffy_core::pipeline::PipelineIngestAdapters {
    diffy_core::pipeline::PipelineIngestAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        ingest_coverage_stdin,
        ingest_diagnostics,
    }
}

pub fn default_impact_adapters() -> testy_core::pipeline::ImpactAdapters {
    testy_core::pipeline::ImpactAdapters {
        ingest_coverage_auto,
        ingest_coverage_with_format,
        git_diff: impact_git_diff,
    }
}

pub fn default_testmap_adapters() -> testy_core::pipeline_testmap::TestMapAdapters {
    testy_core::pipeline_testmap::TestMapAdapters {
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

fn ingest_diagnostics(path: &Path) -> Result<suite_packet_core::diagnostics::DiagnosticsData> {
    covy_ingest::ingest_diagnostics_path(path).map_err(Into::into)
}

fn impact_git_diff(base: &str, head: &str) -> Result<Vec<FileDiff>> {
    diffy_core::diff::git_diff(base, head).map_err(Into::into)
}
