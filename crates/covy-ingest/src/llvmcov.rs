use covy_core::model::{CoverageData, CoverageFormat};
use covy_core::CovyError;
use serde::Deserialize;

use crate::Ingestor;

pub struct LlvmCovIngestor;

impl Ingestor for LlvmCovIngestor {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::LlvmCov
    }

    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError> {
        if data.is_empty() {
            return Err(CovyError::EmptyInput {
                path: "(llvm-cov input)".into(),
            });
        }
        parse_llvmcov(data)
    }
}

/// Top-level llvm-cov JSON export structure.
#[derive(Deserialize)]
struct LlvmCovExport {
    data: Vec<LlvmCovData>,
}

#[derive(Deserialize)]
struct LlvmCovData {
    files: Vec<LlvmCovFile>,
}

#[derive(Deserialize)]
struct LlvmCovFile {
    filename: String,
    segments: Vec<Vec<serde_json::Value>>,
}

fn parse_llvmcov(data: &[u8]) -> Result<CoverageData, CovyError> {
    let export: LlvmCovExport = serde_json::from_slice(data).map_err(|e| CovyError::Parse {
        format: "llvm-cov".into(),
        detail: format!("Invalid JSON: {e}"),
    })?;

    let mut result = CoverageData::new();
    result.format = Some(CoverageFormat::LlvmCov);

    for data_entry in &export.data {
        for file in &data_entry.files {
            let fc = result.files.entry(file.filename.clone()).or_default();

            // Walk segments to determine line coverage.
            // Each segment: [line, col, count, has_count, is_region_entry, ...]
            // Segments define regions: from one segment's line to the next segment's line.
            // A segment with has_count=true and count>0 means the region starting at that
            // segment is covered.
            let segments = &file.segments;
            for i in 0..segments.len() {
                let seg = &segments[i];
                if seg.len() < 5 {
                    continue;
                }

                let line = seg[0].as_u64().unwrap_or(0) as u32;
                let count = seg[2].as_u64().unwrap_or(0);
                let has_count = seg[3]
                    .as_bool()
                    .or_else(|| seg[3].as_u64().map(|v| v != 0))
                    .unwrap_or(false);

                if !has_count {
                    continue;
                }

                // Determine the end line for this segment
                let end_line = if i + 1 < segments.len() {
                    let next = &segments[i + 1];
                    let next_line = next[0].as_u64().unwrap_or(0) as u32;
                    // The segment covers from `line` up to (but not including) the next segment's line,
                    // unless they're on the same line.
                    if next_line > line {
                        next_line - 1
                    } else {
                        line
                    }
                } else {
                    line
                };

                for l in line..=end_line {
                    fc.lines_instrumented.insert(l);
                    if count > 0 {
                        fc.lines_covered.insert(l);
                    }
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_llvmcov_basic() {
        let json = r#"{
            "type": "llvm.coverage.json.export",
            "version": "2.0.1",
            "data": [{
                "files": [{
                    "filename": "src/main.rs",
                    "segments": [
                        [1, 1, 1, true, true],
                        [3, 1, 0, true, true],
                        [5, 1, 1, true, true],
                        [6, 1, 0, false, false]
                    ],
                    "summary": {
                        "lines": {"count": 5, "covered": 3, "percent": 60.0}
                    }
                }]
            }]
        }"#;

        let result = parse_llvmcov(json.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 1);
        let fc = &result.files["src/main.rs"];
        assert!(fc.lines_covered.contains(1));
        assert!(fc.lines_covered.contains(2));
        assert!(!fc.lines_covered.contains(3));
        assert!(fc.lines_instrumented.contains(3));
        assert!(fc.lines_covered.contains(5));
    }

    #[test]
    fn test_parse_llvmcov_empty_input() {
        let ingestor = LlvmCovIngestor;
        let result = ingestor.parse(b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_llvmcov_multiple_files() {
        let json = r#"{
            "data": [{
                "files": [
                    {
                        "filename": "src/a.rs",
                        "segments": [[1, 1, 1, true, true], [2, 1, 0, false, false]]
                    },
                    {
                        "filename": "src/b.rs",
                        "segments": [[1, 1, 0, true, true], [2, 1, 0, false, false]]
                    }
                ]
            }]
        }"#;

        let result = parse_llvmcov(json.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(result.files["src/a.rs"].lines_covered.contains(1));
        assert!(!result.files["src/b.rs"].lines_covered.contains(1));
        assert!(result.files["src/b.rs"].lines_instrumented.contains(1));
    }
}
