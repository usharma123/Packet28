use crate::config::{GateConfig, IssueGateConfig};
use crate::diagnostics::{DiagnosticsData, Severity};
use crate::model::{CoverageData, DiffStatus, FileDiff, IssueGateCounts, QualityGateResult};

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
        issue_counts: None,
    }
}

/// Evaluate issue gates on changed lines.
pub fn evaluate_issue_gate(
    config: &IssueGateConfig,
    diagnostics: &DiagnosticsData,
    diffs: &[FileDiff],
) -> (bool, Vec<String>, IssueGateCounts) {
    let mut changed_errors = 0u32;
    let mut changed_warnings = 0u32;
    let mut changed_notes = 0u32;

    for issue in diagnostics.issues_on_changed_lines(diffs) {
        match issue.severity {
            Severity::Error => changed_errors += 1,
            Severity::Warning => changed_warnings += 1,
            Severity::Note => changed_notes += 1,
        }
    }

    let changed_total = changed_errors + changed_warnings + changed_notes;
    let counts = IssueGateCounts {
        changed_errors,
        changed_warnings,
        changed_notes,
        total_issues: diagnostics.total_issues(),
    };

    let mut passed = true;
    let mut violations = Vec::new();

    if let Some(max) = config.max_new_errors {
        if changed_errors > max {
            passed = false;
            violations.push(format!(
                "Changed-line errors {changed_errors} exceed max_new_errors {max}"
            ));
        }
    }

    if let Some(max) = config.max_new_warnings {
        if changed_warnings > max {
            passed = false;
            violations.push(format!(
                "Changed-line warnings {changed_warnings} exceed max_new_warnings {max}"
            ));
        }
    }

    if let Some(max) = config.max_new_issues {
        if changed_total > max {
            passed = false;
            violations.push(format!(
                "Changed-line total issues {changed_total} exceed max_new_issues {max}"
            ));
        }
    }

    (passed, violations, counts)
}

/// Evaluate coverage gates and optional issue gates together.
pub fn evaluate_full_gate(
    config: &GateConfig,
    coverage: &CoverageData,
    diagnostics: Option<&DiagnosticsData>,
    diffs: &[FileDiff],
) -> QualityGateResult {
    let mut result = evaluate_gate(config, coverage, diffs);

    if let Some(diag) = diagnostics {
        let (issues_passed, issue_violations, issue_counts) =
            evaluate_issue_gate(&config.issues, diag, diffs);
        result.passed = result.passed && issues_passed;
        result.violations.extend(issue_violations);
        result.issue_counts = Some(issue_counts);
    }

    result
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
    use crate::diagnostics::{Issue, Severity};
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

    fn make_issue(path: &str, line: u32, severity: Severity, fingerprint: &str) -> Issue {
        Issue {
            path: path.to_string(),
            line,
            column: None,
            end_line: None,
            severity,
            rule_id: "test-rule".to_string(),
            message: "test message".to_string(),
            source: "test-tool".to_string(),
            fingerprint: fingerprint.to_string(),
        }
    }

    #[test]
    fn test_gate_passes() {
        let config = GateConfig {
            fail_under_total: Some(50.0),
            fail_under_changed: Some(50.0),
            ..GateConfig::default()
        };
        let coverage = make_coverage(vec![("src/main.rs", vec![1, 2, 3], vec![1, 2, 3, 4])]);
        let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2])];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn test_gate_fails_total() {
        let config = GateConfig {
            fail_under_total: Some(90.0),
            ..GateConfig::default()
        };
        let coverage = make_coverage(vec![(
            "src/main.rs",
            vec![1, 2],
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        )]);
        let diffs = vec![];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
        assert!(result.violations[0].contains("Total coverage"));
    }

    #[test]
    fn test_gate_fails_changed() {
        let config = GateConfig {
            fail_under_changed: Some(80.0),
            ..GateConfig::default()
        };
        let coverage = make_coverage(vec![("src/main.rs", vec![1], vec![1, 2, 3, 4, 5])]);
        // Changed lines 1..=5, only line 1 is covered = 20%
        let diffs = vec![make_diff(
            "src/main.rs",
            DiffStatus::Modified,
            vec![1, 2, 3, 4, 5],
        )];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(!result.passed);
        assert!(result.violations[0].contains("Changed lines coverage"));
    }

    #[test]
    fn test_gate_new_file_coverage() {
        let config = GateConfig {
            fail_under_new: Some(75.0),
            ..GateConfig::default()
        };
        let coverage = make_coverage(vec![("src/new.rs", vec![1, 2, 3], vec![1, 2, 3, 4])]);
        let diffs = vec![make_diff("src/new.rs", DiffStatus::Added, vec![1, 2, 3, 4])];

        let result = evaluate_gate(&config, &coverage, &diffs);
        assert!(result.passed); // 75% == threshold
    }

    #[test]
    fn test_gate_no_coverage_data() {
        let config = GateConfig {
            fail_under_total: Some(80.0),
            ..GateConfig::default()
        };
        let coverage = CoverageData::new();
        let diffs = vec![];

        let result = evaluate_gate(&config, &coverage, &diffs);
        // No instrumented lines → no total coverage → threshold not checked
        assert!(result.passed);
    }

    #[test]
    fn test_issue_gate_thresholds() {
        let mut diagnostics = DiagnosticsData::new();
        diagnostics.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![
                make_issue("src/main.rs", 10, Severity::Error, "fp1"),
                make_issue("src/main.rs", 11, Severity::Warning, "fp2"),
                make_issue("src/main.rs", 50, Severity::Note, "fp3"),
            ],
        );

        let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![10, 11])];
        let config = IssueGateConfig {
            max_new_errors: Some(0),
            max_new_warnings: Some(1),
            max_new_issues: Some(2),
        };

        let (passed, violations, counts) = evaluate_issue_gate(&config, &diagnostics, &diffs);
        assert!(!passed);
        assert_eq!(counts.changed_errors, 1);
        assert_eq!(counts.changed_warnings, 1);
        assert_eq!(counts.changed_notes, 0);
        assert_eq!(counts.total_issues, 3);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("max_new_errors"));
    }

    #[test]
    fn test_full_gate_combines_coverage_and_issues() {
        let mut diagnostics = DiagnosticsData::new();
        diagnostics.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![make_issue("src/main.rs", 2, Severity::Error, "fp1")],
        );

        let coverage = make_coverage(vec![("src/main.rs", vec![1], vec![1, 2])]);
        let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2])];

        let config = GateConfig {
            fail_under_changed: Some(60.0),
            issues: IssueGateConfig {
                max_new_errors: Some(0),
                ..IssueGateConfig::default()
            },
            ..GateConfig::default()
        };

        let result = evaluate_full_gate(&config, &coverage, Some(&diagnostics), &diffs);
        assert!(!result.passed);
        assert_eq!(result.issue_counts.as_ref().unwrap().changed_errors, 1);
        assert_eq!(result.violations.len(), 2);
    }
}
