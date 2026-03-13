use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreListRequest {
    pub root: String,
    pub target: Option<String>,
    pub query: Option<String>,
    pub created_after: Option<u64>,
    pub created_before: Option<u64>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreListResponse {
    pub entries: Vec<ContextStoreEntrySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreGetRequest {
    pub root: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreGetResponse {
    pub entry: Option<ContextStoreEntryDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStorePruneDaemonRequest {
    pub root: String,
    pub all: bool,
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStorePruneResponse {
    pub report: ContextStorePruneReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreStatsRequest {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextStoreStatsResponse {
    pub stats: ContextStoreStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextRecallRequest {
    pub query: String,
    pub root: String,
    pub limit: usize,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub target: Option<String>,
    pub task_id: Option<String>,
    pub scope: Option<String>,
    pub packet_types: Vec<String>,
    pub path_filters: Vec<String>,
    pub symbol_filters: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextRecallResponse {
    pub query: String,
    pub hits: Vec<RecallHit>,
}
