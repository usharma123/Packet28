use super::*;

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
    Slim,
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
    pub response_mode: BrokerResponseMode,
    pub artifact_id: Option<String>,
    pub latest_intention: Option<suite_packet_core::AgentIntention>,
    pub next_action_summary: Option<String>,
    pub handoff_ready: bool,
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
pub struct BrokerPrepareHandoffRequest {
    pub task_id: String,
    pub query: Option<String>,
    pub response_mode: Option<BrokerResponseMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrokerPrepareHandoffResponse {
    pub task_id: String,
    pub handoff_ready: bool,
    pub handoff_reason: String,
    pub latest_checkpoint_id: Option<String>,
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_handoff_generated_at_unix: Option<u64>,
    pub latest_handoff_checkpoint_id: Option<String>,
    pub latest_intention: Option<suite_packet_core::AgentIntention>,
    pub next_action_summary: Option<String>,
    pub context: Option<BrokerGetContextResponse>,
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
    Intention,
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
    pub handoff_ready: bool,
    pub handoff_reason: Option<String>,
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_handoff_generated_at_unix: Option<u64>,
    pub latest_handoff_checkpoint_id: Option<String>,
    pub supports_push: bool,
}
