#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ImpactResult {
    pub selected_tests: Vec<String>,
    pub smoke_tests: Vec<String>,
    pub missing_mappings: Vec<String>,
    pub stale: bool,
    pub confidence: f64,
    pub escalate_full_suite: bool,
}
