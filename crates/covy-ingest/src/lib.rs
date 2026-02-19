pub mod cobertura;
pub mod gocov;
pub mod jacoco;
pub mod lcov;
pub mod llvmcov;

use std::path::Path;

use covy_core::model::{CoverageData, CoverageFormat};
use covy_core::CovyError;

/// Trait for coverage format parsers.
pub trait Ingestor: Send + Sync {
    fn format(&self) -> CoverageFormat;
    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError>;
}

/// Detect format from file extension and content sniffing.
pub fn detect_format(path: &Path, content: &[u8]) -> Result<CoverageFormat, CovyError> {
    // Check extension first
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext {
            "info" => return Ok(CoverageFormat::Lcov),
            _ => {}
        }
    }

    // Check filename
    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");

    if filename == "lcov.info" || filename.ends_with(".lcov") {
        return Ok(CoverageFormat::Lcov);
    }

    // Content sniffing
    let prefix = std::str::from_utf8(&content[..content.len().min(512)]).unwrap_or("");

    if prefix.starts_with("TN:") || prefix.starts_with("SF:") || prefix.contains("\nSF:") {
        return Ok(CoverageFormat::Lcov);
    }

    if prefix.starts_with("mode:") {
        return Ok(CoverageFormat::GoCov);
    }

    if prefix.contains("<coverage") || prefix.contains("<cobertura") {
        return Ok(CoverageFormat::Cobertura);
    }

    if prefix.contains("<!DOCTYPE report") || prefix.contains("<report") {
        return Ok(CoverageFormat::JaCoCo);
    }

    if prefix.contains("\"type\"") && prefix.contains("llvm.coverage.json.export") {
        return Ok(CoverageFormat::LlvmCov);
    }

    // Also detect by structure: { "data": [{ "files": ...
    if prefix.trim_start().starts_with('{') && prefix.contains("\"data\"") && prefix.contains("\"files\"") {
        return Ok(CoverageFormat::LlvmCov);
    }

    Err(CovyError::UnknownFormat {
        path: path.display().to_string(),
    })
}

/// Get the appropriate ingestor for a format.
pub fn get_ingestor(format: CoverageFormat) -> Box<dyn Ingestor> {
    match format {
        CoverageFormat::Lcov => Box::new(lcov::LcovIngestor),
        CoverageFormat::Cobertura => Box::new(cobertura::CoberturaIngestor),
        CoverageFormat::JaCoCo => Box::new(jacoco::JaCoCoIngestor),
        CoverageFormat::GoCov => Box::new(gocov::GoCovIngestor),
        CoverageFormat::LlvmCov => Box::new(llvmcov::LlvmCovIngestor),
    }
}

/// Convenience: ingest a file, auto-detecting format.
pub fn ingest_path(path: &Path) -> Result<CoverageData, CovyError> {
    let content = std::fs::read(path)?;
    let format = detect_format(path, &content)?;
    let ingestor = get_ingestor(format);
    ingestor.parse(&content)
}

/// Ingest a file with a specified format.
pub fn ingest_path_with_format(path: &Path, format: CoverageFormat) -> Result<CoverageData, CovyError> {
    let content = std::fs::read(path)?;
    if content.is_empty() {
        return Err(CovyError::EmptyInput {
            path: path.display().to_string(),
        });
    }
    let ingestor = get_ingestor(format);
    ingestor.parse(&content)
}

/// Ingest coverage data from a reader (e.g. stdin) with a specified format.
pub fn ingest_reader<R: std::io::Read>(mut reader: R, format: CoverageFormat) -> Result<CoverageData, CovyError> {
    let mut content = Vec::new();
    reader.read_to_end(&mut content)?;
    if content.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(stdin)".into(),
        });
    }
    let ingestor = get_ingestor(format);
    ingestor.parse(&content)
}
