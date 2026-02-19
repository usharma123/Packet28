use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

use crate::model::FileDiff;

/// Severity of a diagnostic issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Note => write!(f, "note"),
        }
    }
}

/// A single diagnostic issue (lint warning, type error, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub path: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub severity: Severity,
    pub rule_id: String,
    pub message: String,
    pub source: String,
    pub fingerprint: String,
}

/// Source format of diagnostics data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticsFormat {
    Sarif,
}

/// Aggregated diagnostics data from one or more reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsData {
    pub issues_by_file: BTreeMap<String, Vec<Issue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<DiagnosticsFormat>,
    pub timestamp: u64,
}

impl DiagnosticsData {
    pub fn new() -> Self {
        Self {
            issues_by_file: BTreeMap::new(),
            format: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    pub fn total_issues(&self) -> usize {
        self.issues_by_file.values().map(|v| v.len()).sum()
    }

    pub fn count_by_severity(&self) -> BTreeMap<Severity, usize> {
        let mut counts = BTreeMap::new();
        for issues in self.issues_by_file.values() {
            for issue in issues {
                *counts.entry(issue.severity).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Merge another DiagnosticsData into this one, deduplicating by fingerprint.
    pub fn merge(&mut self, other: &DiagnosticsData) {
        for (path, issues) in &other.issues_by_file {
            let existing = self.issues_by_file.entry(path.clone()).or_default();
            let mut seen: HashSet<String> =
                HashSet::with_capacity(existing.len().saturating_add(issues.len()));
            seen.extend(existing.iter().map(|issue| issue.fingerprint.clone()));
            for issue in issues {
                if seen.insert(issue.fingerprint.clone()) {
                    existing.push(issue.clone());
                }
            }
        }
    }

    /// Return issues that fall on changed lines in the given diffs.
    pub fn issues_on_changed_lines(&self, diffs: &[FileDiff]) -> Vec<&Issue> {
        let mut result = Vec::new();
        for diff in diffs {
            if let Some(issues) = self.issues_by_file.get(&diff.path) {
                for issue in issues {
                    if diff.changed_lines.contains(issue.line) {
                        result.push(issue);
                    }
                }
            }
        }
        result
    }
}

impl Default for DiagnosticsData {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;

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
    fn test_merge_dedup() {
        let mut d1 = DiagnosticsData::new();
        d1.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![make_issue("src/main.rs", 10, Severity::Error, "fp1")],
        );

        let mut d2 = DiagnosticsData::new();
        d2.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![
                make_issue("src/main.rs", 10, Severity::Error, "fp1"), // duplicate
                make_issue("src/main.rs", 20, Severity::Warning, "fp2"),
            ],
        );

        d1.merge(&d2);
        assert_eq!(d1.issues_by_file["src/main.rs"].len(), 2);
    }

    #[test]
    fn test_count_by_severity() {
        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "a.rs".to_string(),
            vec![
                make_issue("a.rs", 1, Severity::Error, "fp1"),
                make_issue("a.rs", 2, Severity::Error, "fp2"),
                make_issue("a.rs", 3, Severity::Warning, "fp3"),
            ],
        );
        data.issues_by_file.insert(
            "b.rs".to_string(),
            vec![make_issue("b.rs", 1, Severity::Note, "fp4")],
        );

        let counts = data.count_by_severity();
        assert_eq!(counts[&Severity::Error], 2);
        assert_eq!(counts[&Severity::Warning], 1);
        assert_eq!(counts[&Severity::Note], 1);
    }

    #[test]
    fn test_issues_on_changed_lines() {
        let mut data = DiagnosticsData::new();
        data.issues_by_file.insert(
            "src/main.rs".to_string(),
            vec![
                make_issue("src/main.rs", 5, Severity::Error, "fp1"),
                make_issue("src/main.rs", 10, Severity::Warning, "fp2"),
                make_issue("src/main.rs", 20, Severity::Note, "fp3"),
            ],
        );

        let mut changed = RoaringBitmap::new();
        changed.insert(5);
        changed.insert(10);
        let diffs = vec![crate::model::FileDiff {
            path: "src/main.rs".to_string(),
            old_path: None,
            status: crate::model::DiffStatus::Modified,
            changed_lines: changed,
        }];

        let on_changed = data.issues_on_changed_lines(&diffs);
        assert_eq!(on_changed.len(), 2);
        assert_eq!(on_changed[0].line, 5);
        assert_eq!(on_changed[1].line, 10);
    }
}
