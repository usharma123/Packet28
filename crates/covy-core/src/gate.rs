use crate::config::GateConfig;
use crate::model::{CoverageData, DiffStatus, FileDiff, QualityGateResult};

/// Evaluate quality gates against coverage data and diffs.
pub fn evaluate_gate(
    config: &GateConfig,
    coverage: &CoverageData,
    diffs: &[FileDiff],
) -> QualityGateResult {
    let mut violations = Vec::new();
    let mut passed = true;

    // 1. Total coverage
    let total_coverage_pct = coverage.total_coverage_pct();
    if let (Some(threshold), Some(actual)) = (config.fail_under_total, total_coverage_pct) {
        if actual < threshold {
            passed = false;
            violations.push(format!(
                "Total coverage {actual:.1}% is below threshold {threshold:.1}%"
            ));
        }
    }

    // 2. Changed lines coverage
    let changed_coverage_pct = compute_changed_coverage(coverage, diffs);
    if let (Some(threshold), Some(actual)) = (config.fail_under_changed, changed_coverage_pct) {
        if actual < threshold {
            passed = false;
            violations.push(format!(
                "Changed lines coverage {actual:.1}% is below threshold {threshold:.1}%"
            ));
        }
    }

    // 3. New file coverage
    let new_file_coverage_pct = compute_new_file_coverage(coverage, diffs);
    if let (Some(threshold), Some(actual)) = (config.fail_under_new, new_file_coverage_pct) {
        if actual < threshold {
            passed = false;
            violations.push(format!(
                "New file coverage {actual:.1}% is below threshold {threshold:.1}%"
            ));
        }
    }

    QualityGateResult {
        passed,
        total_coverage_pct,
        changed_coverage_pct,
        new_file_coverage_pct,
        violations,
    }
}

/// Compute coverage percentage for changed lines across all diffs.
fn compute_changed_coverage(coverage: &CoverageData, diffs: &[FileDiff]) -> Option<f64> {
    let mut total_changed = 0u64;
    let mut total_covered = 0u64;

    for diff in diffs {
        if diff.status == DiffStatus::Deleted {
            continue;
        }
        if diff.changed_lines.is_empty() {
            continue;
        }

        if let Some(fc) = coverage.files.get(&diff.path) {
            // Intersect changed lines with instrumented lines
            let changed_instrumented = &diff.changed_lines & &fc.lines_instrumented;
            let changed_covered = &diff.changed_lines & &fc.lines_covered;
            total_changed += changed_instrumented.len();
            total_covered += changed_covered.len();
        } else {
            // File has changes but no coverage data — count all changed as uncovered
            total_changed += diff.changed_lines.len();
        }
    }

    if total_changed == 0 {
        return None;
    }
    Some((total_covered as f64 / total_changed as f64) * 100.0)
}

/// Compute coverage percentage for newly added files.
fn compute_new_file_coverage(coverage: &CoverageData, diffs: &[FileDiff]) -> Option<f64> {
    let mut total_instrumented = 0u64;
    let mut total_covered = 0u64;

    for diff in diffs {
        if diff.status != DiffStatus::Added {
            continue;
        }
        if let Some(fc) = coverage.files.get(&diff.path) {
            total_instrumented += fc.lines_instrumented.len();
            total_covered += fc.lines_covered.len();
        }
    }

    if total_instrumented == 0 {
        return None;
    }
    Some((total_covered as f64 / total_instrumented as f64) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileCoverage;
    use roaring::RoaringBitmap;

    fn make_coverage(files: Vec<(&str, Vec<u32>, Vec<u32>)>) -> CoverageData {
        let mut data = CoverageData::new();
        for (path, covered, instrumented) in files {
            let mut fc = FileCoverage::new();
            for l in covered {
                fc.lines_covered.insert(l);
            }
            for l in instrumented {
                fc.lines_instrumented.insert(l);
            }
            data.files.insert(path.to_string(), fc);
        }
        data
    }

    fn make_diff(path: &str, status: DiffStatus, lines: Vec<u32>) -> FileDiff {
        let mut changed_lines = RoaringBitmap::new();
        for l in lines {
            changed_lines.insert(l);
        }
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status,
            changed_lines,
        }
    }

    #[test]
    fn test_gate_passes() {
        let config = GateConfig {
            fail_under_total: Some(50.0),
            fail_under_changed: Some(50.0),
            fail_under_new: None,
        };
        let coverage = make_coverage(vec![
            ("src/main.rs", vec![1, 2, 3], vec![1, 2, 3, 4]),
        ]);
        let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2])];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn test_gate_fails_total() {
        let config = GateConfig {
            fail_under_total: Some(90.0),
            fail_under_changed: None,
            fail_under_new: None,
        };
        let coverage = make_coverage(vec![
            ("src/main.rs", vec![1, 2], vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        ]);
        let diffs = vec![];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].contains("Total coverage"));
    }

    #[test]
    fn test_gate_fails_changed() {
        let config = GateConfig {
            fail_under_total: None,
            fail_under_changed: Some(80.0),
            fail_under_new: None,
        };
        let coverage = make_coverage(vec![
            ("src/main.rs", vec![1], vec![1, 2, 3, 4, 5]),
        ]);
        // Changed lines 1..=5, only line 1 is covered = 20%
        let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2, 3, 4, 5])];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(!result.passed);
        assert!(result.violations[0].contains("Changed lines coverage"));
    }

    #[test]
    fn test_gate_new_file_coverage() {
        let config = GateConfig {
            fail_under_total: None,
            fail_under_changed: None,
            fail_under_new: Some(75.0),
        };
        let coverage = make_coverage(vec![
            ("src/new.rs", vec![1, 2, 3], vec![1, 2, 3, 4]),
        ]);
        let diffs = vec![make_diff("src/new.rs", DiffStatus::Added, vec![1, 2, 3, 4])];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(result.passed); // 75% == threshold
    }

    #[test]
    fn test_gate_no_coverage_data() {
        let config = GateConfig {
            fail_under_total: Some(80.0),
            fail_under_changed: None,
            fail_under_new: None,
        };
        let coverage = CoverageData::new();
        let diffs = vec![];

        let result = evaluate_gate(&config, &coverage, &diffs);
        // No instrumented lines → no total coverage → threshold not checked
        assert!(result.passed);
    }
}
