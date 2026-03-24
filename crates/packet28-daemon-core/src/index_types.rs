use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexManifest {
    pub schema_version: u32,
    pub root: String,
    pub generation: u64,
    pub include_tests: bool,
    pub status: String,
    pub dirty_paths: Vec<String>,
    pub queued_paths: Vec<String>,
    pub total_files: usize,
    pub indexed_files: usize,
    pub regex_generation: Option<u64>,
    pub regex_status: Option<String>,
    pub regex_base_commit: Option<String>,
    pub regex_weight_table_version: Option<u32>,
    pub regex_stale_reason: Option<String>,
    pub regex_indexed_files: usize,
    pub last_build_started_at_unix: Option<u64>,
    pub last_build_completed_at_unix: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexStatusRequest {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexStatusResponse {
    pub manifest: DaemonIndexManifest,
    pub ready: bool,
    pub fallback_mode: bool,
    pub loaded_generation: Option<u64>,
    pub dirty_file_count: usize,
    pub queued_file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexRebuildRequest {
    pub root: String,
    pub full: bool,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexRebuildResponse {
    pub accepted: bool,
    pub full: bool,
    pub generation: Option<u64>,
    pub queued_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexClearRequest {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonIndexClearResponse {
    pub cleared: bool,
}
