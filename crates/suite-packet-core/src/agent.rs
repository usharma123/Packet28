use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolOperationKind {
    Search,
    Read,
    Edit,
    Build,
    Test,
    Diff,
    Git,
    Fetch,
    #[default]
    Generic,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ToolInvocationSummary {
    pub invocation_id: String,
    pub sequence: u64,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    pub operation_kind: ToolOperationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub occurred_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ToolFailureSummary {
    pub invocation_id: String,
    pub sequence: u64,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    pub operation_kind: ToolOperationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub occurred_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ToolPathSummary {
    pub tool_name: String,
    pub operation_kind: ToolOperationKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct SearchQuerySummary {
    pub tool_name: String,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ToolKindSuccess {
    pub operation_kind: ToolOperationKind,
    pub tool_name: String,
    pub invocation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStateEventKind {
    FocusSet,
    FocusCleared,
    FileRead,
    FileEdited,
    CheckpointSaved,
    DecisionAdded,
    DecisionSuperseded,
    StepCompleted,
    QuestionOpened,
    QuestionResolved,
    ToolInvocationStarted,
    ToolInvocationCompleted,
    ToolInvocationFailed,
    FocusInferred,
    EvidenceCaptured,
}

impl Default for AgentStateEventKind {
    fn default() -> Self {
        Self::FileRead
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentStateEventData {
    FocusSet {
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    FocusCleared {
        #[serde(default)]
        clear_all: bool,
    },
    FileRead {},
    FileEdited {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        regions: Vec<String>,
    },
    CheckpointSaved {
        checkpoint_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    DecisionAdded {
        decision_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        supersedes: Option<String>,
    },
    DecisionSuperseded {
        decision_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        superseded_by: Option<String>,
    },
    StepCompleted {
        step_id: String,
    },
    QuestionOpened {
        question_id: String,
        text: String,
    },
    QuestionResolved {
        question_id: String,
    },
    ToolInvocationStarted {
        invocation_id: String,
        sequence: u64,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        operation_kind: ToolOperationKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_fingerprint: Option<String>,
    },
    ToolInvocationCompleted {
        invocation_id: String,
        sequence: u64,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        operation_kind: ToolOperationKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_fingerprint: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_query: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        artifact_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    ToolInvocationFailed {
        invocation_id: String,
        sequence: u64,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        operation_kind: ToolOperationKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_class: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_fingerprint: Option<String>,
        #[serde(default)]
        retryable: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    FocusInferred {
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    EvidenceCaptured {
        artifact_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

impl Default for AgentStateEventData {
    fn default() -> Self {
        Self::FileRead {}
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AgentStateEventPayload {
    pub task_id: String,
    pub event_id: String,
    pub occurred_at_unix: u64,
    pub actor: String,
    pub kind: AgentStateEventKind,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub data: AgentStateEventData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AgentDecision {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AgentQuestion {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct AgentSnapshotPayload {
    pub task_id: String,
    pub focus_paths: Vec<String>,
    pub focus_symbols: Vec<String>,
    pub files_read: Vec<String>,
    pub files_edited: Vec<String>,
    pub active_decisions: Vec<AgentDecision>,
    pub completed_steps: Vec<String>,
    pub open_questions: Vec<AgentQuestion>,
    pub event_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_at_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_checkpoint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_checkpoint_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths_since_checkpoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_symbols_since_checkpoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tool_invocations: Vec<ToolInvocationSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_failures: Vec<ToolFailureSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_paths_by_tool: Vec<ToolPathSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edited_paths_by_tool: Vec<ToolPathSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_queries: Vec<SearchQuerySummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_artifact_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_successful_tool_by_kind: Vec<ToolKindSuccess>,
}
