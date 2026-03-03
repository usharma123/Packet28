use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Source format of a coverage report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageFormat {
    Lcov,
    Cobertura,
    JaCoCo,
    GoCov,
    LlvmCov,
}

impl std::fmt::Display for CoverageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoverageFormat::Lcov => write!(f, "lcov"),
            CoverageFormat::Cobertura => write!(f, "cobertura"),
            CoverageFormat::JaCoCo => write!(f, "jacoco"),
            CoverageFormat::GoCov => write!(f, "gocov"),
            CoverageFormat::LlvmCov => write!(f, "llvm-cov"),
        }
    }
}

/// Coverage data for a single file.
#[derive(Debug, Clone)]
pub struct FileCoverage {
    /// Lines that were executed at least once.
    pub lines_covered: RoaringBitmap,
    /// Lines that are instrumented (could be executed).
    pub lines_instrumented: RoaringBitmap,
    /// Branch coverage: (line, block) → taken count. Optional.
    pub branches: BTreeMap<(u32, u32), u64>,
    /// Function coverage: name → hit count. Optional.
    pub functions: BTreeMap<String, u64>,
}

impl FileCoverage {
    pub fn new() -> Self {
        Self {
            lines_covered: RoaringBitmap::new(),
            lines_instrumented: RoaringBitmap::new(),
            branches: BTreeMap::new(),
            functions: BTreeMap::new(),
        }
    }

    /// Line coverage percentage (0.0–100.0). Returns None if no instrumented lines.
    pub fn line_coverage_pct(&self) -> Option<f64> {
        let instrumented = self.lines_instrumented.len() as f64;
        if instrumented == 0.0 {
            return None;
        }
        Some((self.lines_covered.len() as f64 / instrumented) * 100.0)
    }

    /// Merge another FileCoverage into this one (OR for bitmaps, sum for counts).
    pub fn merge(&mut self, other: &FileCoverage) {
        self.lines_covered |= &other.lines_covered;
        self.lines_instrumented |= &other.lines_instrumented;
        for (&key, &count) in &other.branches {
            *self.branches.entry(key).or_insert(0) += count;
        }
        for (name, &count) in &other.functions {
            *self.functions.entry(name.clone()).or_insert(0) += count;
        }
    }
}

impl Default for FileCoverage {
    fn default() -> Self {
        Self::new()
    }
}

/// Aggregated coverage data from one or more reports.
#[derive(Debug, Clone)]
pub struct CoverageData {
    /// Per-file coverage, keyed by relative path.
    pub files: BTreeMap<String, FileCoverage>,
    /// Source format(s) that produced this data.
    pub format: Option<CoverageFormat>,
    /// When the coverage was ingested (Unix timestamp).
    pub timestamp: u64,
}

impl CoverageData {
    pub fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            format: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    /// Total line coverage percentage across all files.
    pub fn total_coverage_pct(&self) -> Option<f64> {
        let mut total_covered = 0u64;
        let mut total_instrumented = 0u64;
        for fc in self.files.values() {
            total_covered += fc.lines_covered.len();
            total_instrumented += fc.lines_instrumented.len();
        }
        if total_instrumented == 0 {
            return None;
        }
        Some((total_covered as f64 / total_instrumented as f64) * 100.0)
    }

    /// Merge another CoverageData into this one.
    pub fn merge(&mut self, other: &CoverageData) {
        for (path, fc) in &other.files {
            self.files.entry(path.clone()).or_default().merge(fc);
        }
    }
}

impl Default for CoverageData {
    fn default() -> Self {
        Self::new()
    }
}

/// Status of a file in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// Diff information for a single file.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: DiffStatus,
    /// Lines changed in the new version of the file.
    pub changed_lines: RoaringBitmap,
}

/// Counts of issues on changed lines, used for issue gate reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueGateCounts {
    pub changed_errors: u32,
    pub changed_warnings: u32,
    pub changed_notes: u32,
    pub total_issues: usize,
}

/// Result of a quality gate evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityGateResult {
    pub passed: bool,
    pub total_coverage_pct: Option<f64>,
    pub changed_coverage_pct: Option<f64>,
    pub new_file_coverage_pct: Option<f64>,
    pub violations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_counts: Option<IssueGateCounts>,
}

/// Repository snapshot for cache invalidation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// BLAKE3 Merkle root of all file hashes.
    pub merkle_root: String,
    /// Per-file content hashes.
    pub file_hashes: BTreeMap<String, String>,
}
