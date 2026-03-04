use std::path::Path;

use anyhow::Result;
use suite_packet_core::{CoverageData, CoverageFormat};

pub fn resolve_report_format(explicit: Option<&str>) -> String {
    match explicit {
        Some(fmt) => fmt.to_string(),
        None => "terminal".to_string(),
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
