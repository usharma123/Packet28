pub use crate::coverage::{IssueGateCounts, QualityGateResult};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ImpactResult {
    pub selected_tests: Vec<String>,
    pub smoke_tests: Vec<String>,
    pub missing_mappings: Vec<String>,
    pub stale: bool,
    pub confidence: f64,
    pub escalate_full_suite: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PlannedTest {
    pub id: String,
    pub name: String,
    pub estimated_overlap_lines: u64,
    pub marginal_gain_lines: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UncoveredBlock {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct ImpactPlan {
    pub changed_lines_total: u64,
    pub changed_lines_covered_by_plan: u64,
    pub plan_coverage_pct: f64,
    pub tests: Vec<PlannedTest>,
    pub uncovered_blocks: Vec<UncoveredBlock>,
    pub next_command: String,
}
