use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskAwaitHandoffRequest {
    pub task_id: String,
    pub timeout_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub after_context_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskAwaitHandoffResponse {
    pub task_status: BrokerTaskStatusResponse,
    pub waited_ms: u64,
    pub polls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskLaunchAgentRequest {
    pub task_id: String,
    pub task: Option<String>,
    pub wait_for_handoff: bool,
    pub handoff_timeout_ms: Option<u64>,
    pub handoff_poll_ms: Option<u64>,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TaskLaunchAgentResponse {
    pub task_id: String,
    pub pid: u32,
    pub bootstrap_mode: String,
    pub bootstrap_path: String,
    pub log_path: String,
    pub handoff_artifact_id: Option<String>,
    pub handoff_checkpoint_id: Option<String>,
    pub started_at_unix: u64,
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
    pub latest_handoff_artifact_id: Option<String>,
    pub latest_handoff_generated_at_unix: Option<u64>,
    pub latest_handoff_checkpoint_id: Option<String>,
    pub latest_agent_pid: Option<u32>,
    pub latest_agent_bootstrap_mode: Option<String>,
    pub latest_agent_log_path: Option<String>,
    pub latest_agent_started_at_unix: Option<u64>,
    pub latest_agent_completed_at_unix: Option<u64>,
    pub latest_agent_exit_code: Option<i32>,
    pub latest_agent_context_version: Option<String>,
    pub latest_agent_handoff_artifact_id: Option<String>,
    pub latest_agent_handoff_checkpoint_id: Option<String>,
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
