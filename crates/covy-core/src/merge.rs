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
    paths: &[PathBuf],
    strict: bool,
) -> Result<(DiagnosticsData, usize), CovyError> {
    let mut merged = DiagnosticsData::new();
    let mut skipped = 0usize;

    for path in paths {
        match std::fs::read(path)
            .map_err(CovyError::from)
            .and_then(|bytes| crate::cache::deserialize_diagnostics(&bytes))
        {
            Ok(data) => merged.merge(&data),
            Err(e) => {
                if strict {
                    return Err(CovyError::Cache(format!(
                        "Failed to merge diagnostics input {}: {e}",
                        path.display()
                    )));
                }
                skipped += 1;
            }
        }
    }

    Ok((merged, skipped))
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

    #[test]
    fn test_merge_diagnostics_inputs_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let p1 = dir.path().join("d1.bin");
        let p2 = dir.path().join("d2.bin");

        let mut d1 = DiagnosticsData::new();
        d1.issues_by_file.insert(
            "src/a.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/a.rs".to_string(),
                line: 10,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Error,
                rule_id: "R1".to_string(),
                message: "m1".to_string(),
                source: "tool".to_string(),
                fingerprint: "fp1".to_string(),
            }],
        );
        let mut d2 = DiagnosticsData::new();
        d2.issues_by_file.insert(
            "src/b.rs".to_string(),
            vec![crate::diagnostics::Issue {
                path: "src/b.rs".to_string(),
                line: 20,
                column: None,
                end_line: None,
                severity: crate::diagnostics::Severity::Warning,
                rule_id: "R2".to_string(),
                message: "m2".to_string(),
                source: "tool".to_string(),
                fingerprint: "fp2".to_string(),
            }],
        );

        std::fs::write(&p1, crate::cache::serialize_diagnostics(&d1).unwrap()).unwrap();
        std::fs::write(&p2, crate::cache::serialize_diagnostics(&d2).unwrap()).unwrap();

        let (merged, skipped) = merge_diagnostics_inputs(&[p1, p2], true).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(merged.total_issues(), 2);
    }

    #[test]
    fn test_merge_diagnostics_inputs_non_strict_skips_bad_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, b"broken").unwrap();
        let (merged, skipped) = merge_diagnostics_inputs(&[bad], false).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(merged.total_issues(), 0);
    }
}
