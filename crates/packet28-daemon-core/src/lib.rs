use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use context_memory_core::{
    ContextStoreEntryDetail, ContextStoreEntrySummary, ContextStorePruneReport, ContextStoreStats,
    RecallHit,
};
use context_kernel_core::{KernelRequest, KernelResponse, KernelSequenceRequest, KernelSequenceResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DAEMON_DIR_NAME: &str = ".packet28/daemon";
pub const SOCKET_FILE_NAME: &str = "packet28d.sock";
pub const PID_FILE_NAME: &str = "pid";
pub const RUNTIME_FILE_NAME: &str = "runtime.json";
pub const WATCH_REGISTRY_FILE_NAME: &str = "watch-registry-v1.json";
pub const TASK_REGISTRY_FILE_NAME: &str = "task-registry-v1.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    File,
    Git,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextRecallResponse {
    pub query: String,
    pub hits: Vec<RecallHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    Execute {
        request: KernelRequest,
    },
    ExecuteSequence {
        spec: TaskSubmitSpec,
    },
    Status,
    Stop,
    TaskStatus {
        task_id: String,
    },
    TaskCancel {
        task_id: String,
    },
    WatchList {
        task_id: Option<String>,
    },
    WatchRemove {
        watch_id: String,
    },
    CoverCheck {
        request: CoverCheckRequest,
    },
    PacketFetch {
        request: PacketFetchRequest,
    },
    TestShard {
        request: TestShardRequest,
    },
    TestMap {
        request: TestMapRequest,
    },
    ContextStoreList {
        request: ContextStoreListRequest,
    },
    ContextStoreGet {
        request: ContextStoreGetRequest,
    },
    ContextStorePrune {
        request: ContextStorePruneDaemonRequest,
    },
    ContextStoreStats {
        request: ContextStoreStatsRequest,
    },
    ContextRecall {
        request: ContextRecallRequest,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonResponse {
    Execute {
        response: KernelResponse,
    },
    ExecuteSequence {
        response: KernelSequenceResponse,
        task: TaskRecord,
        watches: Vec<WatchRegistration>,
    },
    Status {
        status: DaemonStatus,
    },
    Ack {
        message: String,
    },
    TaskStatus {
        task: Option<TaskRecord>,
    },
    TaskCancel {
        task: Option<TaskRecord>,
        removed_watch_ids: Vec<String>,
    },
    WatchList {
        watches: Vec<WatchRegistration>,
    },
    WatchRemove {
        removed: Option<WatchRegistration>,
    },
    CoverCheck {
        response: CoverCheckResponse,
    },
    PacketFetch {
        response: PacketFetchResponse,
    },
    TestShard {
        response: TestShardResponse,
    },
    TestMap {
        response: TestMapResponse,
    },
    ContextStoreList {
        response: ContextStoreListResponse,
    },
    ContextStoreGet {
        response: ContextStoreGetResponse,
    },
    ContextStorePrune {
        response: ContextStorePruneResponse,
    },
    ContextStoreStats {
        response: ContextStoreStatsResponse,
    },
    ContextRecall {
        response: ContextRecallResponse,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonRuntimeInfo {
    pub pid: u32,
    pub started_at_unix: u64,
    pub socket_path: String,
    pub workspace_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskRecord {
    pub task_id: String,
    pub running: bool,
    pub cancel_requested: bool,
    pub pending_replan: bool,
    pub last_request_id: Option<u64>,
    pub last_started_at_unix: Option<u64>,
    pub last_completed_at_unix: Option<u64>,
    pub last_replan_at_unix: Option<u64>,
    pub last_error: Option<String>,
    pub watch_ids: Vec<String>,
    pub sequence_present: bool,
    pub sequence: Option<KernelSequenceRequest>,
    pub last_sequence_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchRegistration {
    pub watch_id: String,
    pub spec: WatchSpec,
    pub active: bool,
    pub last_event_at_unix: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WatchRegistry {
    pub watches: Vec<WatchRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskRegistry {
    pub tasks: BTreeMap<String, TaskRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonStatus {
    pub pid: u32,
    pub socket_path: String,
    pub workspace_root: String,
    pub started_at_unix: u64,
    pub uptime_secs: u64,
    pub tasks: Vec<TaskRecord>,
    pub watches: Vec<WatchRegistration>,
}

pub fn daemon_dir(root: &Path) -> PathBuf {
    root.join(DAEMON_DIR_NAME)
}

pub fn socket_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(SOCKET_FILE_NAME)
}

pub fn pid_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(PID_FILE_NAME)
}

pub fn runtime_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(RUNTIME_FILE_NAME)
}

pub fn watch_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(WATCH_REGISTRY_FILE_NAME)
}

pub fn task_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_REGISTRY_FILE_NAME)
}

pub fn ensure_daemon_dir(root: &Path) -> Result<PathBuf> {
    let dir = daemon_dir(root);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon directory '{}'", dir.display()))?;
    Ok(dir)
}

pub fn write_runtime_info(root: &Path, info: &DaemonRuntimeInfo) -> Result<()> {
    ensure_daemon_dir(root)?;
    fs::write(pid_path(root), format!("{}\n", info.pid))
        .with_context(|| format!("failed to write pid file for '{}'", root.display()))?;
    fs::write(runtime_path(root), serde_json::to_vec_pretty(info)?)
        .with_context(|| format!("failed to write runtime file for '{}'", root.display()))?;
    Ok(())
}

pub fn read_runtime_info(root: &Path) -> Result<DaemonRuntimeInfo> {
    let raw = fs::read(runtime_path(root))
        .with_context(|| format!("failed to read runtime file for '{}'", root.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn remove_runtime_files(root: &Path) -> Result<()> {
    for path in [socket_path(root), pid_path(root), runtime_path(root)] {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove '{}'", path.display()))?;
        }
    }
    Ok(())
}

pub fn load_watch_registry(root: &Path) -> Result<WatchRegistry> {
    let path = watch_registry_path(root);
    if !path.exists() {
        return Ok(WatchRegistry::default());
    }
    let raw = fs::read(&path)
        .with_context(|| format!("failed to read watch registry '{}'", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn save_watch_registry(root: &Path, registry: &WatchRegistry) -> Result<()> {
    ensure_daemon_dir(root)?;
    let path = watch_registry_path(root);
    fs::write(&path, serde_json::to_vec_pretty(registry)?)
        .with_context(|| format!("failed to write watch registry '{}'", path.display()))?;
    Ok(())
}

pub fn load_task_registry(root: &Path) -> Result<TaskRegistry> {
    let path = task_registry_path(root);
    if !path.exists() {
        return Ok(TaskRegistry::default());
    }
    let raw = fs::read(&path)
        .with_context(|| format!("failed to read task registry '{}'", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

pub fn save_task_registry(root: &Path, registry: &TaskRegistry) -> Result<()> {
    ensure_daemon_dir(root)?;
    let path = task_registry_path(root);
    fs::write(&path, serde_json::to_vec_pretty(registry)?)
        .with_context(|| format!("failed to write task registry '{}'", path.display()))?;
    Ok(())
}

pub fn resolve_workspace_root(start: &Path) -> PathBuf {
    let mut dir = start
        .canonicalize()
        .unwrap_or_else(|_| start.to_path_buf());
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return start.to_path_buf();
        }
    }
}

pub fn write_socket_message<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let len = bytes.len() as u64;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_socket_message<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T> {
    let mut len_bytes = [0_u8; 8];
    reader.read_exact(&mut len_bytes)?;
    let len = u64::from_be_bytes(len_bytes) as usize;
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
