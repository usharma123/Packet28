use covy_core::config::{GateConfig, IssueGateConfig};
use covy_core::diagnostics::{DiagnosticsData, Issue, Severity};
use covy_core::gate::{evaluate_full_gate, evaluate_gate};
use covy_core::model::*;
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

fn make_issue(path: &str, line: u32, severity: Severity, fp: &str) -> Issue {
    Issue {
        path: path.to_string(),
        line,
        column: None,
        end_line: None,
        severity,
        rule_id: "rule".to_string(),
        message: "message".to_string(),
        source: "tool".to_string(),
        fingerprint: fp.to_string(),
    }
}

#[test]
fn test_gate_all_thresholds_pass() {
    let config = GateConfig {
        fail_under_total: Some(70.0),
        fail_under_changed: Some(80.0),
        fail_under_new: Some(50.0),
        ..GateConfig::default()
    };

    let coverage = make_coverage(vec![
        (
            "src/main.rs",
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        ),
        ("src/new.rs", vec![1, 2, 3], vec![1, 2, 3, 4]),
    ]);

    let diffs = vec![
        make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2, 3, 4, 5]),
        make_diff("src/new.rs", DiffStatus::Added, vec![1, 2, 3, 4]),
    ];

    let result = evaluate_gate(&config, &coverage, &diffs);
    assert!(result.passed);
    assert!(result.violations.is_empty());
    assert!(result.total_coverage_pct.unwrap() > 70.0);
}

#[test]
fn test_gate_changed_lines_fail() {
    let config = GateConfig {
        fail_under_changed: Some(90.0),
        ..GateConfig::default()
    };

    // Only 1 out of 5 changed lines covered = 20%
    let coverage = make_coverage(vec![("src/main.rs", vec![1], vec![1, 2, 3, 4, 5])]);
    let diffs = vec![make_diff(
        "src/main.rs",
        DiffStatus::Modified,
        vec![1, 2, 3, 4, 5],
    )];

    let result = evaluate_gate(&config, &coverage, &diffs);
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn test_gate_no_thresholds() {
    let config = GateConfig::default();

    let coverage = CoverageData::new();
    let diffs = vec![];

    let result = evaluate_gate(&config, &coverage, &diffs);
    assert!(result.passed);
}

#[test]
fn test_gate_deleted_files_ignored() {
    let config = GateConfig {
        fail_under_changed: Some(100.0),
        ..GateConfig::default()
    };

    let coverage = make_coverage(vec![("src/existing.rs", vec![1, 2], vec![1, 2])]);

    let diffs = vec![
        make_diff("src/deleted.rs", DiffStatus::Deleted, vec![1, 2, 3]),
        make_diff("src/existing.rs", DiffStatus::Modified, vec![1, 2]),
    ];

    let result = evaluate_gate(&config, &coverage, &diffs);
    assert!(result.passed); // deleted files not counted, existing has 100% on changed lines
}

#[test]
fn test_gate_uncovered_changed_file() {
    let config = GateConfig {
        fail_under_changed: Some(50.0),
        ..GateConfig::default()
    };

    // File has changes but no coverage data at all
    let coverage = CoverageData::new();
    let diffs = vec![make_diff(
        "src/no_coverage.rs",
        DiffStatus::Modified,
        vec![1, 2],
    )];

    let result = evaluate_gate(&config, &coverage, &diffs);
    assert!(!result.passed); // 0% < 50%
}

#[test]
fn test_full_gate_issue_failure() {
    let config = GateConfig {
        issues: IssueGateConfig {
            max_new_errors: Some(0),
            ..IssueGateConfig::default()
        },
        ..GateConfig::default()
    };

    let coverage = make_coverage(vec![("src/main.rs", vec![1, 2], vec![1, 2])]);
    let diffs = vec![make_diff("src/main.rs", DiffStatus::Modified, vec![1, 2])];

    let mut diagnostics = DiagnosticsData::new();
    diagnostics.issues_by_file.insert(
        "src/main.rs".to_string(),
        vec![make_issue("src/main.rs", 2, Severity::Error, "fp1")],
    );

    let result = evaluate_full_gate(&config, &coverage, Some(&diagnostics), &diffs);
    assert!(!result.passed);
    assert_eq!(result.issue_counts.unwrap().changed_errors, 1);
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn test_full_gate_with_no_diagnostics_keeps_legacy_behavior() {
    let config = GateConfig {
        fail_under_total: Some(50.0),
        ..GateConfig::default()
    };

    let coverage = make_coverage(vec![("src/main.rs", vec![1], vec![1, 2])]);
    let result = evaluate_full_gate(&config, &coverage, None, &[]);

    assert!(result.passed);
    assert!(result.issue_counts.is_none());
}
