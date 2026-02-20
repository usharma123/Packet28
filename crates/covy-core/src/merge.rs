#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct MergeSummary {
    pub coverage_inputs: usize,
    pub diagnostics_inputs: usize,
    pub skipped_inputs: usize,
    pub coverage_files_merged: usize,
    pub diagnostics_files_merged: usize,
}
