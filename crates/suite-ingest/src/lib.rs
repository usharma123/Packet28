use std::io::Read;
use std::path::Path;

use suite_packet_core::{CoverageData, CoverageFormat, CovyError, DiagnosticsData};

pub fn ingest_coverage_path(
    path: &Path,
    format: Option<CoverageFormat>,
) -> Result<CoverageData, CovyError> {
    match format {
        Some(format) => covy_ingest::ingest_path_with_format(path, format),
        None => covy_ingest::ingest_path(path),
    }
}

pub fn ingest_coverage_paths(
    paths: &[String],
    format: Option<CoverageFormat>,
) -> Result<CoverageData, CovyError> {
    let mut merged = CoverageData::new();
    for path in paths {
        let data = ingest_coverage_path(Path::new(path), format)?;
        merged.merge(&data);
    }
    Ok(merged)
}

pub fn ingest_coverage_stdin(format: CoverageFormat) -> Result<CoverageData, CovyError> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .map_err(CovyError::IoRaw)?;
    covy_ingest::ingest_reader(bytes.as_slice(), format)
}

pub fn ingest_diagnostics_path(path: &Path) -> Result<DiagnosticsData, CovyError> {
    covy_ingest::ingest_diagnostics_path(path)
}

pub fn ingest_diagnostics_paths(paths: &[String]) -> Result<DiagnosticsData, CovyError> {
    let mut merged = DiagnosticsData::new();
    for path in paths {
        let data = ingest_diagnostics_path(Path::new(path))?;
        merged.merge(&data);
    }
    Ok(merged)
}
