use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TestMapMetadata {
    pub schema_version: u16,
    pub path_norm_version: u16,
    pub repo_root_id: Option<String>,
    pub generated_at: u64,
    pub granularity: String,
}

impl Default for TestMapMetadata {
    /// Creates a default TestMapMetadata populated with a stable baseline for new indexes.
    ///
    /// The default values are:
    /// - `schema_version = 1`
    /// - `path_norm_version = 1`
    /// - `repo_root_id = None`
    /// - `generated_at = 0`
    /// - `granularity = "file"`
    ///
    /// # Examples
    ///
    /// ```
    /// let meta = crate::testmap::TestMapMetadata::default();
    /// assert_eq!(meta.schema_version, 1);
    /// assert_eq!(meta.path_norm_version, 1);
    /// assert_eq!(meta.repo_root_id, None);
    /// assert_eq!(meta.generated_at, 0);
    /// assert_eq!(meta.granularity, "file");
    /// ```
    fn default() -> Self {
        Self {
            schema_version: 1,
            path_norm_version: 1,
            repo_root_id: None,
            generated_at: 0,
            granularity: "file".to_string(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct TestMapIndex {
    pub metadata: TestMapMetadata,
    pub test_language: BTreeMap<String, String>,
    pub test_to_files: BTreeMap<String, BTreeSet<String>>,
    pub file_to_tests: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct TestTimingHistory {
    pub generated_at: u64,
    pub duration_ms: BTreeMap<String, u64>,
    pub sample_count: BTreeMap<String, u32>,
    pub last_seen: BTreeMap<String, u64>,
}