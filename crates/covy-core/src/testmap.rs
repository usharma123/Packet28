use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TestMapMetadata {
    pub schema_version: u16,
    pub path_norm_version: u16,
    pub repo_root_id: Option<String>,
    pub generated_at: u64,
    pub granularity: String,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub created_at: Option<u64>,
    #[serde(default)]
    pub toolchain_fingerprint: Option<String>,
}

impl Default for TestMapMetadata {
    fn default() -> Self {
        Self {
            schema_version: 2,
            path_norm_version: 1,
            repo_root_id: None,
            generated_at: 0,
            granularity: "file".to_string(),
            commit_sha: None,
            created_at: None,
            toolchain_fingerprint: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct TestMapIndex {
    pub metadata: TestMapMetadata,
    pub test_language: BTreeMap<String, String>,
    /// Legacy index used by pre-v2 impact planners (file-level only).
    pub test_to_files: BTreeMap<String, BTreeSet<String>>,
    /// Legacy inverse index used by pre-v2 impact planners (file-level only).
    pub file_to_tests: BTreeMap<String, BTreeSet<String>>,
    /// V2 canonical test id list (index -> test id).
    #[serde(default)]
    pub tests: Vec<String>,
    /// V2 canonical file key list (index -> repo-relative path).
    #[serde(default)]
    pub file_index: Vec<String>,
    /// V2 coverage matrix: test_idx -> file_idx -> changed line numbers.
    #[serde(default)]
    pub coverage: Vec<Vec<Vec<u32>>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct TestTimingHistory {
    pub generated_at: u64,
    pub duration_ms: BTreeMap<String, u64>,
    pub sample_count: BTreeMap<String, u32>,
    pub last_seen: BTreeMap<String, u64>,
}
