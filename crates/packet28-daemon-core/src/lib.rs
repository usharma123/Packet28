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
pub const INDEX_DIR_NAME: &str = ".packet28/index";
pub const INDEX_MANIFEST_FILE_NAME: &str = "manifest.json";
pub const INDEX_SNAPSHOT_FILE_NAME: &str = "semantic-index-v1.bin";
pub const MAX_SOCKET_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrokerAction {
    Plan,
    Inspect,
    ChooseTool,
    Interpret,
    Edit,
    Summarize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrokerToolResultKind {
    Build,
    Stack,
    Test,
    Diff,
    Generic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrokerVerbosity {
    Compact,
    #[default]
    Standard,
    Rich,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrokerResponseMode {
    #[default]
    Full,
    Delta,
    Auto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSourceKind {
    #[serde(rename = "self")]
    SelfAuthored,
    #[default]
    Derived,
    External,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrokerSupersessionMode {
    #[default]
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BrokerSection {
    pub id: String,
    pub title: String,
    pub body: String,
    pub priority: u8,
    pub source_kind: BrokerSourceKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerGetContextRequest {
    pub task_id: String,
    pub action: Option<BrokerAction>,
    pub budget_tokens: Option<u64>,
    pub budget_bytes: Option<usize>,
    pub since_version: Option<String>,
    pub focus_paths: Vec<String>,
    pub focus_symbols: Vec<String>,
    pub tool_name: Option<String>,
    pub tool_result_kind: Option<BrokerToolResultKind>,
    pub query: Option<String>,
    pub include_sections: Vec<String>,
    pub exclude_sections: Vec<String>,
    pub verbosity: Option<BrokerVerbosity>,
    pub response_mode: Option<BrokerResponseMode>,
    pub include_self_context: bool,
    pub max_sections: Option<usize>,
    pub default_max_items_per_section: Option<usize>,
    pub section_item_limits: BTreeMap<String, usize>,
    pub persist_artifacts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerPacketRef {
    pub cache_key: String,
    pub target: String,
    pub score: f64,
    pub summary: Option<String>,
    pub packet_types: Vec<String>,
    pub est_tokens: u64,
    pub est_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerSectionEstimate {
    pub id: String,
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub source_kind: BrokerSourceKind,
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerEvictionCandidate {
    pub section_id: String,
    pub reason: String,
    pub est_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerRecommendedAction {
    pub kind: String,
    pub summary: String,
    pub related_paths: Vec<String>,
    pub related_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerDecision {
    pub id: String,
    pub text: String,
    pub resolves_question_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerQuestion {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerResolvedQuestion {
    pub id: String,
    pub text: String,
    pub resolved_by_decision_id: Option<String>,
    pub resolution_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerDeltaResponse {
    pub changed_sections: Vec<BrokerSection>,
    pub removed_section_ids: Vec<String>,
    pub unchanged_section_ids: Vec<String>,
    pub full_refresh_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerGetContextResponse {
    pub context_version: String,
    pub stale: bool,
    pub brief: String,
    pub supersedes_prior_context: bool,
    pub supersession_mode: BrokerSupersessionMode,
    pub superseded_before_version: String,
    pub sections: Vec<BrokerSection>,
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub budget_remaining_tokens: u64,
    pub budget_remaining_bytes: u64,
    pub section_estimates: Vec<BrokerSectionEstimate>,
    pub eviction_candidates: Vec<BrokerEvictionCandidate>,
    pub delta: BrokerDeltaResponse,
    pub working_set: Vec<BrokerPacketRef>,
    pub recommended_actions: Vec<BrokerRecommendedAction>,
    pub active_decisions: Vec<BrokerDecision>,
    pub open_questions: Vec<BrokerQuestion>,
    pub resolved_questions: Vec<BrokerResolvedQuestion>,
    pub changed_paths_since_checkpoint: Vec<String>,
    pub changed_symbols_since_checkpoint: Vec<String>,
    pub recent_tool_invocations: Vec<suite_packet_core::ToolInvocationSummary>,
    pub tool_failures: Vec<suite_packet_core::ToolFailureSummary>,
    pub discovered_paths: Vec<String>,
    pub discovered_symbols: Vec<String>,
    pub evidence_artifact_ids: Vec<String>,
    pub invalidates_since_version: bool,
    pub effective_max_sections: usize,
    pub effective_default_max_items_per_section: usize,
    pub effective_section_item_limits: BTreeMap<String, usize>,
    pub diagnostics_ms: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BrokerPlanStep {
    pub id: String,
    pub action: String,
    pub description: Option<String>,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BrokerPlanViolation {
    pub step_id: String,
    pub rule: String,
    pub severity: String,
    pub message: String,
    pub related_paths: Vec<String>,
    pub related_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerValidatePlanRequest {
    pub task_id: String,
    pub steps: Vec<BrokerPlanStep>,
    pub budget_tokens: Option<u64>,
    pub require_read_before_edit: Option<bool>,
    pub require_test_gate: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerValidatePlanResponse {
    pub valid: bool,
    pub violations: Vec<BrokerPlanViolation>,
    pub warnings: Vec<BrokerPlanViolation>,
    pub normalized_steps: Vec<BrokerPlanStep>,
    pub est_plan_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrokerDecomposeIntent {
    Rename,
    Extract,
    SplitFile,
    MergeFiles,
    RestructureModule,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct BrokerDecomposedStep {
    pub id: String,
    pub action: String,
    pub description: String,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub depends_on: Vec<String>,
    pub coverage_gap: bool,
    pub est_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerDecomposeRequest {
    pub task_id: String,
    pub task_text: String,
    pub intent: Option<BrokerDecomposeIntent>,
    pub scope_paths: Vec<String>,
    pub scope_symbols: Vec<String>,
    pub max_steps: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerDecomposeResponse {
    pub steps: Vec<BrokerDecomposedStep>,
    pub assumptions: Vec<String>,
    pub unresolved: Vec<String>,
    pub selected_scope_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerEstimateContextRequest {
    pub task_id: String,
    pub action: Option<BrokerAction>,
    pub budget_tokens: Option<u64>,
    pub budget_bytes: Option<usize>,
    pub since_version: Option<String>,
    pub focus_paths: Vec<String>,
    pub focus_symbols: Vec<String>,
    pub tool_name: Option<String>,
    pub tool_result_kind: Option<BrokerToolResultKind>,
    pub query: Option<String>,
    pub include_sections: Vec<String>,
    pub exclude_sections: Vec<String>,
    pub verbosity: Option<BrokerVerbosity>,
    pub response_mode: Option<BrokerResponseMode>,
    pub include_self_context: bool,
    pub max_sections: Option<usize>,
    pub default_max_items_per_section: Option<usize>,
    pub section_item_limits: BTreeMap<String, usize>,
    pub persist_artifacts: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerEstimateContextResponse {
    pub context_version: String,
    pub selected_section_ids: Vec<String>,
    pub est_tokens: u64,
    pub est_bytes: u64,
    pub budget_remaining_tokens: u64,
    pub budget_remaining_bytes: u64,
    pub section_estimates: Vec<BrokerSectionEstimate>,
    pub eviction_candidates: Vec<BrokerEvictionCandidate>,
    pub would_use_delta: bool,
    pub would_include_brief: bool,
    pub effective_max_sections: usize,
    pub effective_default_max_items_per_section: usize,
    pub effective_section_item_limits: BTreeMap<String, usize>,
    pub diagnostics_ms: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerWriteStateBatchRequest {
    pub requests: Vec<BrokerWriteStateRequest>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrokerWriteOp {
    FocusSet,
    FocusClear,
    FileRead,
    FileEdit,
    CheckpointSave,
    DecisionAdd,
    DecisionSupersede,
    StepComplete,
    QuestionOpen,
    QuestionResolve,
    ToolInvocationStarted,
    ToolInvocationCompleted,
    ToolInvocationFailed,
    ToolResult,
    FocusInferred,
    EvidenceCaptured,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerWriteStateRequest {
    pub task_id: String,
    pub op: Option<BrokerWriteOp>,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub note: Option<String>,
    pub decision_id: Option<String>,
    pub question_id: Option<String>,
    pub checkpoint_id: Option<String>,
    pub step_id: Option<String>,
    pub text: Option<String>,
    pub regions: Vec<String>,
    pub resolves_question_id: Option<String>,
    pub resolution_decision_id: Option<String>,
    pub invocation_id: Option<String>,
    pub tool_name: Option<String>,
    pub server_name: Option<String>,
    pub operation_kind: Option<suite_packet_core::ToolOperationKind>,
    pub request_summary: Option<String>,
    pub result_summary: Option<String>,
    pub request_fingerprint: Option<String>,
    pub search_query: Option<String>,
    pub command: Option<String>,
    pub sequence: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error_class: Option<String>,
    pub error_message: Option<String>,
    pub retryable: Option<bool>,
    pub artifact_id: Option<String>,
    pub refresh_context: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerWriteStateResponse {
    pub event_id: String,
    pub context_version: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerWriteStateBatchResponse {
    pub responses: Vec<BrokerWriteStateResponse>,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerTaskStatusRequest {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerTaskStatusResponse {
    pub task: Option<TaskRecord>,
    pub brief_path: Option<String>,
    pub state_path: Option<String>,
    pub event_path: Option<String>,
    pub latest_context_version: Option<String>,
    pub last_refresh_at_unix: Option<u64>,
    pub latest_context_reason: Option<String>,
    pub supports_push: bool,
}

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
    TaskSubscribe {
        task_id: String,
        replay_last: usize,
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
    BrokerGetContext {
        request: BrokerGetContextRequest,
    },
    BrokerEstimateContext {
        request: BrokerEstimateContextRequest,
    },
    BrokerValidatePlan {
        request: BrokerValidatePlanRequest,
    },
    BrokerDecompose {
        request: BrokerDecomposeRequest,
    },
    BrokerWriteState {
        request: BrokerWriteStateRequest,
    },
    BrokerWriteStateBatch {
        request: BrokerWriteStateBatchRequest,
    },
    BrokerTaskStatus {
        request: BrokerTaskStatusRequest,
    },
    DaemonIndexStatus {
        request: DaemonIndexStatusRequest,
    },
    DaemonIndexRebuild {
        request: DaemonIndexRebuildRequest,
    },
    DaemonIndexClear {
        request: DaemonIndexClearRequest,
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
    TaskSubscribeAck {
        task_id: String,
        replayed: usize,
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
    BrokerGetContext {
        response: BrokerGetContextResponse,
    },
    BrokerEstimateContext {
        response: BrokerEstimateContextResponse,
    },
    BrokerValidatePlan {
        response: BrokerValidatePlanResponse,
    },
    BrokerDecompose {
        response: BrokerDecomposeResponse,
    },
    BrokerWriteState {
        response: BrokerWriteStateResponse,
    },
    BrokerWriteStateBatch {
        response: BrokerWriteStateBatchResponse,
    },
    BrokerTaskStatus {
        response: BrokerTaskStatusResponse,
    },
    DaemonIndexStatus {
        response: DaemonIndexStatusResponse,
    },
    DaemonIndexRebuild {
        response: DaemonIndexRebuildResponse,
    },
    DaemonIndexClear {
        response: DaemonIndexClearResponse,
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
    pub ready_at_unix: Option<u64>,
    pub socket_path: String,
    pub workspace_root: String,
    pub log_path: String,
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
    pub last_event_seq: u64,
    pub last_context_refresh_at_unix: Option<u64>,
    pub working_set_est_tokens: u64,
    pub evictable_est_tokens: u64,
    pub changed_since_checkpoint_paths: usize,
    pub changed_since_checkpoint_symbols: usize,
    pub latest_context_version: Option<String>,
    pub latest_brief_path: Option<String>,
    pub latest_brief_hash: Option<String>,
    pub latest_brief_generated_at_unix: Option<u64>,
    pub latest_context_reason: Option<String>,
    pub latest_broker_request: Option<BrokerGetContextRequest>,
    pub linked_decisions: BTreeMap<String, String>,
    pub resolved_questions: BTreeMap<String, String>,
    pub question_texts: BTreeMap<String, String>,
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
    pub ready_at_unix: Option<u64>,
    pub log_path: String,
    pub uptime_secs: u64,
    pub tasks: Vec<TaskRecord>,
    pub watches: Vec<WatchRegistration>,
    pub index: Option<DaemonIndexStatusResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonEvent {
    pub kind: String,
    pub occurred_at_unix: u64,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonEventFrame {
    pub seq: u64,
    pub task_id: String,
    pub event: DaemonEvent,
}

pub fn daemon_dir(root: &Path) -> PathBuf {
    root.join(DAEMON_DIR_NAME)
}

pub fn index_dir(root: &Path) -> PathBuf {
    root.join(INDEX_DIR_NAME)
}

pub fn index_manifest_path(root: &Path) -> PathBuf {
    index_dir(root).join(INDEX_MANIFEST_FILE_NAME)
}

pub fn index_snapshot_path(root: &Path) -> PathBuf {
    index_dir(root).join(INDEX_SNAPSHOT_FILE_NAME)
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

pub fn ready_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(READY_FILE_NAME)
}

pub fn log_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(LOG_FILE_NAME)
}

pub fn watch_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(WATCH_REGISTRY_FILE_NAME)
}

pub fn task_registry_path(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_REGISTRY_FILE_NAME)
}

pub fn task_events_dir(root: &Path) -> PathBuf {
    daemon_dir(root).join(TASK_EVENTS_DIR_NAME)
}

pub fn task_artifacts_dir(root: &Path) -> PathBuf {
    root.join(".packet28").join(TASK_ARTIFACTS_DIR_NAME)
}

pub fn task_event_log_path(root: &Path, task_id: &str) -> PathBuf {
    let safe = safe_task_id(task_id);
    task_events_dir(root).join(format!("{safe}.events.jsonl"))
}

pub fn task_artifact_dir(root: &Path, task_id: &str) -> PathBuf {
    task_artifacts_dir(root).join(safe_task_id(task_id))
}

pub fn task_brief_markdown_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_BRIEF_MARKDOWN_FILE_NAME)
}

pub fn task_brief_json_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_BRIEF_JSON_FILE_NAME)
}

pub fn task_state_json_path(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join(TASK_STATE_JSON_FILE_NAME)
}

pub fn task_versions_dir(root: &Path, task_id: &str) -> PathBuf {
    task_artifact_dir(root, task_id).join("versions")
}

pub fn task_version_json_path(root: &Path, task_id: &str, context_version: &str) -> PathBuf {
    task_versions_dir(root, task_id).join(format!("{}.json", safe_task_id(context_version)))
}

fn safe_task_id(task_id: &str) -> String {
    let safe = task_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "task".to_string()
    } else {
        safe
    }
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
    for path in [
        socket_path(root),
        pid_path(root),
        runtime_path(root),
        ready_path(root),
    ] {
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

pub fn append_task_event(root: &Path, frame: &DaemonEventFrame) -> Result<()> {
    let dir = task_events_dir(root);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create task events dir '{}'", dir.display()))?;
    let path = task_event_log_path(root, &frame.task_id);
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open task event log '{}'", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to append task event log '{}'", path.display()))?;
    Ok(())
}

pub fn load_task_events(root: &Path, task_id: &str) -> Result<Vec<DaemonEventFrame>> {
    let path = task_event_log_path(root, task_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read task event log '{}'", path.display()))?;
    let mut events = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        events.push(serde_json::from_str(line)?);
    }
    Ok(events)
}

pub fn resolve_workspace_root(start: &Path) -> PathBuf {
    let mut dir = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
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
    let len = usize::try_from(u64::from_be_bytes(len_bytes))
        .context("socket frame length does not fit in usize")?;
    if len == 0 {
        anyhow::bail!("socket frame length must be greater than zero");
    }
    if len > MAX_SOCKET_MESSAGE_BYTES {
        anyhow::bail!(
            "socket frame too large: {len} bytes exceeds limit of {MAX_SOCKET_MESSAGE_BYTES}"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn appends_and_loads_task_events() {
        let dir = tempdir().unwrap();
        let frame = DaemonEventFrame {
            seq: 1,
            task_id: "task/demo".to_string(),
            event: DaemonEvent {
                kind: "task_started".to_string(),
                occurred_at_unix: 1,
                data: serde_json::json!({"task_id":"task/demo"}),
            },
        };
        append_task_event(dir.path(), &frame).unwrap();
        append_task_event(
            dir.path(),
            &DaemonEventFrame {
                seq: 2,
                task_id: "task/demo".to_string(),
                event: DaemonEvent {
                    kind: "task_completed".to_string(),
                    occurred_at_unix: 2,
                    data: serde_json::json!({"task_id":"task/demo"}),
                },
            },
        )
        .unwrap();

        let loaded = load_task_events(dir.path(), "task/demo").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].seq, 1);
        assert_eq!(loaded[1].event.kind, "task_completed");
    }
}
