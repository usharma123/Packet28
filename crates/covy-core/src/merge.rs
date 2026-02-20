use std::path::PathBuf;

use crate::diagnostics::DiagnosticsData;
use crate::error::CovyError;
use crate::model::CoverageData;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct MergeSummary {
    pub coverage_inputs: usize,
    pub diagnostics_inputs: usize,
    pub skipped_inputs: usize,
    pub coverage_files_merged: usize,
    pub diagnostics_files_merged: usize,
}

pub fn merge_coverage_inputs(
    paths: &[PathBuf],
    strict: bool,
) -> Result<(CoverageData, usize), CovyError> {
    let mut merged = CoverageData::new();
    let mut skipped = 0usize;

    for path in paths {
        match std::fs::read(path)
            .map_err(CovyError::from)
            .and_then(|bytes| crate::cache::deserialize_coverage(&bytes))
        {
            Ok(data) => merged.merge(&data),
            Err(e) => {
                if strict {
                    return Err(CovyError::Cache(format!(
                        "Failed to merge coverage input {}: {e}",
                        path.display()
                    )));
                }
                skipped += 1;
            }
        }
    }

    Ok((merged, skipped))
}

pub fn merge_diagnostics_inputs(
    _paths: &[PathBuf],
    _strict: bool,
) -> Result<(DiagnosticsData, usize), CovyError> {
    Ok((DiagnosticsData::new(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileCoverage;

    #[test]
    fn test_merge_coverage_inputs_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p1 = dir.path().join("s1.bin");
        let p2 = dir.path().join("s2.bin");

        let mut c1 = CoverageData::new();
        let mut fc1 = FileCoverage::new();
        fc1.lines_instrumented.insert(1);
        fc1.lines_covered.insert(1);
        c1.files.insert("src/a.rs".to_string(), fc1);

        let mut c2 = CoverageData::new();
        let mut fc2 = FileCoverage::new();
        fc2.lines_instrumented.insert(2);
        c2.files.insert("src/b.rs".to_string(), fc2);

        std::fs::write(&p1, crate::cache::serialize_coverage(&c1).unwrap()).unwrap();
        std::fs::write(&p2, crate::cache::serialize_coverage(&c2).unwrap()).unwrap();

        let (merged, skipped) = merge_coverage_inputs(&[p1, p2], true).unwrap();
        assert_eq!(skipped, 0);
        assert!(merged.files.contains_key("src/a.rs"));
        assert!(merged.files.contains_key("src/b.rs"));
    }

    #[test]
    fn test_merge_coverage_inputs_non_strict_skips_bad_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, b"not-a-coverage-state").unwrap();

        let (merged, skipped) = merge_coverage_inputs(&[bad], false).unwrap();
        assert_eq!(skipped, 1);
        assert!(merged.files.is_empty());
    }
}
