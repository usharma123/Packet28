//! Canonical output types for structured reducer results.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Structured test run result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TestResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: Option<u64>,
    pub failures: Vec<TestFailure>,
}

impl TestResult {
    pub fn summary_line(&self) -> String {
        if self.failed > 0 {
            format!(
                "{} failed, {} passed{}",
                self.failed,
                self.passed,
                self.skipped_suffix()
            )
        } else {
            format!("{} passed{}", self.passed, self.skipped_suffix())
        }
    }

    fn skipped_suffix(&self) -> String {
        if self.skipped > 0 {
            format!(", {} skipped", self.skipped)
        } else {
            String::new()
        }
    }
}

/// A single test failure with location and error details.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TestFailure {
    pub test_name: String,
    pub file_path: Option<String>,
    pub error_message: Option<String>,
    pub stack_trace: Option<String>,
}

/// Structured lint/check result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LintResult {
    pub total: usize,
    pub by_rule: BTreeMap<String, Vec<LintIssue>>,
}

impl LintResult {
    pub fn summary_line(&self) -> String {
        let rule_count = self.by_rule.len();
        if rule_count > 0 {
            let top_rule = self
                .by_rule
                .iter()
                .max_by_key(|(_, issues)| issues.len())
                .map(|(rule, issues)| format!("{} ({})", rule, issues.len()))
                .unwrap_or_default();
            format!("{} issues across {} rules; top: {}", self.total, rule_count, top_rule)
        } else {
            format!("{} issues", self.total)
        }
    }

    pub fn files(&self) -> Vec<String> {
        let mut paths = std::collections::BTreeSet::new();
        for issues in self.by_rule.values() {
            for issue in issues {
                paths.insert(issue.file.clone());
            }
        }
        paths.into_iter().collect()
    }
}

/// A single lint issue with precise location.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LintIssue {
    pub file: String,
    pub line: usize,
    pub col: Option<usize>,
    pub message: String,
    pub rule: String,
}

/// Structured build/compilation result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BuildResult {
    pub errors: usize,
    pub warnings: usize,
    pub diagnostics: Vec<Diagnostic>,
}

impl BuildResult {
    pub fn summary_line(&self) -> String {
        format!("{} error(s), {} warning(s)", self.errors, self.warnings)
    }

    pub fn files(&self) -> Vec<String> {
        let mut paths = std::collections::BTreeSet::new();
        for diag in &self.diagnostics {
            paths.insert(diag.file.clone());
        }
        paths.into_iter().collect()
    }
}

/// A single compiler diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    pub line: usize,
    pub col: Option<usize>,
    pub severity: DiagnosticSeverity,
    pub code: Option<String>,
    pub message: String,
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    #[default]
    Warning,
    Info,
    Hint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_summary_with_failures() {
        let result = TestResult {
            total: 10,
            passed: 8,
            failed: 2,
            skipped: 0,
            duration_ms: Some(1234),
            failures: vec![],
        };
        assert_eq!(result.summary_line(), "2 failed, 8 passed");
    }

    #[test]
    fn test_result_summary_all_pass() {
        let result = TestResult {
            total: 5,
            passed: 5,
            failed: 0,
            skipped: 0,
            duration_ms: None,
            failures: vec![],
        };
        assert_eq!(result.summary_line(), "5 passed");
    }

    #[test]
    fn test_result_summary_with_skipped() {
        let result = TestResult {
            total: 10,
            passed: 7,
            failed: 1,
            skipped: 2,
            duration_ms: None,
            failures: vec![],
        };
        assert_eq!(result.summary_line(), "1 failed, 7 passed, 2 skipped");
    }

    #[test]
    fn lint_result_summary() {
        let mut by_rule = BTreeMap::new();
        by_rule.insert(
            "E401".to_string(),
            vec![
                LintIssue {
                    file: "src/a.py".to_string(),
                    line: 1,
                    col: None,
                    message: "import".to_string(),
                    rule: "E401".to_string(),
                },
                LintIssue {
                    file: "src/b.py".to_string(),
                    line: 2,
                    col: None,
                    message: "import".to_string(),
                    rule: "E401".to_string(),
                },
            ],
        );
        by_rule.insert(
            "W291".to_string(),
            vec![LintIssue {
                file: "src/a.py".to_string(),
                line: 3,
                col: None,
                message: "whitespace".to_string(),
                rule: "W291".to_string(),
            }],
        );
        let result = LintResult {
            total: 3,
            by_rule,
        };
        assert_eq!(
            result.summary_line(),
            "3 issues across 2 rules; top: E401 (2)"
        );
    }

    #[test]
    fn build_result_files() {
        let result = BuildResult {
            errors: 1,
            warnings: 1,
            diagnostics: vec![
                Diagnostic {
                    file: "src/main.rs".to_string(),
                    line: 10,
                    col: None,
                    severity: DiagnosticSeverity::Error,
                    code: Some("E0425".to_string()),
                    message: "not found".to_string(),
                },
                Diagnostic {
                    file: "src/lib.rs".to_string(),
                    line: 5,
                    col: None,
                    severity: DiagnosticSeverity::Warning,
                    code: None,
                    message: "unused".to_string(),
                },
            ],
        };
        assert_eq!(result.files(), vec!["src/lib.rs", "src/main.rs"]);
    }
}
