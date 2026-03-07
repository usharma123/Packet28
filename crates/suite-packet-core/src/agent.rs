use serde::{Deserialize, Serialize};

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
}
