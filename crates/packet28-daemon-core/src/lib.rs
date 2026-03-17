use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use context_kernel_core::{
    KernelRequest, KernelResponse, KernelSequenceRequest, KernelSequenceResponse,
};
use context_memory_core::{
    ContextStoreEntryDetail, ContextStoreEntrySummary, ContextStorePruneReport, ContextStoreStats,
    RecallHit,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod broker_types;
mod context_store_types;
mod daemon_protocol;
mod hook_types;
mod index_types;
pub mod integrity;
mod paths;
mod storage;
mod task_types;
pub mod trust;

pub use broker_types::*;
pub use context_store_types::*;
pub use daemon_protocol::*;
pub use hook_types::*;
pub use index_types::*;
pub use paths::*;
pub use storage::*;
pub use task_types::*;

pub const DAEMON_DIR_NAME: &str = ".packet28/daemon";
pub const SOCKET_FILE_NAME: &str = "packet28d.sock";
pub const PID_FILE_NAME: &str = "pid";
pub const RUNTIME_FILE_NAME: &str = "runtime.json";
pub const READY_FILE_NAME: &str = "ready";
pub const LOG_FILE_NAME: &str = "packet28d.log";
pub const WATCH_REGISTRY_FILE_NAME: &str = "watch-registry-v1.json";
pub const TASK_REGISTRY_FILE_NAME: &str = "task-registry-v1.json";
pub const TASK_EVENTS_DIR_NAME: &str = "tasks";
pub const TASK_ARTIFACTS_DIR_NAME: &str = "task";
pub const TASK_BRIEF_MARKDOWN_FILE_NAME: &str = "brief.md";
pub const TASK_BRIEF_JSON_FILE_NAME: &str = "brief.json";
pub const TASK_STATE_JSON_FILE_NAME: &str = "state.json";
pub const HOOK_RUNTIME_CONFIG_FILE_NAME: &str = "hook-runtime-v1.json";
pub const AGENT_ACTIVE_TASK_FILE_NAME: &str = "active-task.json";
pub const INDEX_DIR_NAME: &str = ".packet28/index";
pub const INDEX_MANIFEST_FILE_NAME: &str = "manifest.json";
pub const INDEX_SNAPSHOT_FILE_NAME: &str = "repo-index-v1.bin";
pub const MAX_SOCKET_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const SOCKET_DIR_NAME: &str = "packet28d-sockets";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    #[serde(alias = "File")]
    File,
    #[serde(alias = "Git")]
    Git,
    #[serde(alias = "TestReport")]
    TestReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WatchSpec {
    pub kind: WatchKind,
    pub task_id: String,
    pub root: String,
    pub paths: Vec<String>,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub debounce_ms: Option<u64>,
}

impl Default for WatchSpec {
    fn default() -> Self {
        Self {
            kind: WatchKind::File,
            task_id: String::new(),
            root: ".".to_string(),
            paths: Vec::new(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            debounce_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskSubmitSpec {
    pub task_id: String,
    pub sequence: KernelSequenceRequest,
    pub watches: Vec<WatchSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceSubmitResponse {
    pub task_id: String,
    pub watch_ids: Vec<String>,
    pub response: KernelSequenceResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CoverCheckRequest {
    pub coverage: Vec<String>,
    pub paths: Vec<String>,
    pub format: String,
    pub issues: Vec<String>,
    pub issues_state: Option<String>,
    pub no_issues_state: bool,
    pub base: Option<String>,
    pub head: Option<String>,
    pub fail_under_total: Option<f64>,
    pub fail_under_changed: Option<f64>,
    pub fail_under_new: Option<f64>,
    pub max_new_errors: Option<u32>,
    pub max_new_warnings: Option<u32>,
    pub input: Option<String>,
    pub strip_prefix: Vec<String>,
    pub source_root: Option<String>,
    pub show_missing: bool,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverCheckResponse {
    pub exit_code: i32,
    pub packet_type: String,
    pub envelope: suite_packet_core::EnvelopeV1<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PacketFetchRequest {
    pub handle: String,
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacketFetchResponse {
    pub wrapper: suite_packet_core::PacketWrapperV1<suite_packet_core::EnvelopeV1<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestShardRequest {
    pub shards: Option<usize>,
    pub tasks_json: Option<String>,
    pub tier: String,
    pub include_tag: Vec<String>,
    pub exclude_tag: Vec<String>,
    pub tests_file: Option<String>,
    pub impact_json: Option<String>,
    pub timings: Option<String>,
    pub unknown_test_seconds: Option<f64>,
    pub algorithm: Option<String>,
    pub write_files: Option<String>,
    pub schema: bool,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestShardResponse {
    pub schema: Option<String>,
    pub plan: Option<suite_packet_core::shard::ShardPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestMapRequest {
    pub manifest: Vec<String>,
    pub output: String,
    pub timings_output: String,
    pub schema: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestMapSummary {
    pub manifest_files: usize,
    pub records: usize,
    pub tests: usize,
    pub files: usize,
    pub output_testmap_path: String,
    pub output_timings_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestMapResponse {
    pub schema: Option<String>,
    pub warnings: Vec<String>,
    pub summary: Option<TestMapSummary>,
}
